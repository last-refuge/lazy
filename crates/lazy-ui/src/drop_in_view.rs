//! The drop-in stage: while this person is in a drop-in, a band at the top of the main column with a
//! tile per person (their video, or their avatar while it is off) and the controls (mute, camera,
//! devices, leave). After a drop-in fails, the band says why until dismissed.
//!
//! Devices opens a menu with a tab each for the camera, microphone, and speakers: "Automatic" and
//! every device the computer offers, the one in use marked. Choosing switches the call at once
//! and is remembered on this computer for the next.
//!
//! Frames come from the runtime's [`FrameStore`] (shared through [`AppModel::frames`]), never
//! through the core. While a drop-in runs, a timer checks the store and redraws when a frame
//! changed; each new frame becomes a GPUI image, and the one it replaces is dropped from the
//! renderer so the atlas does not grow.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, FocusHandle, FontWeight, ObjectFit, RenderImage, Role,
    SharedString, Subscription, Task, Window, div, img, px,
};
use lazy_client_core::{
    ClientCore, DeviceChoices, DeviceKind, DropIn, DropInStatus, Input, MediaDevices,
};
use rtc_video::{FrameStore, TileKey};

use crate::app_model::AppModel;
use crate::avatar::Avatar;
use crate::form::{self, ButtonKind};
use crate::menu::{Menu, MenuEvent, MenuItem};
use crate::theme::Theme;

/// A tile's size: 4:3, like the frames.
pub const TILE_WIDTH: f32 = 200.;
pub const TILE_HEIGHT: f32 = 150.;
/// How often the store is checked for new frames while a drop-in runs.
const FRAME_CHECK: Duration = Duration::from_millis(40);

pub struct DropInView {
    model: Entity<AppModel>,
    mute_focus: FocusHandle,
    video_focus: FocusHandle,
    leave_focus: FocusHandle,
    devices_focus: FocusHandle,
    dismiss_focus: FocusHandle,
    /// The open Devices menu, what each of its items chooses, and the lists it was made from.
    devices_menu: Option<Entity<Menu>>,
    menu_choices: HashMap<SharedString, (DeviceKind, Option<String>)>,
    menu_made_from: Option<(MediaDevices, DeviceChoices)>,
    _menu_subscription: Option<Subscription>,
    /// The image drawn for each tile, and the frame serial it was made from.
    images: HashMap<TileKey, (u64, Arc<RenderImage>)>,
    /// Images replaced since the last frame drawn, to drop from the renderer.
    retired: Vec<Arc<RenderImage>>,
    /// The frame store's serial when last drawn.
    drawn_serial: u64,
    refresh: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl DropInView {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let observed = cx.observe(&model, |this, _, cx| {
            this.follow_frames(cx);
            this.refresh_devices_menu(cx);
            cx.notify();
        });
        let mut view = Self {
            model,
            mute_focus: cx.focus_handle().tab_stop(true),
            video_focus: cx.focus_handle().tab_stop(true),
            leave_focus: cx.focus_handle().tab_stop(true),
            devices_focus: cx.focus_handle().tab_stop(true),
            dismiss_focus: cx.focus_handle().tab_stop(true),
            devices_menu: None,
            menu_choices: HashMap::new(),
            menu_made_from: None,
            _menu_subscription: None,
            images: HashMap::new(),
            retired: Vec::new(),
            drawn_serial: 0,
            refresh: None,
            _subscriptions: vec![observed],
        };
        view.follow_frames(cx);
        view
    }

    /// Whether there is anything to show: a drop-in, or why the last one ended.
    pub fn is_shown(&self, cx: &gpui::App) -> bool {
        let core = self.model.read(cx).core();
        core.drop_in().is_some() || core.drop_in_error().is_some()
    }

    fn frames(&self, cx: &gpui::App) -> FrameStore {
        self.model.read(cx).frames().clone()
    }

