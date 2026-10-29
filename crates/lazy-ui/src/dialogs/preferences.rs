//! The preferences dialog: how and when to be notified, a status, the person's profile (name and
//! picture), and how this device looks and moves.
//!
//! Every change applies as the person makes it (the core takes it optimistically and the server
//! confirms), so there is no Save for the notification settings; the status has its own Save and
//! Clear because a half-typed status should not go out.
//!
//! # Keyboard
//!
//! Tab and Shift+Tab walk the controls in reading order: the two default levels (Left and Right
//! choose), the four switches (Space toggles), Pause (a menu), Quiet hours and, when on, its two
//! times and seven days, then the status text, its emoji button, Clear after (a menu), Save status,
//! Clear status, Choose picture…, Remove picture, the display name, Save name, the theme and
//! reduced-motion choices (Left and Right choose), and Close. Escape closes a menu or the emoji picker first, then the dialog; while
//! an input method is composing in a text field it belongs to the composition.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement, Render, Role, ScrollHandle, Styled, Subscription, Window, deferred, div,
    prelude::*, px,
};
use jiff::tz::TimeZone;
use lazy_client_core::Input;
use lazy_domain::notification::{
    MAX_STATUS_CHARS, NotificationSettings, NotifyLevel, QuietHours, UserStatus,
};
use lazy_domain::{ReactionEmoji, Timestamp};

use super::{any_composing, single_line_field};

mod profile_section;
use crate::app_model::AppModel;
use crate::emoji_picker::{EmojiPicker, EmojiPickerEvent, PickerPrefs};
use crate::form::{self, ButtonKind, Segment};
use crate::menu::{Menu, MenuEvent, MenuItem};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::notification_form::{
    self as forms, ClearAfter, DAYS, PauseFor, clock_text, offset_text, parse_clock,
    utc_offset_minutes,
};
use crate::states;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

/// Quiet hours when the person first turns them on.
const DEFAULT_QUIET: (u16, u16) = (22 * 60, 7 * 60);

pub struct PreferencesDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    zone: TimeZone,
    channel_level_focus: FocusHandle,
    direct_level_focus: FocusHandle,
    thread_focus: FocusHandle,
    channel_mention_focus: FocusHandle,
    desktop_focus: FocusHandle,
    content_focus: FocusHandle,
    pause_focus: FocusHandle,
    resume_focus: FocusHandle,
    quiet_focus: FocusHandle,
    day_focus: [FocusHandle; 7],
    quiet_start: Entity<TextInput>,
    quiet_end: Entity<TextInput>,
    status_text: Entity<TextInput>,
    /// Profile: the display name and the avatar, and Appearance (D23).
    display_name: Entity<TextInput>,
    name_problem: Option<String>,
    picture_error: Option<String>,
    name_save_focus: FocusHandle,
    picture_focus: FocusHandle,
    remove_picture_focus: FocusHandle,
    theme_focus: FocusHandle,
    motion_focus: FocusHandle,
    emoji_focus: FocusHandle,
    clear_after_focus: FocusHandle,
    save_focus: FocusHandle,
    clear_focus: FocusHandle,
    /// The emoji of the status being written.
    status_emoji: Option<ReactionEmoji>,
    clear_after: ClearAfter,
    pause_menu: Option<Entity<Menu>>,
    clear_menu: Option<Entity<Menu>>,
    picker: Option<Entity<EmojiPicker>>,
    /// Why the quiet hours' times were not applied, and why the status was not sent.
    quiet_problem: Option<String>,
    status_problem: Option<String>,
    /// The body scrolls in a short window; the keyboard brings the section it reaches into view.
    scroll: ScrollHandle,
    scrolled_for: Option<usize>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for PreferencesDialog {}

