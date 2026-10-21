use crate::trust::{self, TrustStore};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use rand::Rng;
use std::{
    future::IntoFuture,
    io,
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::{runtime::Builder, sync::{mpsc, watch}};

const PAGE: &str = include_str!("../static/index.html");
const ICON: &str = include_str!("../static/icon.svg");
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSnapshot {
    pub status: ServerStatus,
    pub devices: usize,
    pub trusted_devices: usize,
    pub url: Option<String>,
    pub pin: String,
}

pub struct ServerController {
    commands: Option<watch::Sender<bool>>,
    snapshots: watch::Receiver<ServerSnapshot>,
    requested_enabled: Arc<AtomicBool>,
    revocations: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl ServerController {
    pub fn new() -> Self {
        Self::with_store_path(SocketAddr::from(([0, 0, 0, 0], 8765)), trust::default_path())
    }

    fn with_store_path(address: SocketAddr, path: io::Result<PathBuf>) -> Self {
        let trust = path
            .map(|path| Arc::new(TrustStore::new(path)))
            .map_err(|error| format!("Could not locate the trusted-device store: {error}"));
        let (commands, receiver) = watch::channel(true);
        let (snapshots, subscription) = watch::channel(ServerSnapshot {
            status: ServerStatus::Starting,
            devices: 0,
            trusted_devices: 0,
            url: None,
            pin: next_pin(""),
        });
        let requested_enabled = Arc::new(AtomicBool::new(true));
        let revocations = Arc::new(AtomicU64::new(0));
        let worker_snapshots = snapshots.clone();
        let worker_enabled = requested_enabled.clone();
        let worker_revocations = revocations.clone();
        let worker = match thread::Builder::new()
            .name("touchcursor-server".into())
            .spawn(move || server_worker(address, receiver, worker_snapshots, worker_enabled, worker_revocations, trust))
        {
            Ok(worker) => Some(worker),
            Err(error) => {
                publish_phase(
                    &snapshots,
                    ServerStatus::Error(format!("Could not start the server thread: {error}")),
                    None,
                    None,
                );
                None
            }
        };
        Self {
            commands: Some(commands),
            snapshots: subscription,
            requested_enabled,
            revocations,
            worker,
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<ServerSnapshot> {
        self.snapshots.clone()
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.requested_enabled.store(enabled, Ordering::Release);
        if let Some(commands) = &self.commands {
            // Repeating `true` is also an explicit retry after an error.
            commands.send_replace(enabled);
        }
    }

    pub fn forget_trusted_devices(&self) {
        // Revoke input synchronously, before the worker can begin disk I/O or
        // teardown. This does not change the user's desired on/off state.
        self.revocations.fetch_add(1, Ordering::AcqRel);
        if let Some(commands) = &self.commands {
            commands.send_modify(|_| {});
        }
    }
}

impl Drop for ServerController {
    fn drop(&mut self) {
        self.requested_enabled.store(false, Ordering::Release);
        drop(self.commands.take());
        if let Some(worker) = self.worker.take() {
            // Blocking storage jobs may outlive a retiring runtime, but cannot
            // update its sockets or input state after the bounded shutdown.
            let _ = worker.join();
        }
    }
}

enum GenerationEnd {
    Disabled,
    Revoked,
    Shutdown,
    Failed(String),
}

fn server_worker(
    address: SocketAddr,
    mut commands: watch::Receiver<bool>,
    snapshots: watch::Sender<ServerSnapshot>,
    requested_enabled: Arc<AtomicBool>,
    revocations: Arc<AtomicU64>,
    trust: Result<Arc<TrustStore>, String>,
) {
    let mut wait_for_retry = false;
    let mut handled_revocation = 0;
    loop {
        let runtime = match Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                publish_phase(
                    &snapshots,
                    ServerStatus::Error(format!("Could not create the server runtime: {error}")),
                    None,
                    None,
                );
                return;
            }
        };
        let outcome = runtime.block_on(run_generation(
            address,
            &mut commands,
            &snapshots,
            &requested_enabled,
            &revocations,
            &mut handled_revocation,
            &trust,
            wait_for_retry,
        ));

        // Destroy detached HTTP/upgrade tasks and all DeviceSession guards
        // before another generation can publish counts or accept input.
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        wait_for_retry = matches!(&outcome, GenerationEnd::Failed(_));
        match outcome {
            GenerationEnd::Disabled | GenerationEnd::Revoked => {
                publish_phase(&snapshots, ServerStatus::Stopped, None, None);
            }
            GenerationEnd::Shutdown => {
                publish_phase(&snapshots, ServerStatus::Stopped, None, None);
                return;
            }
            GenerationEnd::Failed(error) => {
                publish_phase(&snapshots, ServerStatus::Error(error), None, None);
            }
        }
    }
}

async fn run_generation(
    address: SocketAddr,
    commands: &mut watch::Receiver<bool>,
    snapshots: &watch::Sender<ServerSnapshot>,
    requested_enabled: &Arc<AtomicBool>,
    revocations: &Arc<AtomicU64>,
    handled_revocation: &mut u64,
    trust: &Result<Arc<TrustStore>, String>,
    wait_for_retry: bool,
) -> GenerationEnd {
    if wait_for_retry && revocations.load(Ordering::Acquire) == *handled_revocation
        && commands.changed().await.is_err()
    {
        return GenerationEnd::Shutdown;
    }
    if commands.has_changed().is_err() {
        return GenerationEnd::Shutdown;
    }
    let store = match trust {
        Ok(store) => store,
        Err(error) => {
            *handled_revocation = revocations.load(Ordering::Acquire);
            return GenerationEnd::Failed(error.clone());
        }
    };
    let revocation = revocations.load(Ordering::Acquire);
    let mut fresh_pin = false;
    if revocation != *handled_revocation {
        publish_phase(snapshots, ServerStatus::Stopping, None, None);
        let store = store.clone();
        let result = storage_task(commands, move || store.forget_all()).await;
        *handled_revocation = revocation;
        match result {
            None => return GenerationEnd::Shutdown,
            Some(Err(error)) => {
                return GenerationEnd::Failed(format!("Could not forget trusted devices: {error}"));
            }
            Some(Ok(())) => {
                publish_trusted_devices(snapshots, 0);
                let pin = next_pin(&snapshots.borrow().pin);
                publish_phase(snapshots, ServerStatus::Stopped, None, Some(pin));
                fresh_pin = true;
            }
        }
    } else {
        let store = store.clone();
        match storage_task(commands, move || store.load()).await {
            None => return GenerationEnd::Shutdown,
            Some(Err(error)) => {
                return GenerationEnd::Failed(format!("Could not load trusted devices: {error}"));
            }
            Some(Ok(count)) => publish_trusted_devices(snapshots, count),
        }
    }

    let listener = loop {
        if commands.has_changed().is_err() {
            return GenerationEnd::Shutdown;
        }
        if revocations.load(Ordering::Acquire) != *handled_revocation {
            return GenerationEnd::Revoked;
        }
        if !*commands.borrow_and_update() {
            publish_phase(snapshots, ServerStatus::Stopped, None, None);
            if commands.changed().await.is_err() {
                return GenerationEnd::Shutdown;
            }
            continue;
        }

        let pin = if fresh_pin {
            fresh_pin = false;
            snapshots.borrow().pin.clone()
        } else {
            next_pin(&snapshots.borrow().pin)
        };
        publish_phase(snapshots, ServerStatus::Starting, None, Some(pin));
        match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => break listener,
            Err(error) => {
                publish_phase(
                    snapshots,
                    ServerStatus::Error(format!(
                        "Could not listen on {address}: {error}. Turn the server on again to retry."
                    )),
                    None,
                    None,
                );
                if commands.changed().await.is_err() {
                    return GenerationEnd::Shutdown;
                }
            }
        }
    };

    if commands.has_changed().is_err() {
        publish_phase(snapshots, ServerStatus::Stopping, None, None);
        return GenerationEnd::Shutdown;
    }
    if revocations.load(Ordering::Acquire) != *handled_revocation {
        publish_phase(snapshots, ServerStatus::Stopping, None, None);
        return GenerationEnd::Revoked;
    }
    if !*commands.borrow_and_update() {
        publish_phase(snapshots, ServerStatus::Stopping, None, None);
        return GenerationEnd::Disabled;
    }

    let active = Arc::new(AtomicBool::new(true));
    let (errors, mut storage_errors) = mpsc::unbounded_channel();
    let state = AppState {
        pin: snapshots.borrow().pin.as_bytes().try_into().expect("four-digit PIN"),
        pairing: Arc::new(Mutex::new(PairingGate::default())),
        snapshots: snapshots.clone(),
        active: active.clone(),
        requested_enabled: requested_enabled.clone(),
        revocations: revocations.clone(),
        revocation: *handled_revocation,
        trust: store.clone(),
        errors,
    };
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route(
            "/icon.svg",
            get(|| async { ([("content-type", "image/svg+xml")], ICON) }),
        )
        .route("/ws", get(websocket))
        .with_state(state);
    let url = listener.local_addr().ok().and_then(|bound| lan_url(bound.port()));
    publish_phase(snapshots, ServerStatus::Running, url, None);
    let server = axum::serve(listener, app).tcp_nodelay(true).into_future();
    tokio::pin!(server);
    let outcome = loop {
        tokio::select! {
            biased;
            changed = commands.changed() => {
                if changed.is_err() {
                    break GenerationEnd::Shutdown;
                }
                if revocations.load(Ordering::Acquire) != *handled_revocation {
                    break GenerationEnd::Revoked;
                }
                if !*commands.borrow_and_update() {
                    break GenerationEnd::Disabled;
                }
            }
            Some(error) = storage_errors.recv() => {
                break GenerationEnd::Failed(error);
            }
            result = &mut server => {
                let message = match result {
                    Ok(()) => "The server stopped unexpectedly. Turn it on again to retry.".into(),
                    Err(error) => format!("The server stopped: {error}. Turn it on again to retry."),
                };
                break GenerationEnd::Failed(message);
            }
        }
    };
    active.store(false, Ordering::Release);
    publish_phase(snapshots, ServerStatus::Stopping, None, None);
    outcome
}

async fn storage_task<T: Send + 'static>(
    commands: &mut watch::Receiver<bool>,
    operation: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Option<Result<T, String>> {
    let task = tokio::task::spawn_blocking(operation);
    tokio::pin!(task);
    loop {
        tokio::select! {
            biased;
            changed = commands.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            result = &mut task => {
                return Some(match result {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(error) => Err(format!("Trusted-device storage task failed: {error}")),
                });
            }
        }
    }
}

fn publish_trusted_devices(snapshots: &watch::Sender<ServerSnapshot>, count: usize) {
    snapshots.send_if_modified(|snapshot| {
        if snapshot.trusted_devices == count {
            false
        } else {
            snapshot.trusted_devices = count;
            true
        }
    });
}

fn publish_phase(
    snapshots: &watch::Sender<ServerSnapshot>,
    status: ServerStatus,
    url: Option<String>,
    pin: Option<String>,
) {
    let mut next = snapshots.borrow().clone();
    next.status = status;
    next.url = url;
    if let Some(pin) = pin {
        next.pin = pin;
    }
    if matches!(&next.status, ServerStatus::Starting | ServerStatus::Stopped | ServerStatus::Error(_)) {
        debug_assert_eq!(next.devices, 0, "sessions must finish before a new phase");
        next.devices = 0;
    }
    snapshots.send_if_modified(|current| {
        if *current == next {
            false
        } else {
            *current = next;
            true
        }
    });
}

fn next_pin(previous: &str) -> String {
    loop {
        let pin = format!("{:04}", rand::thread_rng().gen_range(0..10_000u32));
        if pin != previous {
            return pin;
        }
    }
}

fn lan_url(port: u16) -> Option<String> {
    let local = UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("8.8.8.8:80")?;
            socket.local_addr()
        })
        .ok()?;
    if local.ip().is_loopback() || local.ip().is_unspecified() {
        return None;
    }
    Some(format!("http://{}:{port}", local.ip()))
}

