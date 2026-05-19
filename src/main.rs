#[tokio::main]
async fn main() {
    if let Err(error) = pocker::run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
