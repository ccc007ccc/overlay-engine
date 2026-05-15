#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Starting core-server...");

    tokio::spawn(async {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            println!("\n[Server] Ctrl+C received, shutting down managed Desktop monitors...");
            core_server::process_manager::kill_managed_processes();
            std::process::exit(0);
        }
    });

    core_server::process_manager::load_monitor_catalog();

    let result = core_server::server_task::run_server().await;

    core_server::process_manager::kill_managed_processes();
    result
}