#[derive(Clone)]
struct AppState {
    pin: [u8; 4],
    pairing: Arc<Mutex<PairingGate>>,
    snapshots: watch::Sender<ServerSnapshot>,
    active: Arc<AtomicBool>,
    requested_enabled: Arc<AtomicBool>,
    revocations: Arc<AtomicU64>,
    revocation: u64,
    trust: Arc<TrustStore>,
    errors: mpsc::UnboundedSender<String>,
}

impl AppState {
    fn accepts_input(&self) -> bool {
        self.active.load(Ordering::Acquire)
            && self.requested_enabled.load(Ordering::Acquire)
            && self.revocations.load(Ordering::Acquire) == self.revocation
    }
}

struct DeviceSession {
    snapshots: watch::Sender<ServerSnapshot>,
}

impl DeviceSession {
    fn authenticated(state: &AppState) -> Option<Self> {
        if !state.accepts_input() {
            return None;
        }
        let added = state.snapshots.send_if_modified(|snapshot| {
            if snapshot.status != ServerStatus::Running {
                return false;
            }
            snapshot.devices += 1;
            true
        });
        added.then(|| Self { snapshots: state.snapshots.clone() })
    }
}

impl Drop for DeviceSession {
    fn drop(&mut self) {
        self.snapshots.send_modify(|snapshot| {
            debug_assert!(snapshot.devices > 0, "authenticated session counted once");
            snapshot.devices -= 1;
        });
    }
}

