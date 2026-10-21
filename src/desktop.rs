use std::{sync::Arc, time::Duration};

use gpui::{
    actions, div, img, prelude::*, px, rgb, size, App, Application, Bounds, ClipboardItem,
    Context, Div, FocusHandle, FontWeight, Image, ImageFormat, KeyBinding, MouseButton, RenderImage,
    PromptButton, PromptLevel, Stateful, Subscription, Task, TitlebarOptions, Window, WindowBounds,
    WindowControlArea, WindowOptions,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{
        Input::KeyboardAndMouse::ReleaseCapture,
        WindowsAndMessaging::{
            IsIconic, PostMessageW, SetForegroundWindow, ShowWindow, ShowWindowAsync, HTCAPTION,
            SC_MOVE, SW_HIDE, SW_RESTORE, SW_SHOW, WM_CLOSE, WM_SYSCOMMAND,
        },
    },
};

use crate::{
    server::{ServerController, ServerSnapshot, ServerStatus},
    tray::{Tray, TrayAction},
};

const BACKGROUND: u32 = 0x101216;
const PANEL: u32 = 0x191c22;
const BORDER: u32 = 0x2b3039;
const TEXT: u32 = 0xf1f3f7;
const MUTED: u32 = 0x929ba9;
const BLUE: u32 = 0x628dff;
const GREEN: u32 = 0x60d6a0;
const ERROR: u32 = 0xff9999;

actions!(touch_cursor, [FocusNext, FocusPrevious, Activate, CloseWindow]);

pub fn run(server: ServerController) {
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("tab", FocusNext, Some("TouchCursor")),
            KeyBinding::new("shift-tab", FocusPrevious, Some("TouchCursor")),
            KeyBinding::new("space", Activate, Some("TouchCursorControl")),
            KeyBinding::new("enter", Activate, Some("TouchCursorControl")),
            KeyBinding::new("ctrl-w", CloseWindow, Some("TouchCursor")),
        ]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(500.), px(460.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(440.), px(420.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("TouchCursor".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                app_id: Some("TouchCursor".into()),
                ..Default::default()
            },
            move |window, cx| cx.new(move |cx| Desktop::new(server, window, cx)),
        )
        .expect("Could not open the TouchCursor desktop window");
        cx.activate(true);
    });
}

struct Desktop {
    // Explicitly release native resources before the GPUI event loop stops.
    server: Option<ServerController>,
    tray: Option<Tray>,
    tray_error: Option<String>,
    quit_requested: bool,
    snapshot: ServerSnapshot,
    icon: Arc<RenderImage>,
    root_focus: FocusHandle,
    toggle_focus: FocusHandle,
    copy_focus: FocusHandle,
    open_focus: FocusHandle,
    trust_focus: FocusHandle,
    copied: bool,
    confirming_trust_reset: bool,
    copy_feedback: Option<Task<()>>,
    _server_watch: Task<()>,
    _tray_events: Option<Task<()>>,
    _shutdown: Subscription,
}

impl Desktop {
    fn new(server: ServerController, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut changes = server.subscribe();
        let snapshot = changes.borrow_and_update().clone();
        let icon = Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../static/icon-256.png").to_vec(),
        )
        .to_image_data(cx.svg_renderer())
        .expect("The embedded TouchCursor icon must be a valid PNG");
        let root_focus = cx.focus_handle();
        let toggle_focus = cx.focus_handle().tab_index(1).tab_stop(true);
        let copy_focus = cx.focus_handle().tab_index(3).tab_stop(true);
        let open_focus = cx.focus_handle().tab_index(2).tab_stop(true);
        let trust_focus = cx.focus_handle().tab_index(4).tab_stop(true);
        toggle_focus.focus(window);

