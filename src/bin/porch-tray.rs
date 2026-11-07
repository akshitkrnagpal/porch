//! Native menu/tray entrypoint. The OS event loop runs on the process main thread.
// A GUI program on Windows, so starting the menu opens no console window.
#![cfg_attr(windows, windows_subsystem = "windows")]
fn main() {
    if let Err(error) = porch::tray::run() {
        eprintln!("Porch: {error:#}");
        std::process::exit(1);
    }
}