impl PreferencesDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let quiet_start = single_line_field("22:00", "Quiet hours start", 8, cx);
        let quiet_end = single_line_field("07:00", "Quiet hours end", 8, cx);
        let status_text = single_line_field("What's your status?", "Status", 400, cx);
        let display_name = single_line_field("Display name", "Display name", 256, cx);
        let current_name = model
            .read(cx)
            .core()
            .me()
            .map(|me| me.display_name.to_string())
            .unwrap_or_default();
        display_name.update(cx, |input, cx| input.set_text(&current_name, cx));
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &display_name,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => {
                    this.name_problem = None;
                    cx.notify();
                }
                TextInputEvent::Submit => this.save_name(cx),
                TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                _ => {}
            },
        ));
        // A picture that finishes decoding is drawn at once.
        subscriptions.push(
            cx.observe(&crate::image_store::ImageStore::global(cx), |_, _, cx| {
                cx.notify()
            }),
        );
        for field in [&quiet_start, &quiet_end] {
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |this, _input, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Changed => this.quiet_times_changed(cx),
                    TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                    _ => {}
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &status_text,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => {
                    this.status_problem = None;
                    cx.notify();
                }
                TextInputEvent::Submit => this.save_status(cx),
                TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                _ => {}
            },
        ));
        subscriptions.push(cx.observe(&model, |_, _, cx| cx.notify()));
        let trap = ModalTrap::new(cx);
        let first = cx.focus_handle().tab_stop(true);
        let handles: Vec<FocusHandle> = (0..16).map(|_| cx.focus_handle().tab_stop(true)).collect();
        let mut handles = handles.into_iter();
        let mut handle = move || handles.next().expect("a focus handle");
        let day_focus: [FocusHandle; 7] = std::array::from_fn(|_| cx.focus_handle().tab_stop(true));
        let mut dialog = Self {
            model,
            trap,
            zone: TimeZone::system(),
            channel_level_focus: first.clone(),
            direct_level_focus: handle(),
            thread_focus: handle(),
            channel_mention_focus: handle(),
            desktop_focus: handle(),
            content_focus: handle(),
            pause_focus: handle(),
            resume_focus: handle(),
            quiet_focus: handle(),
            day_focus,
            quiet_start,
            quiet_end,
            status_text,
            display_name,
            name_problem: None,
            picture_error: None,
            name_save_focus: cx.focus_handle().tab_stop(true),
            picture_focus: cx.focus_handle().tab_stop(true),
            remove_picture_focus: cx.focus_handle().tab_stop(true),
            theme_focus: cx.focus_handle().tab_stop(true),
            motion_focus: cx.focus_handle().tab_stop(true),
            emoji_focus: handle(),
            clear_after_focus: handle(),
            save_focus: handle(),
            clear_focus: handle(),
            status_emoji: None,
            clear_after: ClearAfter::DontClear,
            pause_menu: None,
            clear_menu: None,
            picker: None,
            quiet_problem: None,
            status_problem: None,
            scroll: ScrollHandle::new(),
            scrolled_for: None,
            _subscriptions: subscriptions,
        };
        dialog.trap.open(&first, window, cx);
        dialog.load_status(cx);
        dialog.load_quiet_times(cx);
        dialog
    }

    // ----- The core, in one place -----

    fn settings(&self, cx: &gpui::App) -> NotificationSettings {
        *self.model.read(cx).core().notification_settings()
    }

    fn apply(&self, settings: NotificationSettings, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::SetNotificationSettings(settings), cx)
        });
    }

    /// Pauses until `until`, or resumes with `None`.
    fn pause(&self, until: Option<Timestamp>, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::PauseNotifications { until }, cx)
        });
    }

    fn current_status(&self, cx: &gpui::App) -> Option<UserStatus> {
        let core = self.model.read(cx).core();
        let me = core.me()?.id;
        core.status_of(me).cloned()
    }

    fn send_status(&self, status: Option<UserStatus>, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.dispatch(Input::SetStatus(status), cx));
    }

    // ----- Loading what the core has -----

    fn load_status(&mut self, cx: &mut Context<Self>) {
        if let Some(status) = self.current_status(cx) {
            self.status_emoji = status.emoji;
            let text = status.text().to_owned();
            self.status_text
                .update(cx, |input, cx| input.set_text(&text, cx));
        }
    }

    fn load_quiet_times(&mut self, cx: &mut Context<Self>) {
        if let Some(hours) = self.settings(cx).quiet_hours {
            self.quiet_start.update(cx, |input, cx| {
                input.set_text(&clock_text(hours.start_minute), cx)
            });
            self.quiet_end.update(cx, |input, cx| {
                input.set_text(&clock_text(hours.end_minute), cx)
            });
        }
    }

    // ----- Changes -----

    fn set_level(&self, channel: bool, level: NotifyLevel, cx: &mut Context<Self>) {
        let mut settings = self.settings(cx);
        if channel {
            settings.channel_level = level;
        } else {
            settings.direct_level = level;
        }
        self.apply(settings, cx);
    }

    fn toggle(&self, change: impl FnOnce(&mut NotificationSettings), cx: &mut Context<Self>) {
        let mut settings = self.settings(cx);
        change(&mut settings);
        self.apply(settings, cx);
    }

    fn quiet_hours_on(&mut self, on: bool, cx: &mut Context<Self>) {
        let mut settings = self.settings(cx);
        settings.quiet_hours = on.then(|| QuietHours {
            start_minute: DEFAULT_QUIET.0,
            end_minute: DEFAULT_QUIET.1,
            utc_offset_minutes: utc_offset_minutes(Timestamp::now(), &self.zone),
            days: QuietHours::EVERY_DAY,
        });
        self.quiet_problem = None;
        self.apply(settings, cx);
        if on {
            self.quiet_start.update(cx, |input, cx| {
                input.set_text(&clock_text(DEFAULT_QUIET.0), cx)
            });
            self.quiet_end.update(cx, |input, cx| {
                input.set_text(&clock_text(DEFAULT_QUIET.1), cx)
            });
        }
        cx.notify();
    }

    fn toggle_day(&mut self, day: usize, cx: &mut Context<Self>) {
        let mut settings = self.settings(cx);
        let Some(hours) = settings.quiet_hours.as_mut() else {
            return;
        };
        let days = hours.days ^ (1 << day);
        if days == 0 {
            self.quiet_problem = Some(states::QUIET_NEEDS_A_DAY.to_owned());
            cx.notify();
            return;
        }
        hours.days = days;
        self.quiet_problem = None;
        self.apply(settings, cx);
    }

    /// A time was typed: apply the pair once both read as times of day and differ.
    fn quiet_times_changed(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.settings(cx);
        let Some(hours) = settings.quiet_hours.as_mut() else {
            return;
        };
        let start = parse_clock(self.quiet_start.read(cx).text());
        let end = parse_clock(self.quiet_end.read(cx).text());
        self.quiet_problem = match (start, end) {
            (Some(start), Some(end)) if start == end => Some(states::QUIET_SAME_TIME.to_owned()),
            (Some(start), Some(end)) => {
                hours.start_minute = start;
                hours.end_minute = end;
                hours.utc_offset_minutes = utc_offset_minutes(Timestamp::now(), &self.zone);
                if hours.is_valid() {
                    self.apply(settings, cx);
                }
                None
            }
            _ => Some(states::QUIET_BAD_TIME.to_owned()),
        };
        cx.notify();
    }

    fn start_pause(&mut self, choice: PauseFor, cx: &mut Context<Self>) {
        let until = forms::pause_until(choice, Timestamp::now(), &self.zone);
        self.pause(Some(until), cx);
    }

    fn save_status(&mut self, cx: &mut Context<Self>) {
        let until = forms::status_until(self.clear_after, Timestamp::now(), &self.zone);
        let text = self.status_text.read(cx).text().to_owned();
        // The core knows what a status may be: text, a live emoji, an end that is not past.
        let made = self
            .model
            .read(cx)
            .core()
            .make_status(&text, self.status_emoji, until);
        match made {
            Ok(status) => {
                self.status_problem = None;
                self.send_status(Some(status), cx);
            }
            Err(problem) => self.status_problem = Some(states::sentence(&problem)),
        }
        cx.notify();
    }

    fn clear_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send_status(None, cx);
        self.status_emoji = None;
        self.clear_after = ClearAfter::DontClear;
        self.status_problem = None;
        self.status_text
            .update(cx, |input, cx| input.set_text("", cx));
        let handle = self.status_text.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    // ----- Menus and the emoji picker -----

    fn open_pause_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = PauseFor::ALL
            .iter()
            .enumerate()
            .map(|(index, choice)| MenuItem::new(format!("pause-{index}"), choice.label()))
            .collect();
        let menu = cx.new(|cx| Menu::new("pause-menu", items, cx));
        cx.subscribe_in(
            &menu,
            window,
            |this, _menu, event: &MenuEvent, _window, cx| {
                if let MenuEvent::Select(id) = event
                    && let Some(index) = id
                        .strip_prefix("pause-")
                        .and_then(|n| n.parse::<usize>().ok())
                    && let Some(choice) = PauseFor::ALL.get(index)
                {
                    this.start_pause(*choice, cx);
                }
                this.pause_menu = None;
                cx.notify();
            },
        )
        .detach();
        menu.update(cx, |menu, cx| menu.open(window, cx));
        self.pause_menu = Some(menu);
        cx.notify();
    }

    fn open_clear_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = ClearAfter::ALL
            .iter()
            .enumerate()
            .map(|(index, choice)| MenuItem::new(format!("clear-{index}"), choice.label()))
            .collect();
        let menu = cx.new(|cx| Menu::new("clear-menu", items, cx));
        cx.subscribe_in(
            &menu,
            window,
            |this, _menu, event: &MenuEvent, _window, cx| {
                if let MenuEvent::Select(id) = event
                    && let Some(index) = id
                        .strip_prefix("clear-")
                        .and_then(|n| n.parse::<usize>().ok())
                    && let Some(choice) = ClearAfter::ALL.get(index)
                {
                    this.clear_after = *choice;
                }
                this.clear_menu = None;
                cx.notify();
            },
        )
        .detach();
        menu.update(cx, |menu, cx| menu.open(window, cx));
        self.clear_menu = Some(menu);
        cx.notify();
    }

    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = {
            let prefs = self.model.read(cx).core().emoji_prefs();
            PickerPrefs {
                skin_tone: prefs.skin_tone,
                recent: prefs.recent.clone(),
            }
        };
        let model = self.model.clone();
        let picker = cx.new(|cx| EmojiPicker::new(prefs, model, window, cx));
        cx.subscribe_in(
            &picker,
            window,
            |this, _picker, event: &EmojiPickerEvent, window, cx| {
                match event {
                    EmojiPickerEvent::Chosen(emoji) => this.status_emoji = Some(*emoji),
                    EmojiPickerEvent::SkinToneChosen(tone) => {
                        let tone = *tone;
                        this.model
                            .update(cx, |model, cx| model.dispatch(Input::SetSkinTone(tone), cx));
                        return;
                    }
                    EmojiPickerEvent::Dismiss | EmojiPickerEvent::AddCustomEmoji => {}
                }
                this.picker = None;
                window.focus(&this.emoji_focus, cx);
                cx.notify();
            },
        )
        .detach();
        self.picker = Some(picker);
        cx.notify();
    }

    /// Escape: a menu or the picker is closed by its own handling; otherwise the dialog closes,
    /// unless an input method is composing in a text field.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.quiet_start, &self.quiet_end, &self.status_text], cx) {
            return;
        }
        if self.picker.take().is_some() {
            window.focus(&self.emoji_focus, cx);
            cx.notify();
            return;
        }
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    // ----- Drawing -----

    fn heading(&self, theme: &Theme, text: &'static str) -> AnyElement {
        div()
            .pt_2()
            .text_size(theme.type_scale.title)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.colors.text)
            .child(text)
            .into_any_element()
    }

    fn note(&self, theme: &Theme, selector: &'static str, text: String) -> AnyElement {
        div()
            .debug_selector(move || selector.into())
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.text_muted)
            .child(text)
            .into_any_element()
    }

    fn problem(&self, theme: &Theme, selector: &'static str, text: String) -> AnyElement {
        div()
            .id(selector)
            .debug_selector(move || selector.into())
            .role(Role::Alert)
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.danger)
            .child(text)
            .into_any_element()
    }

    fn level_choice(
        &self,
        id: &'static str,
        title: &'static str,
        level: NotifyLevel,
        focus: &FocusHandle,
        channel: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::get(cx);
        let this = cx.entity().downgrade();
        let segments = [
            Segment {
                label: "All messages".into(),
                detail: "".into(),
            },
            Segment {
                label: "Mentions only".into(),
                detail: "".into(),
            },
            Segment {
                label: "Nothing".into(),
                detail: "".into(),
            },
        ];
        let levels = [
            NotifyLevel::All,
            NotifyLevel::Mentions,
            NotifyLevel::Nothing,
        ];
        let selected = levels.iter().position(|l| *l == level).unwrap_or(1);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text_muted)
                    .child(title),
            )
            .child(form::segmented(
                theme,
                id,
                &segments,
                selected,
                focus,
                move |index, _, cx| {
                    this.update(cx, |dialog, cx| {
                        dialog.set_level(channel, levels[index], cx)
                    })
                    .ok();
                },
            ))
            .into_any_element()
    }

    fn switch(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        focus: &FocusHandle,
        change: fn(&mut NotificationSettings, bool),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::get(cx);
        let this = cx.entity().downgrade();
        form::checkbox(theme, id, label, on, focus, move |value, _, cx| {
            this.update(cx, |dialog, cx| dialog.toggle(|s| change(s, value), cx))
                .ok();
        })
        .into_any_element()
    }

    fn pause_section(
        &mut self,
        theme: &Theme,
        settings: &NotificationSettings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let now = Timestamp::now();
        let until = settings.dnd_until.filter(|until| *until > now);
        let status = forms::pause_line(
            until,
            self.model.read(cx).core().notifications_paused(),
            now,
            &self.zone,
        );
        let resume_to = this.clone();
        let menu = self.pause_menu.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.note(theme, "prefs-pause-status", status))
            .child(
                div()
                    .relative()
                    .flex()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "prefs-pause",
                        "Pause notifications ▾",
                        ButtonKind::Secondary,
                        true,
                        &self.pause_focus,
                        move |window, cx| {
                            this.update(cx, |dialog, cx| dialog.open_pause_menu(window, cx))
                                .ok();
                        },
                    ))
                    .children(until.map(|_| {
                        form::button(
                            theme,
                            "prefs-resume",
                            "Resume",
                            ButtonKind::Primary,
                            true,
                            &self.resume_focus,
                            move |_, cx| {
                                resume_to
                                    .update(cx, |dialog, cx| dialog.pause(None, cx))
                                    .ok();
                            },
                        )
                    }))
                    .children(
                        menu.map(|menu| {
                            deferred(div().absolute().top(px(40.)).left_0().child(menu))
                        }),
                    )
                    .text_color(colors.text),
            )
            .into_any_element()
    }

    fn quiet_section(
        &mut self,
        theme: &Theme,
        settings: &NotificationSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let on = settings.quiet_hours.is_some();
        let toggle_to = this.clone();
        let mut section = div().flex().flex_col().gap_2().child(form::checkbox(
            theme,
            "prefs-quiet",
            "Quiet hours",
            on,
            &self.quiet_focus,
            move |value, _, cx| {
                toggle_to
                    .update(cx, |dialog, cx| dialog.quiet_hours_on(value, cx))
                    .ok();
            },
        ));
        if let Some(hours) = settings.quiet_hours {
            let field = |label: &'static str, input: &Entity<TextInput>, selector: &'static str| {
                let focused = input.read(cx).is_focused(window);
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w(px(120.))
                    .child(
                        div()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(label),
                    )
                    .child(
                        div()
                            .debug_selector(move || selector.into())
                            .px_3()
                            .py_1p5()
                            .rounded(theme.radii.medium)
                            .bg(colors.surface_raised)
                            .border_1()
                            .border_color(if focused {
                                colors.focus_ring
                            } else {
                                colors.border_strong
                            })
                            .child(input.clone()),
                    )
            };
            section = section
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(field("From", &self.quiet_start, "prefs-quiet-start"))
                        .child(field("Until", &self.quiet_end, "prefs-quiet-end")),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(DAYS.iter().enumerate().map(|(day, name)| {
                            let to = this.clone();
                            let selector: &'static str = [
                                "prefs-day-0",
                                "prefs-day-1",
                                "prefs-day-2",
                                "prefs-day-3",
                                "prefs-day-4",
                                "prefs-day-5",
                                "prefs-day-6",
                            ][day];
                            form::checkbox(
                                theme,
                                selector,
                                *name,
                                hours.days & (1 << day) != 0,
                                &self.day_focus[day],
                                move |_, _, cx| {
                                    to.update(cx, |dialog, cx| dialog.toggle_day(day, cx)).ok();
                                },
                            )
                        })),
                )
                .child(self.note(
                    theme,
                    "prefs-quiet-zone",
                    format!(
                        "Your time zone, taken from this computer: {}. {}",
                        offset_text(hours.utc_offset_minutes),
                        states::QUIET_ABOUT
                    ),
                ));
        }
        if let Some(problem) = self.quiet_problem.clone() {
            section = section.child(self.problem(theme, "prefs-quiet-problem", problem));
        }
        section.into_any_element()
    }

    fn status_section(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let count = self.status_text.read(cx).text().chars().count();
        let focused = self.status_text.read(cx).is_focused(window);
        let emoji_to = this.clone();
        let clear_to = this.clone();
        let save_to = this.clone();
        let cleared_to = this.clone();
        let glyph = match self.status_emoji {
            Some(emoji) => match emoji.glyph() {
                Some(glyph) => glyph.to_owned(),
                None => ":custom:".to_owned(),
            },
            None => "🙂".to_owned(),
        };
        let has_status = self.current_status(cx).is_some();
        let menu = self.clear_menu.clone();
        let picker = self.picker.clone();
        div()
            .relative()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(form::button(
                        theme,
                        "prefs-status-emoji",
                        glyph,
                        ButtonKind::Secondary,
                        true,
                        &self.emoji_focus,
                        move |window, cx| {
                            emoji_to
                                .update(cx, |dialog, cx| dialog.open_picker(window, cx))
                                .ok();
                        },
                    ))
                    .child(
                        div()
                            .debug_selector(|| "prefs-status-text".into())
                            .flex_1()
                            .min_w_0()
                            .px_3()
                            .py_1p5()
                            .rounded(theme.radii.medium)
                            .bg(colors.surface_raised)
                            .border_1()
                            .border_color(if focused {
                                colors.focus_ring
                            } else {
                                colors.border_strong
                            })
                            .child(self.status_text.clone()),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "prefs-status-count".into())
                    .text_size(theme.type_scale.small)
                    .text_color(if count > MAX_STATUS_CHARS {
                        colors.danger
                    } else {
                        colors.text_muted
                    })
                    .child(format!("{count}/{MAX_STATUS_CHARS}")),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child("Clear after"),
                    )
                    .child(form::button(
                        theme,
                        "prefs-clear-after",
                        format!("{} ▾", self.clear_after.label()),
                        ButtonKind::Secondary,
                        true,
                        &self.clear_after_focus,
                        move |window, cx| {
                            clear_to
                                .update(cx, |dialog, cx| dialog.open_clear_menu(window, cx))
                                .ok();
                        },
                    ))
                    .children(menu.map(|menu| {
                        deferred(div().absolute().top(px(40.)).left(px(90.)).child(menu))
                    })),
            )
            .children(
                self.status_problem
                    .clone()
                    .map(|text| self.problem(theme, "prefs-status-problem", text)),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "prefs-status-save",
                        "Save status",
                        ButtonKind::Primary,
                        true,
                        &self.save_focus,
                        move |_, cx| {
                            save_to.update(cx, |dialog, cx| dialog.save_status(cx)).ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "prefs-status-clear",
                        "Clear status",
                        ButtonKind::Secondary,
                        has_status || self.status_emoji.is_some() || count > 0,
                        &self.clear_focus,
                        move |window, cx| {
                            cleared_to
                                .update(cx, |dialog, cx| dialog.clear_status(window, cx))
                                .ok();
                        },
                    )),
            )
            .children(picker.map(|picker| {
                deferred(div().absolute().top(px(44.)).left_0().child(picker)).with_priority(3)
            }))
            .into_any_element()
    }
}

