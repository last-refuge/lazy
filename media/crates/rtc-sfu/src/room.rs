//! The media thread: every participant's str0m connection, the one UDP socket, and forwarding.
//!
//! Adapted from str0m's `chat` example. Each participant's own tracks are "tracks in"; each
//! other participant's track they receive is a "track out", negotiated by an offer from the SFU
//! (over the signaling socket) and answered by the client. The SFU never offers while an offer
//! is outstanding, and clients never offer after joining, so offers never cross.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use rtc_proto::webhook::{ReportedParticipant, RoomReport};
use rtc_proto::{
    Claims, ErrorCode, Participant, ParticipantId, ServerMessage, TrackInfo, TrackKind,
};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::sync::mpsc::UnboundedSender;

/// What the signaling side asks of the media thread.
#[derive(Debug)]
pub enum Command {
    Join {
        id: ParticipantId,
        claims: Claims,
        offer: String,
        audio_muted: bool,
        video_on: bool,
        out: UnboundedSender<ServerMessage>,
    },
    Answer {
        id: ParticipantId,
        sdp: String,
    },
    State {
        id: ParticipantId,
        audio_muted: bool,
        video_on: bool,
    },
    Leave {
        id: ParticipantId,
    },
}

/// The longest the thread waits for a packet before looking at commands again.
const MAX_WAIT: Duration = Duration::from_millis(10);

/// A connection that never completes is dropped after this.
const CONNECT_WITHIN: Duration = Duration::from_secs(30);

/// Where room reports go (to be sent to the host), and how often every room is reported again.
pub(crate) struct Reports {
    pub sender: UnboundedSender<RoomReport>,
    pub every: Duration,
}

impl Reports {
    /// Reports who is in `room` now (nobody, if it just emptied).
    fn room(&self, room: &str, clients: &[Client]) {
        let participants = clients
            .iter()
            .filter(|c| c.room == room)
            .map(|c| ReportedParticipant {
                identity: c.participant.identity.clone(),
                name: c.participant.name.clone(),
            })
            .collect();
        self.sender
            .send(RoomReport {
                room: room.to_owned(),
                participants,
                sent_at_ms: now_ms(),
            })
            .ok();
    }

