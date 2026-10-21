use std::{
    cell::Cell,
    io,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::mpsc::sync_channel,
    thread::{self, JoinHandle},
};

use tokio::sync::{mpsc, watch};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{GetLastError, SetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP,
                NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETFOCUS, NIM_SETVERSION, NINF_KEY, NIN_SELECT,
                NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, NOTIFYICON_VERSION_4,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
                DestroyMenu, DestroyWindow, DispatchMessageW, EndMenu, GetCursorPos, GetMessageW,
                GetSystemMetrics, GetWindowLongPtrW, IsIconic, IsWindowVisible, LoadImageW,
                ModifyMenuW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
                SendMessageW, SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenuEx,
                TranslateMessage, UnregisterClassW, CREATESTRUCTW, GWLP_USERDATA, HICON, HMENU,
                IMAGE_ICON, LR_DEFAULTCOLOR, MF_BYPOSITION, MF_CHECKED, MF_GRAYED, MF_SEPARATOR,
                MF_STRING, MSG, SM_CXSMICON, SM_CYSMICON, TPM_NONOTIFY, TPM_RETURNCMD,
                TPM_RIGHTBUTTON, TPM_WORKAREA, WM_APP, WM_CANCELMODE, WM_CLOSE, WM_CONTEXTMENU,
                WM_DESTROY, WM_LBUTTONDBLCLK, WM_NCCREATE, WM_NCDESTROY, WM_NULL, WNDCLASSW,
                WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
};

use crate::server::{ServerSnapshot, ServerStatus};

const ICON_ID: u32 = 1;
const CALLBACK_MESSAGE: u32 = WM_APP + 1;
const SHUTDOWN_MESSAGE: u32 = WM_APP + 2;
const MENU_WINDOW: usize = 1;
const MENU_TOGGLE_SERVER: usize = 2;
const MENU_QUIT: usize = 3;
const KEY_SELECT: u32 = NIN_SELECT | NINF_KEY;

#[derive(Clone, Copy, Debug)]
pub enum TrayAction {
    ShowWindow,
    HideWindow,
    ToggleServer,
    Quit,
}

pub struct Tray {
    window: usize,
    worker: Option<JoinHandle<()>>,
}

impl Tray {
    pub fn new(
        target: HWND,
        status: watch::Receiver<ServerSnapshot>,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<TrayAction>)> {
        if target.is_null() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "The tray needs a desktop window"));
        }

        let target = target as usize;
        let (actions, receiver) = mpsc::unbounded_channel();
        let (initialized, ready) = sync_channel(1);
        let worker = thread::Builder::new()
            .name("touchcursor-tray".into())
            .spawn(move || {
                let state = match TrayState::new(target as HWND, status, actions) {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = initialized.send(Err(error));
                        return;
                    }
                };
                if initialized.send(Ok(state.window.get() as usize)).is_err() {
                    return;
                }

                unsafe {
                    let mut message: MSG = zeroed();
                    loop {
                        let result = GetMessageW(&mut message, null_mut(), 0, 0);
                        if result <= 0 {
                            if result < 0 {
                                let _ = state.actions.send(TrayAction::ShowWindow);
                            }
                            break;
                        }
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            })?;

        match ready.recv() {
            Ok(Ok(window)) => Ok((Self { window, worker: Some(worker) }, receiver)),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err(io::Error::other("The tray thread exited before initialization completed"))
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            if !worker.is_finished() {
                // Sent messages reach the tray thread even inside TrackPopupMenuEx.
                // Its handler cancels the modal menu before the synchronous join.
                unsafe { SendMessageW(self.window as HWND, SHUTDOWN_MESSAGE, 0, 0) };
            }
            let _ = worker.join();
        }
    }
}

struct TrayState {
    target: HWND,
    status: watch::Receiver<ServerSnapshot>,
    actions: mpsc::UnboundedSender<TrayAction>,
    instance: HINSTANCE,
    class_registered: bool,
    icon: HICON,
    menu: HMENU,
    window: Cell<HWND>,
    notification: Cell<NOTIFYICONDATAW>,
    icon_added: Cell<bool>,
    menu_active: Cell<bool>,
    stopping: Cell<bool>,
    taskbar_created: u32,
}

