use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SUCCESS, WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::server_task::PIPE_NAME;

const CORE_REGISTRY_SUBKEY: &str = "Software\\overlay-engine\\Core";
const CORE_EXE_NAME: &str = "core-server.exe";
const CONNECT_RETRY: Duration = Duration::from_millis(50);
const CORE_START_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreInstallLocation {
    pub install_dir: PathBuf,
    pub core_exe: PathBuf,
    pub version: Option<String>,
}

pub async fn connect_to_core_or_start() -> anyhow::Result<NamedPipeClient> {
    let first_error = match try_open_core_pipe() {
        Ok(client) => return Ok(client),
        Err(e) if is_pipe_busy(&e) => return wait_for_core_pipe(CORE_START_TIMEOUT).await,
        Err(e) if is_pipe_missing(&e) => e,
        Err(e) => return Err(e).context("open overlay-core pipe failed"),
    };

    let location = read_core_install_location().with_context(|| {
        format!(
            "Core pipe {PIPE_NAME} is not available ({first_error}); installed Core location is missing or invalid"
        )
    })?;
    spawn_installed_core(&location)?;
    wait_for_core_pipe(CORE_START_TIMEOUT)
        .await
        .with_context(|| {
            format!(
                "started {}, but Core pipe {PIPE_NAME} did not become ready within {:?}",
                location.core_exe.display(),
                CORE_START_TIMEOUT
            )
        })
}

pub fn read_core_install_location() -> anyhow::Result<CoreInstallLocation> {
    let install_dir_raw = read_hkcu_reg_sz(CORE_REGISTRY_SUBKEY, "InstallDir")?;
    let core_exe_raw = read_hkcu_reg_sz(CORE_REGISTRY_SUBKEY, "CoreExe")?;
    let version = read_hkcu_reg_sz(CORE_REGISTRY_SUBKEY, "Version").ok();

    validate_core_install_location(install_dir_raw, core_exe_raw, version)
}

fn validate_core_install_location(
    install_dir_raw: String,
    core_exe_raw: String,
    version: Option<String>,
) -> anyhow::Result<CoreInstallLocation> {
    let install_dir = PathBuf::from(non_empty_registry_value("InstallDir", &install_dir_raw)?);
    let core_exe = PathBuf::from(non_empty_registry_value("CoreExe", &core_exe_raw)?);

    if !install_dir.is_absolute() {
        anyhow::bail!(
            "Core InstallDir must be absolute: {}",
            install_dir.display()
        );
    }
    if !core_exe.is_absolute() {
        anyhow::bail!("CoreExe must be absolute: {}", core_exe.display());
    }
    if !core_exe
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(CORE_EXE_NAME))
    {
        anyhow::bail!(
            "CoreExe must point to {CORE_EXE_NAME}: {}",
            core_exe.display()
        );
    }

    let install_dir = fs::canonicalize(&install_dir)
        .with_context(|| format!("canonicalize Core InstallDir {}", install_dir.display()))?;
    let core_exe = fs::canonicalize(&core_exe)
        .with_context(|| format!("canonicalize CoreExe {}", core_exe.display()))?;

    if !path_has_case_insensitive_prefix(&core_exe, &install_dir) {
        anyhow::bail!(
            "CoreExe must be inside InstallDir: CoreExe={} InstallDir={}",
            core_exe.display(),
            install_dir.display()
        );
    }

    Ok(CoreInstallLocation {
        install_dir,
        core_exe,
        version: version.and_then(|v| {
            let v = v.trim().to_string();
            (!v.is_empty()).then_some(v)
        }),
    })
}

fn non_empty_registry_value<'a>(name: &str, value: &'a str) -> anyhow::Result<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Core registry value {name} is empty");
    }
    Ok(trimmed)
}

fn spawn_installed_core(location: &CoreInstallLocation) -> anyhow::Result<()> {
    let mut cmd = Command::new(&location.core_exe);
    cmd.current_dir(&location.install_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }

    cmd.spawn()
        .with_context(|| format!("spawn installed Core {}", location.core_exe.display()))?;
    Ok(())
}

async fn wait_for_core_pipe(timeout: Duration) -> anyhow::Result<NamedPipeClient> {
    let deadline = Instant::now() + timeout;
    loop {
        match try_open_core_pipe() {
            Ok(client) => return Ok(client),
            Err(e) if (is_pipe_busy(&e) || is_pipe_missing(&e)) && Instant::now() < deadline => {
                tokio::time::sleep(CONNECT_RETRY).await;
            }
            Err(e) => return Err(e).context("open overlay-core pipe failed while waiting"),
        }
    }
}

fn try_open_core_pipe() -> std::io::Result<NamedPipeClient> {
    ClientOptions::new().open(PIPE_NAME)
}

fn is_pipe_busy(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32)
}

fn is_pipe_missing(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == ERROR_FILE_NOT_FOUND.0 as i32 || code == ERROR_PATH_NOT_FOUND.0 as i32
    )
}

fn read_hkcu_reg_sz(subkey: &str, value_name: &str) -> anyhow::Result<String> {
    let subkey_w = wide_null(subkey);
    let value_w = wide_null(value_name);
    let mut value_type = REG_VALUE_TYPE(0);
    let mut byte_len = 0u32;

    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            None,
            Some(&mut byte_len),
        )
    };
    ensure_success(status, "query Core registry value length")?;

    if byte_len == 0 {
        return Ok(String::new());
    }

    let mut data = vec![0u16; byte_len.div_ceil(2) as usize];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            Some(data.as_mut_ptr() as *mut c_void),
            Some(&mut byte_len),
        )
    };
    ensure_success(status, "read Core registry value")?;

    let utf16_len = (byte_len / 2) as usize;
    let nul = data[..utf16_len]
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(utf16_len);
    String::from_utf16(&data[..nul]).context("decode Core registry UTF-16 value")
}

fn ensure_success(status: WIN32_ERROR, operation: &str) -> anyhow::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        anyhow::bail!("{operation} failed with WIN32_ERROR({})", status.0)
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn path_has_case_insensitive_prefix(path: &Path, root: &Path) -> bool {
    let path = normalized_path_string(path);
    let root = normalized_path_string(root);
    path == root || path.starts_with(&(root + "\\"))
}

fn normalized_path_string(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_core_exe_outside_install_dir() {
        let result = validate_core_install_location(
            r"C:\Windows".to_string(),
            r"C:\Windows\System32\core-server.exe".to_string(),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_wrong_core_file_name() {
        let result = validate_core_install_location(
            r"C:\Windows".to_string(),
            r"C:\Windows\System32\cmd.exe".to_string(),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn case_insensitive_prefix_respects_directory_boundary() {
        assert!(path_has_case_insensitive_prefix(
            Path::new(r"C:\Apps\Overlay\core-server.exe"),
            Path::new(r"c:\apps\overlay")
        ));
        assert!(!path_has_case_insensitive_prefix(
            Path::new(r"C:\Apps\Overlay2\core-server.exe"),
            Path::new(r"c:\apps\overlay")
        ));
    }
}