    /// Reports every room with someone in it.
    fn all(&self, clients: &[Client]) {
        let mut rooms: Vec<&str> = clients.iter().map(|c| c.room.as_str()).collect();
        rooms.sort_unstable();
        rooms.dedup();
        for room in rooms {
            self.room(room, clients);
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub(crate) fn run(
    socket: UdpSocket,
    media_addr: SocketAddr,
    commands: Receiver<Command>,
    max_per_room: usize,
    reports: Option<Reports>,
) {
    let mut clients: Vec<Client> = Vec::new();
    let mut queue: VecDeque<Propagated> = VecDeque::new();
    let mut buf = vec![0u8; 2000];
    let mut reported_at = Instant::now();
    loop {
        // Commands from the signaling side.
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    let joined = handle_command(command, &mut clients, media_addr, max_per_room);
                    if let (Some(room), Some(reports)) = (joined, &reports) {
                        reports.room(&room, &clients);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Dead connections go, and everyone else in their room hears so.
        let mut gone = Vec::new();
        clients.retain(|c| {
            let connecting_too_long = !c.connected && c.joined_at.elapsed() > CONNECT_WITHIN;
            if c.rtc.is_alive() && !c.left && !connecting_too_long {
                true
            } else {
                if !c.left {
                    c.out
                        .send(ServerMessage::Error {
                            code: ErrorCode::Internal,
                            message: "the media connection was lost".into(),
                        })
                        .ok();
                }
                gone.push((c.room.clone(), c.participant.id));
                false
            }
        });
        let mut emptied: Vec<String> = Vec::new();
        for (room, id) in gone {
            tracing::info!(%id, room, "left");
            for c in clients.iter_mut().filter(|c| c.room == room) {
                c.out.send(ServerMessage::ParticipantLeft { id }).ok();
            }
            if !emptied.contains(&room) {
                emptied.push(room);
            }
        }
        if let Some(reports) = &reports {
            for room in &emptied {
                reports.room(room, &clients);
            }
            if reported_at.elapsed() >= reports.every {
                reported_at = Instant::now();
                reports.all(&clients);
            }
        }

        queue.extend(wanted_keyframes(&mut clients));

        let mut timeout = Instant::now() + MAX_WAIT;
        for client in &mut clients {
            timeout = timeout.min(client.poll_until_timeout(&mut queue, &socket));
        }

        if let Some(propagated) = queue.pop_front() {
            propagate(&propagated, &mut clients);
            continue;
        }

        let wait = timeout
            .saturating_duration_since(Instant::now())
            .clamp(Duration::from_millis(1), MAX_WAIT);
        socket.set_read_timeout(Some(wait)).ok();
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                if let Ok(contents) = buf[..n].try_into() {
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: media_addr,
                            contents,
                        },
                    );
                    if let Some(client) = clients.iter_mut().find(|c| c.rtc.accepts(&input)) {
                        client.handle_input(input);
                    }
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => tracing::warn!("UDP receive failed: {e}"),
        }

        let now = Instant::now();
        for client in &mut clients {
            client.handle_input(Input::Timeout(now));
        }
    }
}

/// Handles one command; returns the room someone joined, if someone did.
fn handle_command(
    command: Command,
    clients: &mut Vec<Client>,
    media_addr: SocketAddr,
    max_per_room: usize,
) -> Option<String> {
    match command {
        Command::Join {
            id,
            claims,
            offer,
            audio_muted,
            video_on,
            out,
        } => {
            let in_room = clients.iter().filter(|c| c.room == claims.room).count();
            if in_room >= max_per_room {
                out.send(ServerMessage::Error {
                    code: ErrorCode::RoomFull,
                    message: format!("the room is full ({max_per_room} people)"),
                })
                .ok();
                return None;
            }
            let participant = Participant {
                id,
                identity: claims.identity,
                name: claims.name,
                audio_muted,
                video_on,
            };
            match Client::join(participant.clone(), claims.room, &offer, media_addr, out) {
                Ok(mut client) => {
                    tracing::info!(%id, room = client.room, "joined");
                    let others: Vec<Participant> = clients
                        .iter()
                        .filter(|c| c.room == client.room)
                        .map(|c| c.participant.clone())
                        .collect();
                    client
                        .out
                        .send(ServerMessage::Joined {
                            you: id,
                            answer: client.answer.take().unwrap_or_default(),
                            participants: others,
                        })
                        .ok();
                    let room = client.room.clone();
                    for other in clients.iter_mut().filter(|c| c.room == room) {
                        other
                            .out
                            .send(ServerMessage::ParticipantJoined {
                                participant: participant.clone(),
                            })
                            .ok();
                        // The newcomer receives everything already being sent.
                        for track in &other.tracks_in {
                            client.open_track(Arc::downgrade(&track.track));
                        }
                    }
                    clients.push(client);
                    return Some(room);
                }
                Err(message) => {
                    tracing::info!(%id, "join failed: {message}");
                    // `out` was moved into the failed join; the socket closes when it drops.
                }
            }
        }
        Command::Answer { id, sdp } => {
            if let Some(client) = clients.iter_mut().find(|c| c.participant.id == id) {
                client.handle_answer(&sdp);
            }
        }
        Command::State {
            id,
            audio_muted,
            video_on,
        } => {
            let client = clients.iter_mut().find(|c| c.participant.id == id)?;
            client.participant.audio_muted = audio_muted;
            client.participant.video_on = video_on;
            let participant = client.participant.clone();
            let room = client.room.clone();
            for c in clients
                .iter_mut()
                .filter(|c| c.room == room && c.participant.id != id)
            {
                c.out
                    .send(ServerMessage::ParticipantChanged {
                        participant: participant.clone(),
                    })
                    .ok();
            }
        }
        Command::Leave { id } => {
            if let Some(client) = clients.iter_mut().find(|c| c.participant.id == id) {
                client.left = true;
                client.rtc.disconnect();
            }
        }
    }
    None
}

/// Hands one client's output to the others in its room.
fn propagate(propagated: &Propagated, clients: &mut [Client]) {
    let Some(origin) = propagated.origin() else {
        return;
    };
    let Some(room) = clients
        .iter()
        .find(|c| c.participant.id == origin)
        .map(|c| c.room.clone())
    else {
        return;
    };
    for client in clients.iter_mut().filter(|c| c.room == room) {
        match propagated {
            Propagated::TrackOpen(_, track) if client.participant.id != origin => {
                client.open_track(track.clone());
            }
            Propagated::MediaData(_, data) if client.participant.id != origin => {
                client.forward(origin, data);
            }
            Propagated::KeyframeRequest(_, request, source, mid_in)
                if client.participant.id == *source =>
            {
                client.request_keyframe(*mid_in, request.kind);
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
struct TrackIn {
    origin: ParticipantId,
    mid: Mid,
    kind: MediaKind,
}

struct TrackInEntry {
    track: Arc<TrackIn>,
    last_keyframe_request: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackOutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
    ToStop(Mid),
    NegotiatingStop(Mid),
}

struct TrackOut {
    track: Weak<TrackIn>,
    state: TrackOutState,
}

impl TrackOut {
    fn mid(&self) -> Option<Mid> {
        match self.state {
            TrackOutState::ToOpen => None,
            TrackOutState::Negotiating(m)
            | TrackOutState::Open(m)
            | TrackOutState::ToStop(m)
            | TrackOutState::NegotiatingStop(m) => Some(m),
        }
    }
}

#[allow(clippy::large_enum_variant)] // Short-lived, in one queue, like str0m's example.
enum Propagated {
    Noop,
    Timeout(Instant),
    TrackOpen(ParticipantId, Weak<TrackIn>),
    MediaData(ParticipantId, MediaData),
    /// From the participant who wants it, to the participant whose track it is, for their `Mid`.
    KeyframeRequest(ParticipantId, KeyframeRequest, ParticipantId, Mid),
}

impl Propagated {
    fn origin(&self) -> Option<ParticipantId> {
        match self {
            Propagated::TrackOpen(c, _)
            | Propagated::MediaData(c, _)
            | Propagated::KeyframeRequest(c, ..) => Some(*c),
            Propagated::Noop | Propagated::Timeout(_) => None,
        }
    }
}

struct Client {
    participant: Participant,
    room: String,
    rtc: Rtc,
    out: UnboundedSender<ServerMessage>,
    /// The answer to the join's offer, until it is sent.
    answer: Option<String>,
    pending: Option<SdpPendingOffer>,
    tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    /// Keyframe requests for newly opened video, sent once the open is answered.
    keyframes_wanted: Vec<(ParticipantId, Mid)>,
    joined_at: Instant,
    connected: bool,
    left: bool,
}

/// The SFU's str0m configuration: ICE-lite, and the codecs clients use.
fn rtc() -> Rtc {
    Rtc::builder()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_opus(true, false)
        .enable_h264(true)
        .build(Instant::now())
}

impl Client {
    fn join(
        participant: Participant,
        room: String,
        offer: &str,
        media_addr: SocketAddr,
        out: UnboundedSender<ServerMessage>,
    ) -> Result<Client, String> {
        let refuse = |out: &UnboundedSender<ServerMessage>, message: String| {
            out.send(ServerMessage::Error {
                code: ErrorCode::BadRequest,
                message: message.clone(),
            })
            .ok();
            message
        };
        let offer = match SdpOffer::from_sdp_string(offer) {
            Ok(offer) => offer,
            Err(e) => return Err(refuse(&out, format!("the offer is not valid SDP: {e}"))),
        };
        let mut rtc = rtc();
        let candidate = Candidate::host(media_addr, "udp").map_err(|e| e.to_string())?;
        rtc.add_local_candidate(candidate);
        let answer = match rtc.sdp_api().accept_offer(offer) {
            Ok(answer) => answer,
            Err(e) => return Err(refuse(&out, format!("the offer was refused: {e}"))),
        };
        Ok(Client {
            participant,
            room,
            rtc,
            out,
            answer: Some(answer.to_sdp_string()),
            pending: None,
            tracks_in: Vec::new(),
            tracks_out: Vec::new(),
            keyframes_wanted: Vec::new(),
            joined_at: Instant::now(),
            connected: false,
            left: false,
        })
    }

    fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }
        if let Err(e) = self.rtc.handle_input(input) {
            tracing::info!(id = %self.participant.id, "disconnected: {e}");
            self.rtc.disconnect();
        }
    }

    fn poll_until_timeout(
        &mut self,
        queue: &mut VecDeque<Propagated>,
        socket: &UdpSocket,
    ) -> Instant {
        loop {
            if !self.rtc.is_alive() {
                return Instant::now();
            }
            match self.poll_output(socket) {
                Propagated::Timeout(t) => return t,
                Propagated::Noop => {}
                other => queue.push_back(other),
            }
        }
    }

    fn poll_output(&mut self, socket: &UdpSocket) -> Propagated {
        self.negotiate_if_needed();
        let output = match self.rtc.poll_output() {
            Ok(output) => output,
            Err(e) => {
                tracing::info!(id = %self.participant.id, "poll failed: {e}");
                self.rtc.disconnect();
                return Propagated::Noop;
            }
        };
        match output {
            Output::Transmit(transmit) => {
                socket
                    .send_to(&transmit.contents, transmit.destination)
                    .ok();
                Propagated::Noop
            }
            Output::Timeout(t) => Propagated::Timeout(t),
            Output::Event(event) => match event {
                Event::IceConnectionStateChange(state) => {
                    match state {
                        IceConnectionState::Disconnected => self.rtc.disconnect(),
                        IceConnectionState::Connected | IceConnectionState::Completed => {
                            self.connected = true;
                        }
                        _ => {}
                    }
                    Propagated::Noop
                }
                Event::MediaAdded(added) if added.direction == Direction::RecvOnly => {
                    let track = Arc::new(TrackIn {
                        origin: self.participant.id,
                        mid: added.mid,
                        kind: added.kind,
                    });
                    let weak = Arc::downgrade(&track);
                    self.tracks_in.push(TrackInEntry {
                        track,
                        last_keyframe_request: None,
                    });
                    Propagated::TrackOpen(self.participant.id, weak)
                }
                Event::MediaData(data) => {
                    if !data.contiguous {
                        self.request_keyframe(data.mid, KeyframeRequestKind::Pli);
                    }
                    Propagated::MediaData(self.participant.id, data)
                }
                Event::KeyframeRequest(request) => self.incoming_keyframe_request(request),
                _ => Propagated::Noop,
            },
        }
    }

    /// Someone receiving one of this client's track-outs wants a keyframe: pass it to the
    /// track's origin.
    fn incoming_keyframe_request(&self, request: KeyframeRequest) -> Propagated {
        let Some(track) = self
            .tracks_out
            .iter()
            .find(|t| t.mid() == Some(request.mid))
            .and_then(|t| t.track.upgrade())
        else {
            return Propagated::Noop;
        };
        Propagated::KeyframeRequest(self.participant.id, request, track.origin, track.mid)
    }

    /// Asks this client for a keyframe on its own track `mid`, at most once a second.
    fn request_keyframe(&mut self, mid: Mid, kind: KeyframeRequestKind) {
        let Some(entry) = self.tracks_in.iter_mut().find(|t| t.track.mid == mid) else {
            return;
        };
        if entry.track.kind != MediaKind::Video {
            return;
        }
        if entry
            .last_keyframe_request
            .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
        {
            return;
        }
        if let Some(mut writer) = self.rtc.writer(mid)
            && writer.request_keyframe(None, kind).is_ok()
        {
            entry.last_keyframe_request = Some(Instant::now());
        }
    }

    fn open_track(&mut self, track: Weak<TrackIn>) {
        self.tracks_out.push(TrackOut {
            track,
            state: TrackOutState::ToOpen,
        });
    }

    fn negotiate_if_needed(&mut self) {
        if self.pending.is_some() || self.answer.is_some() {
            return;
        }
        for track in &mut self.tracks_out {
            if let TrackOutState::Open(m) = track.state
                && track.track.upgrade().is_none()
            {
                track.state = TrackOutState::ToStop(m);
            }
        }
        // Tracks whose sender left before they were negotiated are simply forgotten.
        self.tracks_out
            .retain(|t| t.state != TrackOutState::ToOpen || t.track.upgrade().is_some());

        let mut change = self.rtc.sdp_api();
        for track in &mut self.tracks_out {
            match track.state {
                TrackOutState::ToOpen => {
                    if let Some(source) = track.track.upgrade() {
                        let mid = change.add_media(
                            source.kind,
                            Direction::SendOnly,
                            Some(source.origin.to_string()),
                            None,
                            None,
                        );
                        track.state = TrackOutState::Negotiating(mid);
                    }
                }
                TrackOutState::ToStop(mid) => {
                    change.stop_media(mid);
                    track.state = TrackOutState::NegotiatingStop(mid);
                }
                _ => {}
            }
        }
        if !change.has_changes() {
            return;
        }
        let Some((offer, pending)) = change.apply() else {
            return;
        };
        let tracks = self
            .tracks_out
            .iter()
            .filter(|t| {
                matches!(
                    t.state,
                    TrackOutState::Negotiating(_) | TrackOutState::Open(_)
                )
            })
            .filter_map(|t| {
                let source = t.track.upgrade()?;
                Some(TrackInfo {
                    mid: t.mid()?.to_string(),
                    participant: source.origin,
                    kind: match source.kind {
                        MediaKind::Audio => TrackKind::Audio,
                        MediaKind::Video => TrackKind::Video,
                    },
                })
            })
            .collect();
        self.out
            .send(ServerMessage::Offer {
                sdp: offer.to_sdp_string(),
                tracks,
            })
            .ok();
        self.pending = Some(pending);
    }

    fn handle_answer(&mut self, sdp: &str) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let answer = match SdpAnswer::from_sdp_string(sdp) {
            Ok(answer) => answer,
            Err(e) => {
                tracing::info!(id = %self.participant.id, "bad answer: {e}");
                self.rtc.disconnect();
                return;
            }
        };
        if let Err(e) = self.rtc.sdp_api().accept_answer(pending, answer) {
            tracing::info!(id = %self.participant.id, "answer refused: {e}");
            self.rtc.disconnect();
            return;
        }
        for track in &mut self.tracks_out {
            if let TrackOutState::Negotiating(m) = track.state {
                track.state = TrackOutState::Open(m);
                if let Some(source) = track.track.upgrade()
                    && source.kind == MediaKind::Video
                {
                    // A new viewer needs a keyframe to start from.
                    self.keyframes_wanted.push((source.origin, source.mid));
                }
            }
        }
        self.tracks_out
            .retain(|t| !matches!(t.state, TrackOutState::NegotiatingStop(_)));
    }

    /// Sends `data` from `origin` on the matching track-out.
    fn forward(&mut self, origin: ParticipantId, data: &MediaData) {
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                o.track
                    .upgrade()
                    .is_some_and(|i| i.origin == origin && i.mid == data.mid)
            })
            .and_then(|o| match o.state {
                TrackOutState::Open(m) => Some(m),
                _ => None,
            })
        else {
            return;
        };
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = writer.match_params(data.params) else {
            return;
        };
        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            tracing::info!(id = %self.participant.id, "forward failed: {e}");
            self.rtc.disconnect();
        }
    }
}

/// Keyframe requests for viewers whose video just opened, as propagations from the viewer.
fn wanted_keyframes(clients: &mut [Client]) -> Vec<Propagated> {
    let mut out = Vec::new();
    for client in clients.iter_mut() {
        for (origin, mid) in client.keyframes_wanted.drain(..) {
            out.push(Propagated::KeyframeRequest(
                client.participant.id,
                KeyframeRequest {
                    mid,
                    rid: None,
                    kind: KeyframeRequestKind::Pli,
                },
                origin,
                mid,
            ));
        }
    }
    out
}
