use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref MANAGED_PROCESSES: Mutex<Vec<Child>> = Mutex::new(Vec::new());
}

pub fn launch_and_manage_monitors() {
    let Some((config_path, config_dir)) = find_config_path() else {
        println!("[Process Manager] No config.ini found, skipping auto-launch.");
        return;
    };

    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[Process Manager] Failed to read {}: {}",
                config_path.display(),
                e
            );
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
            if cmd_str.is_empty() {
                continue;
            }
            println!("[Process Manager] Launching monitor: {}", cmd_str);

            let child_res = build_launch_command(cmd_str, &config_dir).spawn();

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

fn find_config_path() -> Option<(PathBuf, PathBuf)> {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let config_path = exe_dir.join("config.ini");
            if config_path.exists() {
                return Some((config_path, exe_dir.to_path_buf()));
            }
        }
    }

    let config_path = PathBuf::from("config.ini");
    if config_path.exists() {
        let config_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        return Some((config_path, config_dir));
    }

    None
}

fn build_launch_command(cmd_str: &str, config_dir: &Path) -> Command {
    let parts: Vec<&str> = cmd_str.split_whitespace().collect();
    let exe = parts.first().copied().unwrap_or(cmd_str);
    let args = if parts.is_empty() {
        &[][..]
    } else {
        &parts[1..]
    };

    let mut cmd = if exe.eq_ignore_ascii_case("explorer.exe") {
        let mut cmd = Command::new(exe);
        cmd.args(args);
        cmd
    } else {
        let exe_path = PathBuf::from(exe);
        let resolved = if exe_path.is_absolute() {
            exe_path
        } else {
            config_dir.join(exe_path)
        };
        let mut cmd = Command::new(resolved);
        cmd.args(args);
        cmd.current_dir(config_dir);
        cmd
    };

    if exe.eq_ignore_ascii_case("explorer.exe") {
        cmd.current_dir(config_dir);
    }
    cmd
}

pub fn kill_managed_processes() {
    let mut procs = MANAGED_PROCESSES.lock().unwrap();
    if procs.is_empty() {
        return;
    }
    println!(
        "[Process Manager] Shutting down {} managed processes...",
        procs.len()
    );
    for child in procs.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    procs.clear();
}