impl TrayState {
    fn new(
        target: HWND,
        status: watch::Receiver<ServerSnapshot>,
        actions: mpsc::UnboundedSender<TrayAction>,
    ) -> io::Result<Box<Self>> {
        unsafe {
            let mut state = Box::new(Self {
                target,
                status,
                actions,
                instance: null_mut(),
                class_registered: false,
                icon: null_mut(),
                menu: null_mut(),
                window: Cell::new(null_mut()),
                notification: Cell::new(zeroed()),
                icon_added: Cell::new(false),
                menu_active: Cell::new(false),
                stopping: Cell::new(false),
                taskbar_created: 0,
            });
            state.instance = GetModuleHandleW(null());
            if state.instance.is_null() {
                return Err(api_error("GetModuleHandleW"));
            }
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: state.instance,
                lpszClassName: w!("TouchCursor.Tray"),
                ..zeroed()
            };
            if RegisterClassW(&class) == 0 {
                return Err(api_error("RegisterClassW"));
            }
            state.class_registered = true;

            // Without LR_SHARED, this thread owns the resource icon and destroys it.
            state.icon = LoadImageW(
                state.instance,
                ICON_ID as *const u16,
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            ) as HICON;
            if state.icon.is_null() {
                return Err(api_error("LoadImageW(resource 1)"));
            }
            state.menu = CreatePopupMenu();
            if state.menu.is_null() {
                return Err(api_error("CreatePopupMenu"));
            }
            for (flags, command, label) in [
                (MF_STRING, MENU_WINDOW, w!("Show window")),
                (MF_STRING, MENU_TOGGLE_SERVER, w!("Toggle server")),
                (MF_SEPARATOR, 0, null()),
                (MF_STRING, MENU_QUIT, w!("Quit app")),
            ] {
                if AppendMenuW(state.menu, flags, command, label) == 0 {
                    return Err(api_error("AppendMenuW"));
                }
            }
            state.taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
            if state.taskbar_created == 0 {
                return Err(api_error("RegisterWindowMessageW(TaskbarCreated)"));
            }

            // A hidden top-level window, rather than HWND_MESSAGE, receives the
            // Explorer restart broadcast. The boxed userdata stays at one address.
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("TouchCursor.Tray"),
                w!("TouchCursor tray"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                state.instance,
                (&*state as *const Self).cast(),
            );
            if window.is_null() {
                return Err(api_error("CreateWindowExW"));
            }
            let mut notification: NOTIFYICONDATAW = zeroed();
            notification.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            notification.hWnd = window;
            notification.uID = ICON_ID;
            notification.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
            notification.uCallbackMessage = CALLBACK_MESSAGE;
            notification.hIcon = state.icon;
            notification.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            for (slot, character) in notification.szTip.iter_mut().zip("TouchCursor".encode_utf16()) {
                *slot = character;
            }
            state.notification.set(notification);
            state.add_icon()?;
            Ok(state)
        }
    }

    fn add_icon(&self) -> io::Result<()> {
        unsafe {
            let notification = self.notification.get();
            SetLastError(0);
            if Shell_NotifyIconW(NIM_ADD, &notification) == 0 {
                return Err(api_error("Shell_NotifyIconW(NIM_ADD)"));
            }
            self.icon_added.set(true);
            SetLastError(0);
            if Shell_NotifyIconW(NIM_SETVERSION, &notification) == 0 {
                let error = api_error("Shell_NotifyIconW(NIM_SETVERSION)");
                self.remove_icon();
                return Err(error);
            }
            Ok(())
        }
    }

    fn remove_icon(&self) {
        if self.icon_added.replace(false) {
            unsafe { Shell_NotifyIconW(NIM_DELETE, &self.notification.get()) };
        }
    }

    fn shutdown(&self) {
        if !self.stopping.replace(true) {
            unsafe {
                if self.menu_active.get() {
                    EndMenu();
                    PostMessageW(self.window.get(), WM_CANCELMODE, 0, 0);
                }
                self.remove_icon();
                PostQuitMessage(0);
            }
        }
    }

    fn show_menu(&self, mut point: POINT) {
        if self.stopping.get() || self.menu_active.replace(true) {
            return;
        }

        // Never retain a watch read guard across the modal menu loop: server
        // snapshot writers must remain free to run while this menu is open.
        let toggle_flags = {
            let snapshot = self.status.borrow();
            match &snapshot.status {
                ServerStatus::Running => MF_CHECKED,
                ServerStatus::Starting | ServerStatus::Stopping => MF_GRAYED,
                _ => 0,
            }
        };
        unsafe {
            let (window_label, window_action) =
                if IsWindowVisible(self.target) != 0 && IsIconic(self.target) == 0 {
                    (w!("Hide window"), TrayAction::HideWindow)
                } else {
                    (w!("Show window"), TrayAction::ShowWindow)
                };
            if ModifyMenuW(self.menu, 0, MF_BYPOSITION | MF_STRING, MENU_WINDOW, window_label) == 0
                || ModifyMenuW(
                    self.menu,
                    1,
                    MF_BYPOSITION | MF_STRING | toggle_flags,
                    MENU_TOGGLE_SERVER,
                    w!("Toggle server"),
                ) == 0
            {
                self.menu_active.set(false);
                return;
            }

            if point.x == -1 && point.y == -1 {
                let identifier = NOTIFYICONIDENTIFIER {
                    cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
                    hWnd: self.window.get(),
                    uID: ICON_ID,
                    guidItem: zeroed(),
                };
                let mut rect: RECT = zeroed();
                if Shell_NotifyIconGetRect(&identifier, &mut rect) >= 0 {
                    point = POINT { x: rect.left, y: rect.bottom };
                } else {
                    GetCursorPos(&mut point);
                }
            }

            // Foreground ownership makes dismissal by clicking elsewhere work.
            SetForegroundWindow(self.window.get());
            if self.stopping.get() {
                self.menu_active.set(false);
                return;
            }
            let command = TrackPopupMenuEx(
                self.menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_WORKAREA,
                point.x,
                point.y,
                self.window.get(),
                null(),
            ) as usize;
            self.menu_active.set(false);
            PostMessageW(self.window.get(), WM_NULL, 0, 0);
            if !self.stopping.get() {
                let action = match command {
                    MENU_WINDOW => Some(window_action),
                    MENU_TOGGLE_SERVER => Some(TrayAction::ToggleServer),
                    MENU_QUIT => Some(TrayAction::Quit),
                    _ => None,
                };
                if let Some(action) = action {
                    let _ = self.actions.send(action);
                }
                Shell_NotifyIconW(NIM_SETFOCUS, &self.notification.get());
            }
        }
    }
}