    /// Starts checking for frames while in a drop-in, and stops after.
    fn follow_frames(&mut self, cx: &mut Context<Self>) {
        let in_drop_in = self.model.read(cx).core().drop_in().is_some();
        if !in_drop_in {
            self.refresh = None;
            if !self.images.is_empty() {
                self.retired
                    .extend(self.images.drain().map(|(_, (_, image))| image));
            }
            return;
        }
        if self.refresh.is_some() {
            return;
        }
        let frames = self.frames(cx);
        self.refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME_CHECK).await;
                let serial = frames.serial();
                let more = this
                    .update(cx, |view, cx| {
                        if serial != view.drawn_serial {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !more {
                    break;
                }
            }
        }));
    }

    /// The Devices menu's tabs, and what each item chooses.
    #[allow(clippy::type_complexity)]
    fn device_tabs(
        core: &ClientCore,
    ) -> (
        Vec<(SharedString, Vec<MenuItem>)>,
        HashMap<SharedString, (DeviceKind, Option<String>)>,
    ) {
        let devices = core.media_devices();
        let chosen = core.device_choices();
        let mut choices = HashMap::new();
        let tabs = [
            (
                DeviceKind::Camera,
                "Camera",
                "camera",
                "The first camera that works",
            ),
            (
                DeviceKind::Microphone,
                "Microphone",
                "microphone",
                "The system's default",
            ),
            (
                DeviceKind::Speakers,
                "Speakers",
                "speakers",
                "The system's default",
            ),
        ]
        .into_iter()
        .map(|(kind, title, key, automatic)| {
            let current = chosen.get(kind);
            let listed = devices.of(kind);
            // A chosen device that is not listed (unplugged) is not in use: automatic is.
            let in_use_listed = current.is_some_and(|id| listed.iter().any(|d| d.id == id));
            let mark = |item: MenuItem, on: bool| if on { item.shortcut("In use") } else { item };
            let auto_id = SharedString::from(format!("{key}-auto"));
            choices.insert(auto_id.clone(), (kind, None));
            let mut items = vec![mark(
                MenuItem::new(auto_id, "Automatic").detail(automatic),
                !in_use_listed,
            )];
            for (index, device) in listed.iter().enumerate() {
                let id = SharedString::from(format!("{key}-{index}"));
                choices.insert(id.clone(), (kind, Some(device.id.clone())));
                items.push(mark(
                    MenuItem::new(id, device.name.clone()),
                    current == Some(device.id.as_str()),
                ));
            }
            if listed.is_empty() {
                items.push(MenuItem::new(format!("{key}-none"), "None found").enabled(false));
            }
            (SharedString::from(title), items)
        })
        .collect();
        (tabs, choices)
    }

    fn open_devices_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Plugged-in devices change: list them again each time.
        self.model
            .update(cx, |m, cx| m.dispatch(Input::ListMediaDevices, cx));
        let core = self.model.read(cx).core();
        let (tabs, choices) = Self::device_tabs(core);
        self.menu_made_from = Some((core.media_devices().clone(), core.device_choices().clone()));
        self.menu_choices = choices;
        let menu = cx.new(|cx| Menu::tabbed("drop-in-devices", tabs, cx));
        self._menu_subscription = Some(cx.subscribe(&menu, |this, _, event: &MenuEvent, cx| {
            if let MenuEvent::Select(id) = event
                && let Some((kind, choice)) = this.menu_choices.get(id).cloned()
            {
                this.model.update(cx, |m, cx| {
                    m.dispatch(Input::ChooseDevice { kind, id: choice }, cx)
                });
            }
            this.devices_menu = None;
            this.menu_made_from = None;
            cx.notify();
        }));
        menu.update(cx, |menu, cx| menu.open(window, cx));
        self.devices_menu = Some(menu);
        cx.notify();
    }

    /// Keeps an open Devices menu in step with the lists (the fresh listing arrives after it
    /// opens), and closes it when the drop-in ends.
    fn refresh_devices_menu(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.devices_menu.clone() else {
            return;
        };
        let core = self.model.read(cx).core();
        if core.drop_in().is_none() {
            self.devices_menu = None;
            self.menu_made_from = None;
            return;
        }
        let now = (core.media_devices().clone(), core.device_choices().clone());
        if self.menu_made_from.as_ref() == Some(&now) {
            return;
        }
        let (tabs, choices) = Self::device_tabs(core);
        self.menu_made_from = Some(now);
        self.menu_choices = choices;
        menu.update(cx, |menu, cx| menu.replace_tabs(tabs, cx));
    }

    /// The image for `key`'s newest frame, made once per frame.
    fn image(&mut self, frames: &FrameStore, key: TileKey) -> Option<Arc<RenderImage>> {
        let Some(tile) = frames.get(key) else {
            if let Some((_, old)) = self.images.remove(&key) {
                self.retired.push(old);
            }
            return None;
        };
        if let Some((serial, image)) = self.images.get(&key)
            && *serial == tile.serial
        {
            return Some(image.clone());
        }
        let frame = &tile.frame;
        let buffer = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra.clone())?;
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        if let Some((_, old)) = self.images.insert(key, (tile.serial, image.clone())) {
            self.retired.push(old);
        }
        Some(image)
    }

    fn tile(
        &mut self,
        theme: &Theme,
        frames: &FrameStore,
        key: TileKey,
        (name, seed, muted, video_on, speaking): (String, u128, bool, bool, bool),
    ) -> AnyElement {
        let colors = theme.colors;
        let picture = if video_on {
            self.image(frames, key)
        } else {
            if let Some((_, old)) = self.images.remove(&key) {
                self.retired.push(old);
            }
            None
        };
        let label = match key {
            TileKey::Local => "drop-in-tile-me".to_owned(),
            TileKey::Remote(id) => format!("drop-in-tile-{id}"),
        };
        let has_picture = picture.is_some();
        let aria = format!(
            "{name}{}{}{}",
            if speaking { ", speaking" } else { "" },
            if muted { ", muted" } else { "" },
            if has_picture { "" } else { ", camera off" }
        );
        let selector = label.clone();
        div()
            .id(SharedString::from(label))
            .debug_selector(move || selector.clone())
            .role(Role::Group)
            .aria_label(aria)
            .relative()
            .flex_none()
            .w(px(TILE_WIDTH))
            .h(px(TILE_HEIGHT))
            .rounded(theme.radii.medium)
            .overflow_hidden()
            .bg(colors.code_surface)
            .border_2()
            .border_color(if speaking {
                colors.success
            } else {
                colors.border
            })
            .flex()
            .items_center()
            .justify_center()
            .child(match picture {
                Some(picture) => img(picture)
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    .into_any_element(),
                None => Avatar::new(seed, name.clone(), 56.).into_any_element(),
            })
            .child(
                div()
                    .absolute()
                    .left(px(6.))
                    .bottom(px(6.))
                    .max_w(px(TILE_WIDTH - 12.))
                    .px_1p5()
                    .py_0p5()
                    .rounded(theme.radii.small)
                    .bg(gpui::black().opacity(0.55))
                    .text_color(gpui::white())
                    .text_size(theme.type_scale.small)
                    .truncate()
                    .child(if muted {
                        format!("{name} · muted")
                    } else {
                        name
                    }),
            )
            .into_any_element()
    }

    fn controls(&self, theme: &Theme, drop_in: &DropIn, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.downgrade();
        let this = cx.entity().downgrade();
        let (mute_to, video_to, leave_to) = (model.clone(), model.clone(), model);
        let muted = drop_in.muted;
        let video_on = drop_in.video_on;
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(form::button(
                theme,
                "drop-in-mute",
                if muted { "Unmute" } else { "Mute" },
                ButtonKind::Secondary,
                true,
                &self.mute_focus,
                move |_, cx| {
                    mute_to
                        .update(cx, |m, cx| m.dispatch(Input::SetDropInMuted(!muted), cx))
                        .ok();
                },
            ))
            .child(form::button(
                theme,
                "drop-in-video",
                if video_on {
                    "Stop video"
                } else {
                    "Start video"
                },
                ButtonKind::Secondary,
                true,
                &self.video_focus,
                move |_, cx| {
                    video_to
                        .update(cx, |m, cx| m.dispatch(Input::SetDropInVideo(!video_on), cx))
                        .ok();
                },
            ))
            .child(
                div()
                    .relative()
                    .child(form::button(
                        theme,
                        "drop-in-devices",
                        "Devices",
                        ButtonKind::Secondary,
                        true,
                        &self.devices_focus,
                        move |window, cx| {
                            this.update(cx, |view, cx| view.open_devices_menu(window, cx))
                                .ok();
                        },
                    ))
                    .children(self.devices_menu.clone().map(|menu| {
                        gpui::deferred(div().absolute().top(px(44.)).right_0().child(menu))
                    })),
            )
            .child(form::button(
                theme,
                "drop-in-leave",
                "Leave",
                ButtonKind::Danger,
                true,
                &self.leave_focus,
                move |_, cx| {
                    leave_to
                        .update(cx, |m, cx| m.dispatch(Input::LeaveDropIn, cx))
                        .ok();
                },
            ))
            .into_any_element()
    }

    fn failed(&self, theme: &Theme, reason: &str) -> AnyElement {
        let colors = theme.colors;
        let model = self.model.downgrade();
        div()
            .debug_selector(|| "drop-in-error".into())
            .flex_none()
            .px_5()
            .py_2()
            .flex()
            .items_center()
            .gap_3()
            .bg(colors.warning_surface)
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text)
                    .child(format!("The drop-in ended: {reason}")),
            )
            .child(form::button(
                theme,
                "drop-in-dismiss",
                "Dismiss",
                ButtonKind::Ghost,
                true,
                &self.dismiss_focus,
                move |_, cx| {
                    model
                        .update(cx, |m, cx| m.dispatch(Input::DismissDropInError, cx))
                        .ok();
                },
            ))
            .into_any_element()
    }
}

