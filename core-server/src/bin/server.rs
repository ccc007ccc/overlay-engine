#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Starting core-server...");

    // Setup Ctrl+C handler to kill managed processes
    tokio::spawn(async {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            println!("\n[Server] Ctrl+C received, shutting down managed monitors...");
            core_server::process_manager::kill_managed_processes();
            std::process::exit(0);
        }
    });

    core_server::process_manager::launch_and_manage_monitors();

    let result = core_server::server_task::run_server().await;

    // In case the server drops out of the loop naturally
    core_server::process_manager::kill_managed_processes();
    result
}
