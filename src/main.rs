#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let notice = porch::update::Notice::start(&args);
    let result = porch::cli::run().await;
    if let Some(notice) = notice {
        notice.finish();
    }
    match result {
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
