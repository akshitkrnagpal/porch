#[tokio::main]
async fn main() -> anyhow::Result<()> {
    porch::cli::run().await
}
