use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;

use crate::ipc::protocol::{
    DesktopWindowMode, MonitorKind, MonitorStartPolicy, MonitorTypeEntry,
    DESKTOP_WINDOW_FLAG_CLICK_THROUGH, DESKTOP_WINDOW_MODE_BORDERED,
    DESKTOP_WINDOW_MODE_BORDERLESS, DESKTOP_WINDOW_MODE_BORDERLESS_FULLSCREEN,
};

lazy_static::lazy_static! {
    static ref MANAGED_PROCESSES: Mutex<Vec<Child>> = Mutex::new(Vec::new());
    static ref MONITOR_CATALOG: Mutex<Option<MonitorCatalog>> = Mutex::new(None);
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MonitorCatalog {
    pub desktop_window: Option<DesktopWindowCapability>,
    pub game_bar: Option<GameBarCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopWindowCapability {
    pub path: PathBuf,
    pub max_instances_per_app: u32,
    pub window_modes: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameBarCapability {
    pub available: bool,
    pub max_instances: u32,
    pub start_policy: MonitorStartPolicy,
}

#[derive(Debug, Clone, Copy)]
pub struct DesktopWindowLaunchOptions {
    pub request_id: u32,
    pub owner_app_id: u32,
    pub target_canvas_id: u32,
    pub mode: DesktopWindowMode,
    pub flags: u32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl MonitorCatalog {
    pub fn to_monitor_type_entries(&self) -> Vec<MonitorTypeEntry> {
        let mut entries = Vec::new();
        if let Some(desktop) = &self.desktop_window {
            entries.push(MonitorTypeEntry {
                kind: MonitorKind::DesktopWindow,
                available: true,
                start_policy: MonitorStartPolicy::CoreOnDemand,
                core_startable: true,
                core_managed: true,
                max_instances: desktop.max_instances_per_app,
                window_modes: desktop.window_modes,
                flags: desktop.flags,
            });
        }
        if let Some(game_bar) = &self.game_bar {
            entries.push(MonitorTypeEntry {
                kind: MonitorKind::GameBar,
                available: game_bar.available,
                start_policy: game_bar.start_policy,
                core_startable: false,
                core_managed: false,
                max_instances: game_bar.max_instances,
                window_modes: 0,
                flags: 0,
            });
        }
        entries
    }
}

pub fn load_monitor_catalog() -> MonitorCatalog {
    let catalog = match find_config_path() {
        Some((config_path, config_dir)) => match fs::read_to_string(&config_path) {
            Ok(content) => {
                let catalog = parse_monitor_catalog(&content, &config_dir);
                println!(
                    "[Process Manager] Loaded monitor catalog from {}: desktop={}, gamebar={}",
                    config_path.display(),
                    catalog.desktop_window.is_some(),
                    catalog.game_bar.is_some()
                );
                catalog
            }
            Err(e) => {
                eprintln!(
                    "[Process Manager] Failed to read {}: {}",
                    config_path.display(),
                    e
                );
                MonitorCatalog::default()
            }
        },
        None => {
            println!("[Process Manager] No config.ini found; monitor catalog is empty.");
            MonitorCatalog::default()
        }
    };

    *MONITOR_CATALOG.lock().unwrap() = Some(catalog.clone());
    catalog
}

pub fn get_monitor_catalog() -> MonitorCatalog {
    if let Some(catalog) = MONITOR_CATALOG.lock().unwrap().clone() {
        return catalog;
    }
    load_monitor_catalog()
}

pub fn start_desktop_window_monitor(options: DesktopWindowLaunchOptions) -> anyhow::Result<u32> {
    let catalog = get_monitor_catalog();
    let desktop = catalog
        .desktop_window
        .ok_or_else(|| anyhow::anyhow!("Desktop Window Monitor is not installed"))?;

    let mut cmd = Command::new(&desktop.path);
    cmd.arg("--request-id")
        .arg(options.request_id.to_string())
        .arg("--owner-app-id")
        .arg(options.owner_app_id.to_string())
        .arg("--target-canvas-id")
        .arg(options.target_canvas_id.to_string())
        .arg("--mode")
        .arg(options.mode.cli_value())
        .arg("--click-through")
        .arg(if options.flags & DESKTOP_WINDOW_FLAG_CLICK_THROUGH != 0 {
            "1"
        } else {
            "0"
        })
        .arg("--x")
        .arg(options.x.to_string())
        .arg("--y")
        .arg(options.y.to_string())
        .arg("--w")
        .arg(options.w.to_string())
        .arg("--h")
        .arg(options.h.to_string());

    if let Some(dir) = desktop.path.parent() {
        cmd.current_dir(dir);
    }

    reap_exited_processes();
    let child = cmd.spawn()?;
    let pid = child.id();
    MANAGED_PROCESSES.lock().unwrap().push(child);
    println!("[Process Manager] Started Desktop Window Monitor launcher PID: {pid}");
    Ok(pid)
}

pub fn kill_managed_processes() {
    let mut procs = MANAGED_PROCESSES.lock().unwrap();
    if procs.is_empty() {
        return;
    }
    println!(
        "[Process Manager] Shutting down {} managed Desktop monitor processes...",
        procs.len()
    );
    for child in procs.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    procs.clear();
}

fn reap_exited_processes() {
    let mut procs = MANAGED_PROCESSES.lock().unwrap();
    procs.retain_mut(|child| match child.try_wait() {
        Ok(Some(_)) => false,
        Ok(None) => true,
        Err(_) => false,
    });
}

fn parse_monitor_catalog(content: &str, config_dir: &Path) -> MonitorCatalog {
    let mut desktop_path = None;
    let mut desktop_max = 16;
    let mut desktop_modes = DESKTOP_WINDOW_MODE_BORDERED
        | DESKTOP_WINDOW_MODE_BORDERLESS
        | DESKTOP_WINDOW_MODE_BORDERLESS_FULLSCREEN;
    let mut desktop_flags = 0;
    let mut legacy_desktop_launch = None;

    let mut game_bar_available = false;
    let mut game_bar_max = 1;
    let mut game_bar_policy = MonitorStartPolicy::UserManual;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        let value = raw_value.trim();

        if key.eq_ignore_ascii_case("Launch") {
            if value
                .to_ascii_lowercase()
                .contains("desktop-window-monitor")
            {
                legacy_desktop_launch =
                    first_command_token(value).map(|token| resolve_config_path(token, config_dir));
                eprintln!(
                    "[Process Manager] Deprecated Launch={} ignored; Desktop monitor is now app-started on demand.",
                    value
                );
            }
            continue;
        }

        if key.eq_ignore_ascii_case("Monitor.DesktopWindow.Path") {
            if !value.is_empty() {
                desktop_path = Some(resolve_config_path(value, config_dir));
            }
        } else if key.eq_ignore_ascii_case("Monitor.DesktopWindow.MaxInstancesPerApp") {
            desktop_max = value.parse::<u32>().unwrap_or(desktop_max).max(1);
        } else if key.eq_ignore_ascii_case("Monitor.DesktopWindow.WindowModes") {
            desktop_modes = parse_window_modes(value);
        } else if key.eq_ignore_ascii_case("Monitor.DesktopWindow.Flags") {
            desktop_flags = parse_desktop_flags(value);
        } else if key.eq_ignore_ascii_case("Monitor.GameBar.Available") {
            game_bar_available = parse_bool(value);
        } else if key.eq_ignore_ascii_case("Monitor.GameBar.MaxInstances") {
            game_bar_max = value.parse::<u32>().unwrap_or(game_bar_max).max(1);
        } else if key.eq_ignore_ascii_case("Monitor.GameBar.StartPolicy") {
            game_bar_policy = if value.eq_ignore_ascii_case("core-on-demand") {
                MonitorStartPolicy::CoreOnDemand
            } else {
                MonitorStartPolicy::UserManual
            };
        }
    }

    if desktop_path.is_none() {
        desktop_path = legacy_desktop_launch;
    }

    MonitorCatalog {
        desktop_window: desktop_path.map(|path| DesktopWindowCapability {
            path,
            max_instances_per_app: desktop_max,
            window_modes: desktop_modes,
            flags: desktop_flags,
        }),
        game_bar: game_bar_available.then_some(GameBarCapability {
            available: true,
            max_instances: game_bar_max,
            start_policy: game_bar_policy,
        }),
    }
}

fn resolve_config_path(value: &str, config_dir: &Path) -> PathBuf {
    let path = PathBuf::from(value.trim_matches('"'));
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

fn first_command_token(value: &str) -> Option<&str> {
    value.split_whitespace().next().filter(|s| !s.is_empty())
}

fn parse_window_modes(value: &str) -> u32 {
    let mut modes = 0;
    for part in value.split(',').map(|p| p.trim()) {
        if part.eq_ignore_ascii_case("bordered") {
            modes |= DESKTOP_WINDOW_MODE_BORDERED;
        } else if part.eq_ignore_ascii_case("borderless") {
            modes |= DESKTOP_WINDOW_MODE_BORDERLESS;
        } else if part.eq_ignore_ascii_case("borderless-fullscreen")
            || part.eq_ignore_ascii_case("fullscreen")
        {
            modes |= DESKTOP_WINDOW_MODE_BORDERLESS_FULLSCREEN;
        }
    }
    modes
}

fn parse_desktop_flags(value: &str) -> u32 {
    let mut flags = 0;
    for part in value.split(',').map(|p| p.trim()) {
        if part.eq_ignore_ascii_case("click-through") || part.eq_ignore_ascii_case("clickthrough") {
            flags |= DESKTOP_WINDOW_FLAG_CLICK_THROUGH;
        }
    }
    flags
}

fn parse_bool(value: &str) -> bool {
    value.eq_ignore_ascii_case("true") || value == "1" || value.eq_ignore_ascii_case("yes")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_catalog_without_launching_legacy_launch() {
        let catalog = parse_monitor_catalog(
            "Launch=desktop-window-monitor.exe\nMonitor.GameBar.Available=true\n",
            Path::new("C:/overlay-engine"),
        );
        let desktop = catalog.desktop_window.unwrap();
        assert!(desktop.path.ends_with("desktop-window-monitor.exe"));
        assert_eq!(desktop.max_instances_per_app, 16);
        assert_eq!(
            catalog.game_bar.unwrap().start_policy,
            MonitorStartPolicy::UserManual
        );
    }

    #[test]
    fn parses_desktop_capability_catalog() {
        let catalog = parse_monitor_catalog(
            "Monitor.DesktopWindow.Path=bin/desktop-window-monitor.exe\n\
             Monitor.DesktopWindow.MaxInstancesPerApp=3\n\
             Monitor.DesktopWindow.WindowModes=bordered,borderless-fullscreen\n\
             Monitor.DesktopWindow.Flags=click-through\n",
            Path::new("C:/overlay-engine"),
        );
        let desktop = catalog.desktop_window.unwrap();
        assert!(desktop.path.ends_with("bin/desktop-window-monitor.exe"));
        assert_eq!(desktop.max_instances_per_app, 3);
        assert_eq!(
            desktop.window_modes,
            DESKTOP_WINDOW_MODE_BORDERED | DESKTOP_WINDOW_MODE_BORDERLESS_FULLSCREEN
        );
        assert_eq!(desktop.flags, DESKTOP_WINDOW_FLAG_CLICK_THROUGH);
    }
}
