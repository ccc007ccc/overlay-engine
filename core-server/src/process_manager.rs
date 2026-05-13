use std::path::PathBuf;
use std::process::Child;
use std::sync::Mutex;
use std::fs;

lazy_static::lazy_static! {
    static ref MANAGED_PROCESSES: Mutex<Vec<Child>> = Mutex::new(Vec::new());
}

pub fn launch_and_manage_monitors() {
    let config_path = PathBuf::from("config.ini");
    if !config_path.exists() {
        println!("[Process Manager] No config.ini found, skipping auto-launch.");
        return;
    }

    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[Process Manager] Failed to read config.ini: {}", e);
            return;
        }
    };

    let mut procs = MANAGED_PROCESSES.lock().unwrap();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(cmd_str) = line.strip_prefix("Launch=") {
            let cmd_str = cmd_str.trim();
            println!("[Process Manager] Launching monitor: {}", cmd_str);

            // Allow launching UWP apps via shell (e.g. explorer.exe shell:AppsFolder/...)
            // or normal exes.
            let child_res = if cmd_str.starts_with("explorer.exe") {
                let args: Vec<&str> = cmd_str.split_whitespace().collect();
                if args.len() > 1 {
                    std::process::Command::new(args[0])
                        .args(&args[1..])
                        .spawn()
                } else {
                    std::process::Command::new(cmd_str).spawn()
                }
            } else {
                std::process::Command::new(cmd_str).spawn()
            };

            match child_res {
                Ok(child) => {
                    println!("[Process Manager] Successfully started PID: {}", child.id());
                    procs.push(child);
                }
                Err(e) => {
                    eprintln!("[Process Manager] Failed to start {}: {}", cmd_str, e);
                }
            }
        }
    }
}

pub fn kill_managed_processes() {
    let mut procs = MANAGED_PROCESSES.lock().unwrap();
    if procs.is_empty() {
        return;
    }
    println!("[Process Manager] Shutting down {} managed processes...", procs.len());
    for child in procs.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    procs.clear();
}