impl Drop for TrayState {
    fn drop(&mut self) {
        self.stopping.set(true);
        self.remove_icon();
        unsafe {
            let window = self.window.replace(null_mut());
            if !window.is_null() {
                // Detach userdata before destroying its owner, so destruction
                // callbacks cannot borrow a state that is already being dropped.
                SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                DestroyWindow(window);
            }
            if !self.menu.is_null() {
                DestroyMenu(self.menu);
            }
            if !self.icon.is_null() {
                DestroyIcon(self.icon);
            }
            if self.class_registered {
                UnregisterClassW(w!("TouchCursor.Tray"), self.instance);
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let creation = &*(lparam as *const CREATESTRUCTW);
        let state = creation.lpCreateParams as *const TrayState;
        SetLastError(0);
        if SetWindowLongPtrW(window, GWLP_USERDATA, state as isize) == 0 && GetLastError() != 0 {
            return 0;
        }
        (*state).window.set(window);
        return 1;
    }

    let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *const TrayState;
    if state.is_null() {
        return DefWindowProcW(window, message, wparam, lparam);
    }
    let state = &*state;
    match message {
        SHUTDOWN_MESSAGE | WM_CLOSE | WM_DESTROY => {
            state.shutdown();
            0
        }
        WM_NCDESTROY => {
            state.remove_icon();
            state.window.set(null_mut());
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            DefWindowProcW(window, message, wparam, lparam)
        }
        CALLBACK_MESSAGE => {
            if !state.stopping.get() && ((lparam as usize >> 16) & 0xffff) == ICON_ID as usize {
                match (lparam as u32) & 0xffff {
                    WM_CONTEXTMENU => state.show_menu(screen_point(wparam)),
                    WM_LBUTTONDBLCLK | KEY_SELECT => {
                        let _ = state.actions.send(TrayAction::ShowWindow);
                    }
                    _ => {}
                }
            }
            0
        }
        WM_CONTEXTMENU => {
            state.show_menu(screen_point(lparam as usize));
            0
        }
        message if message == state.taskbar_created => {
            if !state.stopping.get() {
                state.icon_added.set(false);
                if state.add_icon().is_err() {
                    let _ = state.actions.send(TrayAction::ShowWindow);
                    state.shutdown();
                }
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

fn screen_point(value: usize) -> POINT {
    POINT {
        x: value as u16 as i16 as i32,
        y: (value >> 16) as u16 as i16 as i32,
    }
}

fn api_error(operation: &str) -> io::Error {
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(0) {
        io::Error::other(format!("{operation} failed"))
    } else {
        io::Error::new(error.kind(), format!("{operation}: {error}"))
    }
}
