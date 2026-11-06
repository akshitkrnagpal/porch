#[tokio::main]
async fn main() {
    match porch::cli::run().await {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("porch: {error:#}");
            std::process::exit(
                error
                    .downcast_ref::<porch::integrations::ExternalExit>()
                    .map(|e| e.0)
                    .unwrap_or(1),
            );
        }
    }
}