#[derive(Default)]
struct PairingGate {
    failures: u8,
    locked_until: Option<Instant>,
}

async fn websocket(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.max_message_size(96)
        .max_frame_size(96)
        .write_buffer_size(0)
        .max_write_buffer_size(128)
        .on_upgrade(move |socket| connection(socket, state))
}

async fn connection(mut socket: WebSocket, state: AppState) {
    if !authenticate(&mut socket, &state).await {
        return;
    }
    let Some(_device) = DeviceSession::authenticated(&state) else {
        return;
    };

    loop {
        match socket.recv().await {
            Some(Ok(Message::Binary(data))) => {
                if !state.accepts_input() || !handle_input(&data) {
                    reject(&mut socket, "error").await;
                    return;
                }
            }
            Some(Ok(Message::Ping(data))) => {
                if socket.send(Message::Pong(data)).await.is_err() {
                    return;
                }
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(_))) | None => return,
            _ => {
                reject(&mut socket, "error").await;
                return;
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Authentication<'a> {
    Pair(&'a [u8]),
    Trust(&'a str),
}

fn authentication(text: &str) -> Option<Authentication<'_>> {
    if let Some(pin) = text.strip_prefix("pair:") {
        (pin.len() == 4 && pin.bytes().all(|byte| byte.is_ascii_digit()))
            .then_some(Authentication::Pair(pin.as_bytes()))
    } else {
        text.strip_prefix("trust:").map(Authentication::Trust)
    }
}

async fn authenticate(socket: &mut WebSocket, state: &AppState) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let message = match tokio::time::timeout_at(deadline, socket.recv()).await {
            Ok(message) => message,
            Err(_) => {
                reject(socket, "error").await;
                return false;
            }
        };
        match message {
            Some(Ok(Message::Text(text))) => {
                if !state.accepts_input() {
                    reject(socket, "error").await;
                    return false;
                }
                match authentication(&text) {
                    Some(Authentication::Pair(pin)) => {
                        let status = check_pin(state, pin);
                        if status != "ready" {
                            reject(socket, status).await;
                            return false;
                        }
                        let store = state.trust.clone();
                        let active = state.active.clone();
                        let requested_enabled = state.requested_enabled.clone();
                        let revocations = state.revocations.clone();
                        let revocation = state.revocation;
                        let result = tokio::task::spawn_blocking(move || {
                            store.issue_token(|| {
                                active.load(Ordering::Acquire)
                                    && requested_enabled.load(Ordering::Acquire)
                                    && revocations.load(Ordering::Acquire) == revocation
                            })
                        }).await;
                        let token = match result {
                            Ok(Ok(Some(token))) => token,
                            Ok(Ok(None)) => {
                                reject(socket, "error").await;
                                return false;
                            }
                            Ok(Err(error)) => {
                                storage_failed(socket, state, error.to_string()).await;
                                return false;
                            }
                            Err(error) => {
                                storage_failed(socket, state, format!("Storage task failed: {error}")).await;
                                return false;
                            }
                        };
                        state.snapshots.send_if_modified(|snapshot| {
                            if state.accepts_input() && snapshot.status == ServerStatus::Running {
                                let count = state.trust.count();
                                if snapshot.trusted_devices != count {
                                    snapshot.trusted_devices = count;
                                    return true;
                                }
                            }
                            false
                        });
                        if !state.accepts_input() {
                            reject(socket, "error").await;
                            return false;
                        }
                        if socket.send(Message::Text(format!("trusted:{token}"))).await.is_err() {
                            return false;
                        }
                    }
                    Some(Authentication::Trust(token)) => {
                        let token = token.to_owned();
                        let store = state.trust.clone();
                        match tokio::task::spawn_blocking(move || store.contains(&token)).await {
                            Ok(Ok(true)) => {}
                            Ok(Ok(false)) => {
                                reject(socket, "untrusted").await;
                                return false;
                            }
                            Ok(Err(error)) => {
                                storage_failed(socket, state, error.to_string()).await;
                                return false;
                            }
                            Err(error) => {
                                storage_failed(socket, state, format!("Storage task failed: {error}")).await;
                                return false;
                            }
                        }
                    }
                    None => {
                        reject(socket, "error").await;
                        return false;
                    }
                }
                if !state.accepts_input() {
                    reject(socket, "error").await;
                    return false;
                }
                return socket.send(Message::Text("ready".into())).await.is_ok();
            }
            Some(Ok(Message::Ping(data))) => {
                if socket.send(Message::Pong(data)).await.is_err() {
                    return false;
                }
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(_))) | None => return false,
            _ => {
                reject(socket, "error").await;
                return false;
            }
        }
    }
}

