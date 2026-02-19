#[tokio::main]
async fn main() {
    if let Err(err) = sublinear_dev::run_from_env().await {
        eprintln!("sublinear failed: {err:#}");
        std::process::exit(1);
    }
}