        let handle = native_window_handle(window);
        let (tray, tray_events, tray_error) = match Tray::new(handle, server.subscribe()) {
            Ok((tray, mut actions)) => {
                let handle = handle as usize;
                let events = cx.spawn(async move |this, cx| {
                    while let Some(action) = actions.recv().await {
                        let active = this.update(cx, |this, cx| {
                            if this.quit_requested {
                                return false;
                            }
                            match action {
                                TrayAction::ToggleServer => this.toggle_server(),
                                TrayAction::Quit => {
                                    this.quit_requested = true;
                                    cx.quit();
                                }
                                TrayAction::ShowWindow | TrayAction::HideWindow => {}
                            }
                            true
                        });
                        if !matches!(active, Ok(true)) {
                            return;
                        }
                        // Native activation can re-enter GPUI. Release the view
                        // update before changing window visibility or focus.
                        match action {
                            TrayAction::ShowWindow => show_native_window(handle as HWND),
                            TrayAction::HideWindow => hide_native_window(handle as HWND),
                            TrayAction::ToggleServer => {}
                            TrayAction::Quit => return,
                        }
                    }
                    // A failed tray thread must not leave an inaccessible app.
                    let show = this.update(cx, |this, cx| {
                        if this.quit_requested {
                            return false;
                        }
                        drop(this.tray.take());
                        this.tray_error = Some(
                            "System tray unavailable. Closing the window will quit TouchCursor and stop the server."
                                .into(),
                        );
                        cx.notify();
                        true
                    });
                    if matches!(show, Ok(true)) {
                        show_native_window(handle as HWND);
                    }
                });
                (Some(tray), Some(events), None)
            }
            Err(error) => (
                None,
                None,
                Some(format!(
                    "System tray unavailable: {error}. Closing the window will quit TouchCursor and stop the server."
                )),
            ),
        };

        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let hide = view
                .update(cx, |this, _| this.tray.is_some() && !this.quit_requested)
                .unwrap_or(false);
            if hide {
                hide_native_window(native_window_handle(window));
            }
            !hide
        });

        // The watch future is woken by server changes; it does not need a Tokio
        // executor or a periodic UI refresh. The task retains only a weak view.
        let server_watch = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let next = changes.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| {
                        if this.snapshot != next {
                            if this.snapshot.status != next.status
                                || this.snapshot.url != next.url
                                || this.snapshot.pin != next.pin
                            {
                                this.copied = false;
                                this.copy_feedback = None;
                            }
                            this.snapshot = next;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let shutdown = cx.on_app_quit(|this, _| {
            this.quit_requested = true;
            drop(this._tray_events.take());
            drop(this.tray.take());
            drop(this.server.take());
            async {}
        });

        Self {
            server: Some(server),
            tray,
            tray_error,
            quit_requested: false,
            snapshot,
            icon,
            root_focus,
            toggle_focus,
            copy_focus,
            open_focus,
            trust_focus,
            copied: false,
            confirming_trust_reset: false,
            copy_feedback: None,
            _server_watch: server_watch,
            _tray_events: tray_events,
            _shutdown: shutdown,
        }
    }

    fn online(&self) -> bool {
        matches!(&self.snapshot.status, ServerStatus::Running)
    }

    fn transitioning(&self) -> bool {
        matches!(
            &self.snapshot.status,
            ServerStatus::Starting | ServerStatus::Stopping
        )
    }

    fn address(&self) -> Option<&str> {
        self.online().then_some(self.snapshot.url.as_deref()).flatten()
    }

    fn toggle_server(&mut self) {
        if let Some(server) = &self.server {
            let latest = server.subscribe();
            let snapshot = latest.borrow();
            if !matches!(
                &snapshot.status,
                ServerStatus::Starting | ServerStatus::Stopping
            ) {
                server.set_enabled(!matches!(&snapshot.status, ServerStatus::Running));
            }
        }
    }

    fn forget_trusted_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.transitioning() || self.confirming_trust_reset {
            return;
        }
        self.confirming_trust_reset = true;
        cx.notify();
        let answer = window.prompt(
            PromptLevel::Warning,
            "Forget all trusted devices?",
            Some("Every controller will disconnect. Devices must pair again with a new PIN."),
            &[
                PromptButton::Cancel("Cancel".into()),
                PromptButton::Ok("Forget all".into()),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let confirmed = matches!(answer.await, Ok(1));
            let _ = this.update(cx, |this, cx| {
                this.confirming_trust_reset = false;
                if confirmed {
                    if let Some(server) = &this.server {
                        server.forget_trusted_devices();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }


    fn copy_address(&mut self, cx: &mut Context<Self>) {
        let Some(address) = self.address() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(address.to_owned()));
        self.copied = true;
        self.copy_feedback = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied {
                    this.copied = false;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn open_address(&self, cx: &App) {
        if let Some(address) = self.address() {
            cx.open_url(address);
        }
    }

    fn status(&self) -> (&'static str, u32) {
        match &self.snapshot.status {
            ServerStatus::Starting => ("Server starting", BLUE),
            ServerStatus::Running => ("Server online", GREEN),
            ServerStatus::Stopping => ("Server stopping", BLUE),
            ServerStatus::Stopped => ("Server offline", MUTED),
            ServerStatus::Error(_) => ("Server error", ERROR),
        }
    }

    fn pairing_hint(&self) -> &'static str {
        match &self.snapshot.status {
            ServerStatus::Running if self.snapshot.url.is_some() => {
                "Same Wi-Fi. Open the address on your phone."
            }
            ServerStatus::Running => {
                "No LAN address found. Connect this PC to Wi-Fi or Ethernet, then switch the server off and on."
            }
            ServerStatus::Starting => "The address and PIN will appear when the server is ready.",
            ServerStatus::Stopping => "Disconnecting phone sessions…",
            ServerStatus::Stopped => "Turn on the server to pair your phone.",
            ServerStatus::Error(_) => "Turn on the server to retry.",
        }
    }

    fn render_titlebar(&self, window: &Window) -> impl IntoElement {
        let maximized = window.is_maximized();
        div()
            .flex()
            .items_center()
            .h(px(44.))
            .w_full()
            .flex_shrink_0()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .id("titlebar-drag")
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, |event, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                        if event.click_count == 2 {
                            toggle_maximize(window);
                        } else {
                            drag_window(window);
                        }
                    })
                    .flex()
                    .items_center()
                    .gap_2()
                    .h_full()
                    .flex_1()
                    .pl_5()
                    .child(img(self.icon.clone()).size(px(24.)))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("TouchCursor"),
                    ),
            )
            .child(caption_button(
                "minimize",
                "−",
                WindowControlArea::Min,
                "Minimize",
            ))
            .child(caption_button(
                "maximize",
                if maximized { "❐" } else { "□" },
                WindowControlArea::Max,
                if maximized { "Restore" } else { "Maximize" },
            ))
            .child(caption_button(
                "close",
                "×",
                WindowControlArea::Close,
                if self.tray.is_some() {
                    "Hide TouchCursor to the system tray"
                } else {
                    "Close TouchCursor and stop the server"
                },
            ))
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (heading, color) = self.status();
        let transitioning = self.transitioning();
        let enabled = matches!(
            &self.snapshot.status,
            ServerStatus::Starting | ServerStatus::Running
        );
        let toggle_label = if enabled { "On" } else { "Off" };
        let thumb = div().size(px(22.)).rounded_full().bg(rgb(TEXT));
        let label = div()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(if enabled { 0xffffff } else { 0xc6ccd6 }))
            .child(toggle_label);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_shrink_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(8.)).rounded_full().bg(rgb(color)))
                            .child(
                                div()
                                    .text_size(px(20.))
                                    .line_height(px(26.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(heading),
                            ),
                    )
                    .child(
                        div()
                            .id("server-toggle")
                            .flex()
                            .items_center()
                            .justify_between()
                            .w(px(76.))
                            .h(px(34.))
                            .px(px(6.))
                            .border_1()
                            .border_color(rgb(if enabled { BLUE } else { 0x424957 }))
                            .rounded_full()
                            .bg(rgb(if enabled { 0x456ee1 } else { BORDER }))
                            .flex_shrink_0()
                            .track_focus(&self.toggle_focus)
                            .key_context("TouchCursorControl")
                            .focus(|style| style.border_color(rgb(TEXT)))
                            .when(!transitioning, |this| this.cursor_pointer())
                            .when(transitioning, |this| this.opacity(0.55))
                            .on_click(cx.listener(|this, _, window, _| {
                                this.toggle_focus.focus(window);
                                this.toggle_server();
                            }))
                            .on_action(cx.listener(|this, _: &Activate, _, _| {
                                this.toggle_server();
                            }))
                            .map(|this| {
                                if enabled {
                                    this.child(label).child(thumb)
                                } else {
                                    this.child(thumb).child(label)
                                }
                            }),
                    ),
            )
            .child(self.render_devices())
            .when_some(
                match &self.snapshot.status {
                    ServerStatus::Error(message) => Some(message.clone()),
                    _ => None,
                },
                |this, error| {
                    this.child(
                        div()
                            .w_full()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x683a42))
                            .bg(rgb(0x2b1d25))
                            .text_size(px(13.))
                            .line_height(px(19.))
                            .text_color(rgb(ERROR))
                            .child(error),
                    )
                },
            )
    }

    fn render_devices(&self) -> impl IntoElement {
        div()
            .pl_4()
            .text_size(px(13.))
            .line_height(px(18.))
            .text_color(rgb(MUTED))
            .child(format!(
                "{} device{} connected",
                self.snapshot.devices,
                if self.snapshot.devices == 1 { "" } else { "s" }
            ))
    }

    fn render_pairing(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let available = self.address().is_some();
        let address = self.address().unwrap_or(if self.online() {
            "No LAN address available"
        } else {
            "Address available when online"
        });
        let pin = if available { self.snapshot.pin.as_str() } else { "────" };

        div()
            .flex()
            .flex_col()
            .w_full()
            .flex_shrink_0()
            .gap_4()
            .p_5()
            .rounded_xl()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(rgb(MUTED))
                            .child("Phone address"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id("open-address")
                                    .flex_1()
                                    .min_w(px(0.))
                                    .px_1()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(PANEL))
                                    .text_size(px(if available { 17. } else { 13. }))
                                    .line_height(px(24.))
                                    .text_color(rgb(if available { 0xb7c9ff } else { MUTED }))
                                    .when(available, |this| {
                                        this.font_family("Consolas")
                                            .cursor_pointer()
                                            .track_focus(&self.open_focus)
                                            .key_context("TouchCursorControl")
                                            .hover(|style| style.text_color(rgb(TEXT)))
                                            .focus(|style| style.border_color(rgb(BLUE)))
                                    })
                                    .child(address.to_owned())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if this.address().is_some() {
                                            this.open_focus.focus(window);
                                            this.open_address(cx);
                                        }
                                    }))
                                    .on_action(cx.listener(|this, _: &Activate, _, cx| {
                                        this.open_address(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("copy-address")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(68.))
                                    .h(px(28.))
                                    .flex_shrink_0()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .text_size(px(12.))
                                    .text_color(rgb(if self.copied { GREEN } else { 0xc2ccdc }))
                                    .when(available, |this| {
                                        this.cursor_pointer()
                                            .track_focus(&self.copy_focus)
                                            .key_context("TouchCursorControl")
                                            .hover(|style| style.bg(rgb(BORDER)))
                                            .active(|style| style.bg(rgb(0x363e4d)))
                                            .focus(|style| style.border_color(rgb(BLUE)))
                                    })
                                    .when(!available, |this| this.opacity(0.35))
                                    .child(if self.copied { "Copied!" } else { "Copy" })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if this.address().is_some() {
                                            this.copy_focus.focus(window);
                                            this.copy_address(cx);
                                        }
                                    }))
                                    .on_action(cx.listener(|this, _: &Activate, _, cx| {
                                        this.copy_address(cx);
                                    })),
                            ),
                    ),
            )
            .child(div().h(px(1.)).w_full().bg(rgb(BORDER)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(rgb(MUTED))
                            .child("Pairing PIN"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .children(pin.chars().enumerate().map(|(index, digit)| {
                                div()
                                    .id(("pin-digit", index))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(48.))
                                    .h(px(48.))
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .bg(rgb(BACKGROUND))
                                    .font_family("Consolas")
                                    .text_size(px(28.))
                                    .line_height(px(34.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(if available { TEXT } else { 0x525c6c }))
                                    .child(digit.to_string())
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(rgb(MUTED))
                    .child(self.pairing_hint()),
            )
    }
    fn render_trust_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.snapshot.trusted_devices;
        let disabled = self.transitioning() || self.confirming_trust_reset;
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .text_size(px(12.))
            .text_color(rgb(MUTED))
            .child(format!("{count} trusted device{}", if count == 1 { "" } else { "s" }))
            .child(
                div()
                    .id("forget-trusted")
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .track_focus(&self.trust_focus)
                    .key_context("TouchCursorControl")
                    .focus(|style| style.border_color(rgb(BLUE)))
                    .when(disabled, |this| this.opacity(0.45))
                    .when(!disabled, |this| {
                        this.cursor_pointer().hover(|style| style.text_color(rgb(ERROR)))
                    })
                    .child("Forget all")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.trust_focus.focus(window);
                        this.forget_trusted_devices(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &Activate, window, cx| {
                        this.forget_trusted_devices(window, cx);
                    })),
            )
    }

}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("touchcursor")
            .track_focus(&self.root_focus)
            .key_context("TouchCursor")
            .on_action(|_: &FocusNext, window, _| window.focus_next())
            .on_action(|_: &FocusPrevious, window, _| window.focus_prev())
            .on_action(|_: &CloseWindow, window, _| request_close(window))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .text_size(px(14.))
            .child(self.render_titlebar(window))
            .child(
                div()
                    .id("desktop-content")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p_6()
                    .gap_4()
                    .when_some(self.tray_error.as_ref(), |this, error| {
                        this.child(
                            div()
                                .w_full()
                                .p_3()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(0x683a42))
                                .bg(rgb(0x2b1d25))
                                .text_size(px(13.))
                                .line_height(px(19.))
                                .text_color(rgb(ERROR))
                                .child(error.clone()),
                        )
                    })
                    .child(self.render_status(cx))
                    .child(self.render_pairing(cx))
                    .when(
                        self.snapshot.trusted_devices > 0
                            || matches!(&self.snapshot.status, ServerStatus::Error(_)),
                        |this| this.child(self.render_trust_controls(cx)),
                    )
            )
    }
}