async fn storage_failed(socket: &mut WebSocket, state: &AppState, error: String) {
    state.active.store(false, Ordering::Release);
    reject(socket, "storage").await;
    let _ = state.errors.send(format!("Could not save or read trusted devices: {error}. Turn the server on again to retry."));
}

fn check_pin(state: &AppState, supplied: &[u8]) -> &'static str {
    let mut gate = state.pairing.lock().expect("pairing gate mutex");
    let now = Instant::now();
    if let Some(until) = gate.locked_until {
        if now < until {
            return "locked";
        }
        gate.locked_until = None;
        gate.failures = 0;
    }
    if constant_time_eq(supplied, &state.pin) {
        gate.failures = 0;
        return "ready";
    }
    gate.failures += 1;
    if gate.failures == 5 {
        gate.locked_until = Some(now + Duration::from_secs(30));
        "locked"
    } else {
        "pin"
    }
}

async fn reject(socket: &mut WebSocket, status: &'static str) {
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        let _ = socket.send(Message::Text(status.into())).await;
        let _ = socket.send(Message::Close(None)).await;
    })
    .await;
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(*a.get(i).unwrap_or(&0) ^ *b.get(i).unwrap_or(&0));
    }
    diff == 0
}

fn handle_input(data: &[u8]) -> bool {
    match data {
        [1, x0, x1, x2, x3, y0, y1, y2, y3] => {
            let dx = i32::from_le_bytes([*x0, *x1, *x2, *x3]);
            let dy = i32::from_le_bytes([*y0, *y1, *y2, *y3]);
            (-2500..=2500).contains(&dx) && (-2500..=2500).contains(&dy) && move_cursor(dx, dy)
        }
        [2, d0, d1, d2, d3] => {
            let delta = i32::from_le_bytes([*d0, *d1, *d2, *d3]);
            (-6000..=6000).contains(&delta) && scroll(delta)
        }
        [3, button @ 0..=1] => click(*button),
        _ => false,
    }
}