impl Focusable for PreferencesDialog {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.channel_level_focus.clone()
    }
}

/// Stands for the end of the body, in `focused_section`.
const LAST_ROW: usize = usize::MAX;

impl PreferencesDialog {
    /// The index, among the body's children, of the section holding the focus.
    fn focused_section(&self, window: &Window, cx: &gpui::App) -> Option<usize> {
        let focused = |handle: &FocusHandle| handle.is_focused(window);
        if focused(&self.channel_level_focus) {
            // The first stop: show the heading above it too.
            Some(0)
        } else if focused(&self.direct_level_focus) {
            Some(2)
        } else if focused(&self.thread_focus) {
            Some(3)
        } else if focused(&self.channel_mention_focus) {
            Some(4)
        } else if focused(&self.desktop_focus) {
            Some(5)
        } else if focused(&self.content_focus) {
            Some(6)
        } else if focused(&self.pause_focus) || focused(&self.resume_focus) {
            Some(7)
        } else if focused(&self.quiet_focus)
            || self.day_focus.iter().any(focused)
            || focused(&self.quiet_start.focus_handle(cx))
            || focused(&self.quiet_end.focus_handle(cx))
        {
            Some(8)
        } else if focused(&self.theme_focus) || focused(&self.motion_focus) {
            // The last section: all the way down, with nothing cut off under it.
            Some(LAST_ROW)
        } else if focused(&self.display_name.focus_handle(cx))
            || focused(&self.name_save_focus)
            || focused(&self.picture_focus)
            || focused(&self.remove_picture_focus)
        {
            Some(12)
        } else if focused(&self.emoji_focus)
            || focused(&self.status_text.focus_handle(cx))
            || focused(&self.clear_after_focus)
            || focused(&self.save_focus)
            || focused(&self.clear_focus)
        {
            Some(10)
        } else {
            None
        }
    }
}

