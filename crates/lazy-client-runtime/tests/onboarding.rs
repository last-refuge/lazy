//! What the runtime asks a server before anyone has signed in (D29): how it takes accounts, heard
//! just before the check's own answer, and what an invitation code is for, against an in-process
//! stub of the two routes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lazy_client_core::{Effect, Input, PreviewFailure, ServerFacts};
use lazy_client_runtime::async_channel::Receiver;
use lazy_client_runtime::{
    BackoffPolicy, MemoryStorage, Runtime, RuntimeConfig, SecureStorageChoice,
};
use lazy_domain::DisplayName;
use lazy_protocol::{
    ApiError, ErrorCode, HealthResponse, InvitationKind, InvitationPreview, InviteCode,
    PROTOCOL_VERSION, PreviewInvitationRequest, ServerInfoResponse, ServerRegistration,
};
use tempfile::TempDir;

const VALID: &str = "K7QM4TXD9BWPH2VAZX3C";
const CLOSED: &str = "CLOSED00000000000000";
const OLD_SERVER: &str = "OLDSERVER00000000000";
const BROKEN: &str = "BROKEN000000000000000";

// ----- Stub server -----

#[derive(Clone, Copy)]
enum Info {
    Says(ServerRegistration, bool),
    NotFound,
    Garbage,
    OtherProtocol,
}

struct Stub {
    info: Mutex<Info>,
    codes_seen: Mutex<Vec<String>>,
}

fn refusal(code: ErrorCode) -> Response {
    let status = StatusCode::from_u16(code.http_status()).unwrap();
    (status, Json(ApiError::new(code, "refused"))).into_response()
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        protocol_version: PROTOCOL_VERSION,
    })
}

async fn info(State(stub): State<Arc<Stub>>) -> Response {
    let mode = *stub.info.lock().unwrap();
    match mode {
        Info::Says(registration, setup_code_required) => Json(ServerInfoResponse {
            protocol_version: PROTOCOL_VERSION,
            registration,
            setup_code_required,
        })
        .into_response(),
        Info::NotFound => refusal(ErrorCode::NotFound),
        Info::Garbage => "<html>nothing to see</html>".into_response(),
        Info::OtherProtocol => Json(ServerInfoResponse {
            protocol_version: PROTOCOL_VERSION + 1,
            registration: ServerRegistration::Open,
            setup_code_required: false,
        })
        .into_response(),
    }
}

async fn preview(
    State(stub): State<Arc<Stub>>,
    Json(request): Json<PreviewInvitationRequest>,
) -> Response {
    let code = request.code.expose().to_owned();
    stub.codes_seen.lock().unwrap().push(code.clone());
    match code.as_str() {
        VALID => Json(InvitationPreview {
            kind: InvitationKind::Invitation,
            workspace_name: "Zebrafish Collective".into(),
            inviter: Some(DisplayName::parse("Alice Example").unwrap()),
            member_count: 12,
        })
        .into_response(),
        CLOSED => refusal(ErrorCode::Forbidden),
        OLD_SERVER => refusal(ErrorCode::NotFound),
        BROKEN => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => refusal(ErrorCode::InvitationInvalid),
    }
}

struct Server {
    _rt: tokio::runtime::Runtime,
    stub: Arc<Stub>,
    url: String,
}

impl Server {
    fn start(info_mode: Info) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let stub = Arc::new(Stub {
            info: Mutex::new(info_mode),
            codes_seen: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/healthz", get(health))
            .route("/v1/server", get(info))
            .route("/v1/invitations/preview", post(preview))
            .with_state(stub.clone());
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { _rt: rt, stub, url }
    }
}

// ----- Client -----

struct Client {
    runtime: Option<Runtime>,
    inputs: Receiver<Input>,
    _profile: TempDir,
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future)
}

impl Client {
    fn start(url: &str) -> Self {
        let profile = tempfile::tempdir().unwrap();
        let (runtime, inputs) = Runtime::start(RuntimeConfig {
            server_locked: true,
            server_url: url.to_owned(),
            profile_dir: Some(profile.path().to_owned()),
            secure_storage: SecureStorageChoice::Custom(MemoryStorage::new()),
            request_timeout: Duration::from_secs(3),
            read_attempts: 3,
            reconnect: BackoffPolicy {
                base: Duration::from_millis(10),
                cap: Duration::from_millis(40),
            },
            idle_timeout: Duration::from_secs(5),
            save_debounce: Duration::from_millis(20),
        })
        .expect("runtime starts");
        let mut client = Self {
            runtime: Some(runtime),
            inputs,
            _profile: profile,
        };
        assert!(matches!(client.next(), Input::Started(_)));
        client
    }

    fn execute(&self, effects: Vec<Effect>) {
        self.runtime.as_ref().unwrap().execute(effects);
    }