#[cfg(windows)]
fn mouse_input(flags: u32, dx: i32, dy: i32, data: u32) -> windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_0, INPUT_MOUSE, MOUSEINPUT};
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

#[cfg(windows)]
fn send_mouse(flags: u32, dx: i32, dy: i32, data: u32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};
    let input = mouse_input(flags, dx, dy, data);
    unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 1 }
}

#[cfg(windows)]
fn move_cursor(dx: i32, dy: i32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE;
    send_mouse(MOUSEEVENTF_MOVE, dx, dy, 0)
}

#[cfg(not(windows))]
fn move_cursor(_: i32, _: i32) -> bool { false }

#[cfg(windows)]
fn click(button: u8) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP};
    let (down, up) = match button {
        0 => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        1 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        _ => return false,
    };
    let inputs: [INPUT; 2] = [mouse_input(down, 0, 0, 0), mouse_input(up, 0, 0, 0)];
    unsafe { SendInput(2, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) == 2 }
}

#[cfg(not(windows))]
fn click(_: u8) -> bool { false }

#[cfg(windows)]
fn scroll(delta: i32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_WHEEL;
    send_mouse(MOUSEEVENTF_WHEEL, 0, 0, delta as u32)
}

#[cfg(not(windows))]
fn scroll(_: i32) -> bool { false }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::test_support::TestDirectory;
    use std::{
        fs,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
    };

    fn state() -> (AppState, TestDirectory) {
        let directory = TestDirectory::new();
        let trust = Arc::new(TrustStore::new(directory.store_path()));
        trust.load().unwrap();
        let (snapshots, _) = watch::channel(ServerSnapshot {
            status: ServerStatus::Running,
            devices: 0,
            trusted_devices: 0,
            url: None,
            pin: "0042".into(),
        });
        let (errors, _) = mpsc::unbounded_channel();
        (AppState {
            pin: *b"0042",
            pairing: Arc::new(Mutex::new(PairingGate::default())),
            snapshots,
            active: Arc::new(AtomicBool::new(true)),
            requested_enabled: Arc::new(AtomicBool::new(true)),
            revocations: Arc::new(AtomicU64::new(0)),
            revocation: 0,
            trust,
            errors,
        }, directory)
    }

    #[test]
    fn leading_zero_pin_authenticates_and_resets_failed_attempts() {
        let (state, _directory) = state();
        let Some(Authentication::Pair(pin)) = authentication("pair:0042") else {
            panic!("leading zero PIN must be preserved");
        };
        for _ in 0..2 {
            for _ in 0..4 {
                assert_eq!(check_pin(&state, b"0043"), "pin");
            }
            assert_eq!(check_pin(&state, pin), "ready");
        }
        for malformed in ["0042", "pair:042", "pair:00042", "pair:１２３４", "pair:00x2"] {
            assert_eq!(authentication(malformed), None);
        }
    }

    #[test]
    fn pairing_lockout_blocks_correct_pin_until_expiry() {
        let (state, _directory) = state();
        for _ in 0..4 {
            assert_eq!(check_pin(&state, b"9999"), "pin");
        }
        assert_eq!(check_pin(&state, b"9999"), "locked");
        assert_eq!(check_pin(&state, b"0042"), "locked");
        state.pairing.lock().unwrap().locked_until =
            Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(check_pin(&state, b"0042"), "ready");
        assert_eq!(check_pin(&state, b"9999"), "pin");
    }

    #[test]
    fn device_guards_count_once_and_revocation_blocks_input_before_teardown() {
        let (state, _directory) = state();
        let first = DeviceSession::authenticated(&state).unwrap();
        let second = DeviceSession::authenticated(&state).unwrap();
        assert_eq!(state.snapshots.borrow().devices, 2);
        drop(first);
        assert_eq!(state.snapshots.borrow().devices, 1);
        state.revocations.fetch_add(1, Ordering::AcqRel);
        assert!(!state.accepts_input());
        assert!(DeviceSession::authenticated(&state).is_none());
        state.active.store(false, Ordering::Release);
        publish_phase(&state.snapshots, ServerStatus::Stopping, None, None);
        assert!(DeviceSession::authenticated(&state).is_none());
        drop(second);
        assert_eq!(state.snapshots.borrow().devices, 0);
        publish_phase(&state.snapshots, ServerStatus::Stopped, None, None);
        assert!(DeviceSession::authenticated(&state).is_none());
    }

    fn wait_for(
        receiver: &mut watch::Receiver<ServerSnapshot>,
        predicate: impl FnMut(&ServerSnapshot) -> bool,
    ) -> ServerSnapshot {
        Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), receiver.wait_for(predicate))
                    .await
                    .expect("server state transition timed out")
                    .expect("server worker exited")
                    .clone()
            })
    }

    fn websocket_client(address: SocketAddr) -> TcpStream {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        write!(
            stream,
            "GET /ws HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        )
        .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
            assert!(headers.len() < 4096);
        }
        assert!(headers.starts_with(b"HTTP/1.1 101"));
        stream
    }

    fn send_text(stream: &mut TcpStream, text: &str) {
        assert!(text.len() <= 96);
        let mask = [7, 13, 19, 29];
        let mut frame = Vec::with_capacity(text.len() + 6);
        frame.extend_from_slice(&[0x81, 0x80 | text.len() as u8]);
        frame.extend_from_slice(&mask);
        frame.extend(text.bytes().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));
        stream.write_all(&frame).unwrap();
    }

    fn pair(stream: &mut TcpStream, pin: &str) -> String {
        send_text(stream, &format!("pair:{pin}"));
        let issued = read_status(stream);
        let token = issued.strip_prefix("trusted:").expect("committed credential before ready");
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert_eq!(read_status(stream), "ready");
        token.to_owned()
    }

    fn authenticate_trust(stream: &mut TcpStream, token: &str) {
        send_text(stream, &format!("trust:{token}"));
        assert_eq!(read_status(stream), "ready");
    }

    fn read_status(stream: &mut TcpStream) -> String {
        let mut header = [0; 2];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(header[0], 0x81);
        assert!(header[1] <= 96);
        let mut body = vec![0; usize::from(header[1])];
        stream.read_exact(&mut body).unwrap();
        String::from_utf8(body).unwrap()
    }

    fn assert_closed(stream: &mut TcpStream) {
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Err(error) if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
            result => panic!("socket should be closed after stop: {result:?}"),
        }
    }

    fn assert_rejection(stream: &mut TcpStream, status: &str) {
        assert_eq!(read_status(stream), status);
        let mut close = [0; 2];
        stream.read_exact(&mut close).unwrap();
        assert_eq!(close, [0x88, 0]);
        assert_closed(stream);
    }

    fn unused_address() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    fn start_server(directory: &TestDirectory) -> (ServerController, watch::Receiver<ServerSnapshot>, SocketAddr, ServerSnapshot) {
        let address = unused_address();
        let server = ServerController::with_store_path(address, Ok(directory.store_path()));
        let mut snapshots = server.subscribe();
        let running = wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running);
        (server, snapshots, address, running)
    }

    #[test]
    fn lifecycle_retries_bind_and_closes_all_sessions_before_restart_and_drop() {
        let directory = TestDirectory::new();
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        let server = ServerController::with_store_path(address, Ok(directory.store_path()));
        let mut snapshots = server.subscribe();
        wait_for(&mut snapshots, |snapshot| matches!(snapshot.status, ServerStatus::Error(_)));
        drop(reservation);
        server.set_enabled(true);
        let running = wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running);
        assert_eq!(running.pin.len(), 4);
        assert!(running.pin.bytes().all(|byte| byte.is_ascii_digit()));

        let mut unauthenticated = websocket_client(address);
        let mut rejected = websocket_client(address);
        let wrong_pin = if running.pin == "0000" { "0001" } else { "0000" };
        send_text(&mut rejected, &format!("pair:{wrong_pin}"));
        assert_rejection(&mut rejected, "pin");
        assert_eq!(snapshots.borrow().devices, 0);
        drop(rejected);

        let mut abandoned = websocket_client(address);
        pair(&mut abandoned, &running.pin);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 1);
        drop(abandoned);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 0);

        let mut authenticated = websocket_client(address);
        pair(&mut authenticated, &running.pin);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 1);
        server.set_enabled(false);
        let stopped = wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Stopped);
        assert_eq!(stopped.devices, 0);
        assert_eq!(stopped.trusted_devices, 2);
        assert!(stopped.url.is_none());
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
        assert_closed(&mut unauthenticated);
        assert_closed(&mut authenticated);

        server.set_enabled(true);
        let restarted = wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running);
        assert_ne!(running.pin, restarted.pin);
        assert_eq!(restarted.devices, 0);
        assert_eq!(restarted.trusted_devices, 2);
        server.set_enabled(false);
        server.set_enabled(true);
        server.set_enabled(false);
        wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Stopped);
        server.set_enabled(true);
        wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running);
        let mut pending_at_drop = websocket_client(address);
        let started = Instant::now();
        drop(server);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
        assert_closed(&mut pending_at_drop);
    }

    #[test]
    fn trusted_credential_survives_server_toggle_and_controller_restart() {
        let directory = TestDirectory::new();
        let (server, mut snapshots, address, running) = start_server(&directory);
        let mut paired = websocket_client(address);
        let token = pair(&mut paired, &running.pin);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 1 && snapshot.trusted_devices == 1);
        server.set_enabled(false);
        wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Stopped);
        assert_closed(&mut paired);
        server.set_enabled(true);
        let restarted = wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running);
        assert_ne!(running.pin, restarted.pin);
        let mut trusted = websocket_client(address);
        authenticate_trust(&mut trusted, &token);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 1 && snapshot.trusted_devices == 1);
        drop(server);
        assert_closed(&mut trusted);

        let server = ServerController::with_store_path(address, Ok(directory.store_path()));
        let mut snapshots = server.subscribe();
        let restored = wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running);
        assert_eq!(restored.trusted_devices, 1);
        let mut trusted = websocket_client(address);
        authenticate_trust(&mut trusted, &token);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 1 && snapshot.trusted_devices == 1);
        drop(server);
        assert_closed(&mut trusted);
    }

    #[test]
    fn invalid_trust_does_not_spend_pin_attempts_and_valid_trust_bypasses_pin_lockout() {
        let directory = TestDirectory::new();
        let (_server, mut snapshots, address, running) = start_server(&directory);
        for token in ["0".repeat(64), "A".repeat(64), "short".into(), "f".repeat(65)] {
            let mut client = websocket_client(address);
            send_text(&mut client, &format!("trust:{token}"));
            assert_rejection(&mut client, "untrusted");
        }
        let mut legacy = websocket_client(address);
        send_text(&mut legacy, &running.pin);
        assert_rejection(&mut legacy, "error");
        let wrong = if running.pin == "0000" { "0001" } else { "0000" };
        for _ in 0..4 {
            let mut client = websocket_client(address);
            send_text(&mut client, &format!("pair:{wrong}"));
            assert_rejection(&mut client, "pin");
        }
        let mut client = websocket_client(address);
        let token = pair(&mut client, &running.pin);
        drop(client);
        for attempt in 0..5 {
            let mut client = websocket_client(address);
            send_text(&mut client, &format!("pair:{wrong}"));
            assert_rejection(&mut client, if attempt == 4 { "locked" } else { "pin" });
        }
        let mut locked = websocket_client(address);
        send_text(&mut locked, &format!("pair:{}", running.pin));
        assert_rejection(&mut locked, "locked");
        let mut trusted = websocket_client(address);
        authenticate_trust(&mut trusted, &token);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 1 && snapshot.trusted_devices == 1);
    }

    #[test]
    fn revoke_closes_every_session_rotates_pin_and_remains_revoked_after_offline_restart() {
        let directory = TestDirectory::new();
        let (server, mut snapshots, address, running) = start_server(&directory);
        let mut paired = websocket_client(address);
        let token = pair(&mut paired, &running.pin);
        let mut trusted = websocket_client(address);
        authenticate_trust(&mut trusted, &token);
        let mut pending = websocket_client(address);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 2);
        server.forget_trusted_devices();
        let revoked = wait_for(&mut snapshots, |snapshot| {
            snapshot.status == ServerStatus::Running && snapshot.trusted_devices == 0 && snapshot.pin != running.pin
        });
        assert_eq!(revoked.devices, 0);
        assert_closed(&mut paired);
        assert_closed(&mut trusted);
        assert_closed(&mut pending);
        let mut removed = websocket_client(address);
        send_text(&mut removed, &format!("trust:{token}"));
        assert_rejection(&mut removed, "untrusted");
        let mut old_pin = websocket_client(address);
        send_text(&mut old_pin, &format!("pair:{}", running.pin));
        assert_rejection(&mut old_pin, "pin");

        let mut new_pairing = websocket_client(address);
        let new_token = pair(&mut new_pairing, &revoked.pin);
        wait_for(&mut snapshots, |snapshot| snapshot.trusted_devices == 1 && snapshot.devices == 1);
        server.set_enabled(false);
        let stopped = wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Stopped);
        assert_closed(&mut new_pairing);
        server.forget_trusted_devices();
        let forgotten = wait_for(&mut snapshots, |snapshot| {
            snapshot.status == ServerStatus::Stopped && snapshot.trusted_devices == 0 && snapshot.pin != stopped.pin
        });
        assert_eq!(forgotten.devices, 0);
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
        drop(server);

        let server = ServerController::with_store_path(address, Ok(directory.store_path()));
        let mut snapshots = server.subscribe();
        wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running && snapshot.trusted_devices == 0);
        let mut removed = websocket_client(address);
        send_text(&mut removed, &format!("trust:{new_token}"));
        assert_rejection(&mut removed, "untrusted");
    }

    #[test]
    fn pairing_storage_failure_never_issues_a_token_or_ready_and_surfaces_controller_error() {
        let directory = TestDirectory::new();
        let (server, mut snapshots, address, running) = start_server(&directory);
        fs::create_dir(directory.store_path()).unwrap();
        let mut client = websocket_client(address);
        send_text(&mut client, &format!("pair:{}", running.pin));
        assert_rejection(&mut client, "storage");
        let failed = wait_for(&mut snapshots, |snapshot| matches!(snapshot.status, ServerStatus::Error(_)));
        assert_eq!(failed.devices, 0);
        assert_eq!(failed.trusted_devices, 0);
        fs::remove_dir(directory.store_path()).unwrap();
        server.forget_trusted_devices();
        let recovered = wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running);
        let mut client = websocket_client(address);
        pair(&mut client, &recovered.pin);
        wait_for(&mut snapshots, |snapshot| snapshot.trusted_devices == 1 && snapshot.devices == 1);
    }

    #[test]
    fn corrupt_store_fails_closed_and_can_be_explicitly_reset_while_off_with_zero_known_devices() {
        let directory = TestDirectory::new();
        fs::write(directory.store_path(), b"corrupt\n").unwrap();
        let address = unused_address();
        let server = ServerController::with_store_path(address, Ok(directory.store_path()));
        let mut snapshots = server.subscribe();
        let failed = wait_for(&mut snapshots, |snapshot| matches!(snapshot.status, ServerStatus::Error(_)));
        assert_eq!(failed.trusted_devices, 0);
        assert_eq!(fs::read(directory.store_path()).unwrap(), b"corrupt\n");
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
        server.set_enabled(false);
        server.forget_trusted_devices();
        wait_for(&mut snapshots, |snapshot| {
            snapshot.status == ServerStatus::Stopped && snapshot.trusted_devices == 0 && snapshot.pin != failed.pin
        });
        assert!(fs::read(directory.store_path()).unwrap().is_empty());
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
        server.set_enabled(true);
        wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running);
    }

    #[test]
    fn revoke_write_failure_closes_sessions_without_claiming_success_and_can_be_retried() {
        let directory = TestDirectory::new();
        let (server, mut snapshots, address, running) = start_server(&directory);
        let mut client = websocket_client(address);
        let token = pair(&mut client, &running.pin);
        wait_for(&mut snapshots, |snapshot| snapshot.devices == 1 && snapshot.trusted_devices == 1);
        let original = fs::read(directory.store_path()).unwrap();
        fs::remove_file(directory.store_path()).unwrap();
        fs::create_dir(directory.store_path()).unwrap();
        server.forget_trusted_devices();
        let failed = wait_for(&mut snapshots, |snapshot| matches!(snapshot.status, ServerStatus::Error(_)));
        assert_eq!(failed.trusted_devices, 1);
        assert_eq!(failed.devices, 0);
        assert_closed(&mut client);
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
        fs::remove_dir(directory.store_path()).unwrap();
        fs::write(directory.store_path(), original).unwrap();
        server.forget_trusted_devices();
        wait_for(&mut snapshots, |snapshot| snapshot.status == ServerStatus::Running && snapshot.trusted_devices == 0);
        let mut removed = websocket_client(address);
        send_text(&mut removed, &format!("trust:{token}"));
        assert_rejection(&mut removed, "untrusted");
    }
}
