use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn compile_windows_icon_resource(rc_relative: &str, icon_relative: &str, res_name: &str) {
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let rc_path = manifest_dir.join(rc_relative);
    let icon_path = manifest_dir.join(icon_relative);
    let res_path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join(res_name);

    println!("cargo:rerun-if-changed={}", rc_path.display());
    println!("cargo:rerun-if-changed={}", icon_path.display());

    let Some(rc_exe) = resolve_rc_exe() else {
        println!("cargo:warning=rc.exe not found; Windows icon resource will not be embedded");
        return;
    };

    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg(format!("/fo{}", res_path.display()))
        .arg(&rc_path)
        .current_dir(rc_path.parent().unwrap_or(Path::new(".")))
        .status();

    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg-bins={}", res_path.display());
        }
        Ok(status) => {
            println!(
                "cargo:warning=rc.exe failed with status {status}; Windows icon resource skipped"
            );
        }
        Err(err) => {
            println!(
                "cargo:warning=failed to run {}: {err}; Windows icon resource skipped",
                rc_exe.display()
            );
        }
    }
}

fn resolve_rc_exe() -> Option<PathBuf> {
    if let Ok(path) = env::var("RC_EXE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Some(program_files_x86) = env::var_os("ProgramFiles(x86)") {
        let bin = PathBuf::from(program_files_x86)
            .join("Windows Kits")
            .join("10")
            .join("bin");
        for version in ["10.0.26100.0", "10.0.22621.0", "10.0.19041.0"] {
            candidates.push(bin.join(version).join("x64").join("rc.exe"));
        }
        if let Ok(entries) = std::fs::read_dir(&bin) {
            for entry in entries.flatten() {
                candidates.push(entry.path().join("x64").join("rc.exe"));
            }
        }
    }

    candidates.into_iter().find(|path| path.is_file())
}
