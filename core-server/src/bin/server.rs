#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Starting core-server...");
    core_server::server_task::run_server().await
}