impl Render for PreferencesDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let settings = self.settings(cx);
        let this = cx.entity().downgrade();
        let close_to = this.clone();
        // Scroll once when the keyboard arrives in a section, so the mouse wheel is not fought.
        // A click that focuses a control does not scroll: the control is under the pointer, and
        // moving it between press and release would lose the click.
        let section = self.focused_section(window, cx);
        if section != self.scrolled_for {
            self.scrolled_for = section;
            if window.last_input_was_keyboard() {
                match section {
                    Some(LAST_ROW) => self.scroll.scroll_to_bottom(),
                    Some(index) => self.scroll.scroll_to_item(index),
                    None => {}
                }
            }
        }
        let body = div()
            .id("prefs-scroll")
            .debug_selector(|| "prefs-scroll".into())
            // As tall as the window allows, so little needs scrolling in a big window.
            .max_h(px(
                (f32::from(window.viewport_size().height) - 200.0).clamp(240.0, 1000.0)
            ))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap_3()
            .pr_1()
            .child(self.heading(&theme, "Notifications"))
            .child(self.level_choice(
                "prefs-channel-level",
                "In channels, notify me about",
                settings.channel_level,
                &self.channel_level_focus,
                true,
                cx,
            ))
            .child(self.level_choice(
                "prefs-direct-level",
                "In direct messages and groups, notify me about",
                settings.direct_level,
                &self.direct_level_focus,
                false,
                cx,
            ))
            .child(self.switch(
                "prefs-thread-replies",
                "Notify me about replies in threads I follow",
                settings.thread_replies,
                &self.thread_focus,
                |s, v| s.thread_replies = v,
                cx,
            ))
            .child(self.switch(
                "prefs-channel-mentions",
                "Notify me when someone uses @channel",
                settings.channel_mentions,
                &self.channel_mention_focus,
                |s, v| s.channel_mentions = v,
                cx,
            ))
            .child(self.switch(
                "prefs-desktop",
                "Show desktop notifications",
                settings.desktop,
                &self.desktop_focus,
                |s, v| s.desktop = v,
                cx,
            ))
            .child(self.switch(
                "prefs-show-content",
                "Show message text in notifications",
                settings.show_content,
                &self.content_focus,
                |s, v| s.show_content = v,
                cx,
            ))
            .child(self.pause_section(&theme, &settings, cx))
            .child(self.quiet_section(&theme, &settings, window, cx))
            .child(self.heading(&theme, "Status"))
            // A little room under it, so the last button of the section is not cut off when the
            // keyboard scrolls the section into view.
            .child(div().pb_1().child(self.status_section(&theme, window, cx)))
            .child(self.heading(&theme, "Profile"))
            .child(div().pb_1().child(self.profile_section(&theme, window, cx)))
            .child(self.heading(&theme, "Appearance"))
            .child(self.appearance_section(&theme, cx));
        dialog_overlay(
            &theme,
            "Preferences",
            580.,
            &self.trap,
            body.into_any_element(),
            "Close",
            move |window, cx| {
                close_to
                    .update(cx, |dialog, cx| {
                        dialog.trap.close(window, cx);
                        cx.emit(DialogEvent::Dismiss);
                    })
                    .ok();
            },
        )
        .on_action(cx.listener(|this, _: &DialogDismiss, window, cx| this.dismiss(window, cx)))
        .on_action(cx.listener(|this, _: &DialogNext, window, cx| this.trap.tab(false, window, cx)))
        .on_action(cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)))
    }
}