fn native_window_handle(window: &Window) -> HWND {
    let handle = HasWindowHandle::window_handle(window).expect("native GPUI window handle");
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        unreachable!("TouchCursor desktop requires Windows");
    };
    handle.hwnd.get() as _
}

fn request_close(window: &Window) {
    unsafe {
        PostMessageW(native_window_handle(window), WM_CLOSE, 0, 0);
    }
}

fn hide_native_window(handle: HWND) {
    // The close callback runs inside a GPUI update, so defer native callbacks.
    unsafe {
        ShowWindowAsync(handle, SW_HIDE);
    }
}

fn show_native_window(handle: HWND) {
    unsafe {
        let state = if IsIconic(handle) != 0 {
            SW_RESTORE
        } else {
            SW_SHOW
        };
        // SW_SHOW preserves placement, including an already-maximized window.
        ShowWindow(handle, state);
        SetForegroundWindow(handle);
    }
}

fn toggle_maximize(window: &Window) {
    if window.is_maximized() {
        // GPUI 0.2.2's Windows zoom API maximizes but does not restore.
        unsafe {
            ShowWindowAsync(native_window_handle(window), SW_RESTORE);
        }
    } else {
        window.zoom_window();
    }
}

fn drag_window(window: &Window) {
    // Post the native move command so its modal loop starts after GPUI dispatch.
    unsafe {
        ReleaseCapture();
        PostMessageW(
            native_window_handle(window),
            WM_SYSCOMMAND,
            (SC_MOVE | HTCAPTION) as usize,
            0,
        );
    }
}