    /// The next input, within a deadline.
    fn next(&mut self) -> Input {
        let deadline = Instant::now() + Duration::from_secs(15);
        let remaining = deadline.saturating_duration_since(Instant::now());
        block_on(async { tokio::time::timeout(remaining, self.inputs.recv()).await })
            .expect("an input in time")
            .expect("the channel is open")
    }

    /// Nothing more arrives for `window`.
    fn assert_quiet(&mut self, window: Duration) {
        let received = block_on(async { tokio::time::timeout(window, self.inputs.recv()).await });
        if let Ok(Ok(input)) = received {
            panic!("unexpected input: {input:?}");
        }
    }

    /// Checks `url` and returns what the runtime reported, in order, up to the check's answer.
    fn check(&mut self, url: &str) -> Vec<Input> {
        self.execute(vec![Effect::CheckServer {
            url: url.to_owned(),
            generation: 5,
        }]);
        let mut heard = Vec::new();
        loop {
            let input = self.next();
            let done = matches!(input, Input::ServerChecked { .. });
            heard.push(input);
            if done {
                return heard;
            }
        }
    }

    fn preview(&mut self, code: &str) -> Result<InvitationPreview, PreviewFailure> {
        self.execute(vec![Effect::PreviewInvitation {
            request: PreviewInvitationRequest {
                code: InviteCode::new(code),
            },
            generation: 9,
        }]);
        let Input::InvitationPreviewed { generation, result } = self.next() else {
            panic!("expected the answer to the question");
        };
        assert_eq!(generation, 9, "the answer echoes the question");
        result
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }
}

// ----- Tests -----

#[test]
fn what_the_server_says_about_accounts_is_heard_just_before_the_checks_answer() {
    for (says, expected) in [
        (
            Info::Says(ServerRegistration::InviteOnly, true),
            Some(ServerFacts {
                registration: ServerRegistration::InviteOnly,
                setup_code_required: true,
            }),
        ),
        (
            Info::Says(ServerRegistration::Open, false),
            Some(ServerFacts {
                registration: ServerRegistration::Open,
                setup_code_required: false,
            }),
        ),
        // A server built before the route existed, a page that is not ours, and a server speaking
        // another protocol say nothing the screen can use.
        (Info::NotFound, None),
        (Info::Garbage, None),
        (Info::OtherProtocol, None),
    ] {
        let server = Server::start(says);
        let mut client = Client::start(&server.url);
        let heard = client.check(&server.url);
        let [
            Input::ServerFacts { url, facts },
            Input::ServerChecked { generation, result },
        ] = &heard[..]
        else {
            panic!("{heard:?}");
        };
        assert_eq!(url, &server.url);
        assert_eq!(*facts, expected);
        assert_eq!(*generation, 5);
        assert_eq!(*result, Ok(PROTOCOL_VERSION));
    }
}

#[test]
fn an_address_that_does_not_answer_is_not_asked_what_it_says() {
    // A port nothing listens on: bind one and let it go.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dead = format!("http://127.0.0.1:{port}");
    let server = Server::start(Info::Says(ServerRegistration::Open, false));
    let mut client = Client::start(&server.url);
    let heard = client.check(&dead);
    assert!(
        matches!(&heard[..], [Input::ServerChecked { result: Err(_), .. }]),
        "{heard:?}"
    );
    client.assert_quiet(Duration::from_millis(300));
}

#[test]
fn a_code_is_asked_about_and_each_answer_is_told_apart() {
    let server = Server::start(Info::Says(ServerRegistration::InviteOnly, false));
    let mut client = Client::start(&server.url);

    let seen = client.preview(VALID).expect("a valid code");
    assert_eq!(seen.kind, InvitationKind::Invitation);
    assert_eq!(seen.workspace_name, "Zebrafish Collective");
    assert_eq!(seen.member_count, 12);

    assert_eq!(
        client.preview("W".repeat(20).as_str()),
        Err(PreviewFailure::Invalid)
    );
    assert_eq!(client.preview(CLOSED), Err(PreviewFailure::Closed));
    assert_eq!(client.preview(OLD_SERVER), Err(PreviewFailure::Unsupported));
    let Err(PreviewFailure::Failed(reason)) = client.preview(BROKEN) else {
        panic!("a server error is a failure to ask");
    };
    assert!(!reason.contains(BROKEN), "{reason}");

    // The code went as it was given, once each, to the server in use.
    assert_eq!(
        *server.stub.codes_seen.lock().unwrap(),
        [VALID, "WWWWWWWWWWWWWWWWWWWW", CLOSED, OLD_SERVER, BROKEN]
    );
}

#[test]
fn a_server_that_cannot_be_reached_is_a_failure_to_ask_not_a_wrong_code() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut client = Client::start(&format!("http://127.0.0.1:{port}"));
    let result = client.preview(VALID);
    let Err(PreviewFailure::Failed(reason)) = result else {
        panic!("{result:?}");
    };
    assert_eq!(reason, "connection failed");
}
