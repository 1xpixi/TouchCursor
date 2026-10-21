#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod server;
mod trust;
#[cfg(target_os = "windows")]
mod desktop;
#[cfg(target_os = "windows")]
mod tray;

#[cfg(target_os = "windows")]
fn main() {
    desktop::run(server::ServerController::new());
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("TouchCursor requires Windows to control the mouse.");
    std::process::exit(1);
}