fn caption_button(
    id: &'static str,
    symbol: &'static str,
    role: WindowControlArea,
    label: &'static str,
) -> Stateful<Div> {
    div()
        .id(id)
        // Keep native hit testing for caption semantics and snap layouts, but
        // handle clicks explicitly because GPUI captures the mouse-up event.
        .window_control_area(role)
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            match role {
                WindowControlArea::Min => window.minimize_window(),
                WindowControlArea::Max => toggle_maximize(window),
                WindowControlArea::Close => request_close(window),
                WindowControlArea::Drag => unreachable!("drag is not a caption button"),
            }
        })
        .flex()
        .items_center()
        .justify_center()
        .w(px(44.))
        .h_full()
        .text_size(px(20.))
        .text_color(rgb(MUTED))
        .hover(move |style| {
            style
                .bg(rgb(if role == WindowControlArea::Close {
                    0xb13c48
                } else {
                    BORDER
                }))
                .text_color(rgb(TEXT))
        })
        .tooltip(move |_, cx| cx.new(|_| Hint(label)).into())
        .child(symbol)
}

struct Hint(&'static str);

impl Render for Hint {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x424957))
            .bg(rgb(PANEL))
            .font_family("Segoe UI")
            .text_size(px(12.))
            .text_color(rgb(TEXT))
            .child(self.0)
    }
}