impl Render for DropInView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        // The renderer forgets images replaced since the last frame.
        for old in self.retired.drain(..) {
            window.drop_image(old).ok();
        }
        let core = self.model.read(cx).core();
        let Some(drop_in) = core.drop_in().cloned() else {
            return match core.drop_in_error().map(str::to_owned) {
                Some(reason) => self.failed(&theme, &reason),
                None => div().into_any_element(),
            };
        };
        let title = core.conversation_title(drop_in.conversation);
        let prefix = core
            .conversation(drop_in.conversation)
            .filter(|c| c.kind == lazy_domain::ConversationKind::Channel)
            .map_or("", |_| "#");
        let me = core.me().map(|me| (me.display_name.to_string(), me.id));
        let frames = self.frames(cx);
        self.drawn_serial = frames.serial();

        let people = drop_in.peers.len() + 1;
        let status = match drop_in.status {
            DropInStatus::Joining => "Joining…".to_owned(),
            DropInStatus::Connecting => "Connecting…".to_owned(),
            DropInStatus::Live if people == 1 => "Just you so far".to_owned(),
            DropInStatus::Live => format!("{people} people"),
        };

        let (my_name, my_seed) = me
            .map(|(name, id)| (name, id.as_uuid().as_u128()))
            .unwrap_or_else(|| ("You".to_owned(), 0));
        let mut tiles = vec![self.tile(
            &theme,
            &frames,
            TileKey::Local,
            (
                format!("{my_name} (you)"),
                my_seed,
                drop_in.muted,
                drop_in.video_on,
                drop_in.is_speaking(None),
            ),
        )];
        for peer in &drop_in.peers {
            let seed = peer
                .user
                .map_or(u128::from(peer.id), |u| u.as_uuid().as_u128());
            tiles.push(self.tile(
                &theme,
                &frames,
                TileKey::Remote(peer.id),
                (
                    peer.name.clone(),
                    seed,
                    peer.muted,
                    peer.video_on,
                    drop_in.is_speaking(Some(peer.id)),
                ),
            ));
        }

        div()
            .id("drop-in-stage")
            .debug_selector(|| "drop-in-stage".into())
            .role(Role::Region)
            .aria_label(format!("Drop-in, {prefix}{title}"))
            .flex_none()
            .flex()
            .flex_col()
            .gap_3()
            .px_5()
            .py_3()
            .bg(colors.surface_raised)
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .child(format!("Drop-in · {prefix}{title}")),
                    )
                    .child(
                        div()
                            .debug_selector(|| "drop-in-status".into())
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(match &drop_in.problem {
                                Some(problem) => format!("{status} · {problem}"),
                                None => status,
                            }),
                    )
                    .child(self.controls(&theme, &drop_in, cx)),
            )
            .child(
                div()
                    .id("drop-in-tiles")
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .max_h(px(TILE_HEIGHT * 2. + 12.))
                    .overflow_y_scroll()
                    .children(tiles),
            )
            .into_any_element()
    }
}
