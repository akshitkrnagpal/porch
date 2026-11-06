//! Native menu/tray entrypoint. The OS event loop runs on the process main thread.
fn main() {
    if let Err(error) = porch::tray::run() {
        eprintln!("Porch: {error:#}");
        std::process::exit(1);
    }
}
