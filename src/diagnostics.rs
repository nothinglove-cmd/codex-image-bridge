use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::{image, install, model_config, network};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningProcess {
    pub process_id: u32,
    pub executable: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SupportBundle {
    schema_version: u32,
    application_version: &'static str,
    generated_at_unix: u64,
    operating_system: String,
    architecture: &'static str,
    status_exit_code: Option<i32>,
    install: Option<install::StatusReport>,
    install_error: Option<String>,
    model: Option<RedactedModelStatus>,
    model_error: Option<String>,
    network: Option<network::NetworkReport>,
    network_error: Option<String>,
    running_codex_processes: Vec<RunningProcess>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RedactedModelStatus {
    codex_home: PathBuf,
    config_path: PathBuf,
    auth_path: PathBuf,
    config_sha256: Option<String>,
    auth_sha256: Option<String>,
    provider_id: String,
    normalized_server_url: Option<String>,
    image_model: String,
    image_generation_enabled: bool,
    static_header_count: usize,
    environment_header_count: usize,
    api_key_configured: bool,
    managed_backup_present: bool,
}

pub fn create_support_bundle(output: Option<&Path>) -> Result<PathBuf> {
    let generated_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let (install, install_error) = match install::status_report() {
        Ok(report) => (Some(report), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let (model, model_error) = match model_config::load_settings() {
        Ok(settings) => {
            let normalized_server_url = if settings.server_url.is_empty() {
                None
            } else {
                model_config::normalize_server_url(&settings.server_url).ok()
            };
            let status = RedactedModelStatus {
                codex_home: settings.codex_home.clone(),
                config_path: settings.config_path.clone(),
                auth_path: settings.auth_path.clone(),
                config_sha256: file_sha256(&settings.config_path),
                auth_sha256: file_sha256(&settings.auth_path),
                provider_id: settings.provider_id.clone(),
                normalized_server_url,
                image_model: settings.image_model.clone(),
                image_generation_enabled: settings.image_model_enabled,
                static_header_count: settings.static_headers.len(),
                environment_header_count: settings.env_headers.len(),
                api_key_configured: !settings.api_key.is_empty(),
                managed_backup_present: settings.managed,
            };
            (Some(status), None)
        }
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let (network, network_error) = match network::diagnose() {
        Ok(report) => (Some(report), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let bundle = SupportBundle {
        schema_version: 2,
        application_version: env!("CARGO_PKG_VERSION"),
        generated_at_unix,
        operating_system: operating_system_version(),
        architecture: std::env::consts::ARCH,
        status_exit_code: install.as_ref().map(|report| match report.health {
            install::InstallationHealth::Healthy => 0,
            install::InstallationHealth::NotInstalled => 10,
            install::InstallationHealth::Broken => 11,
        }),
        install,
        install_error,
        model,
        model_error,
        network,
        network_error,
        running_codex_processes: running_codex_processes().unwrap_or_default(),
    };
    let output = output
        .map(Path::to_owned)
        .unwrap_or_else(|| default_bundle_path(generated_at_unix));
    let bytes = serde_json::to_vec_pretty(&bundle)?;
    write_new_file(&output, &bytes)?;
    Ok(output)
}

#[cfg(windows)]
fn operating_system_version() -> String {
    use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};

    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let Ok(key) = key else {
        return "Windows (version unavailable)".to_owned();
    };
    let product = key
        .get_value::<String, _>("ProductName")
        .unwrap_or_else(|_| "Windows".to_owned());
    let display = key
        .get_value::<String, _>("DisplayVersion")
        .unwrap_or_default();
    let build = key
        .get_value::<String, _>("CurrentBuildNumber")
        .unwrap_or_default();
    let update_revision = key.get_value::<u32, _>("UBR").ok();
    let build = match update_revision {
        Some(revision) if !build.is_empty() => format!("{build}.{revision}"),
        _ => build,
    };
    [product, display, build]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(not(windows))]
fn operating_system_version() -> String {
    std::env::consts::OS.to_owned()
}

pub fn launch_codex() -> Result<()> {
    let report = install::status_report()?;
    let real_cli = report
        .real_cli
        .context("official Codex CLI was not found; start Codex Desktop manually")?;
    let mut command = Command::new(real_cli);
    command.arg("app");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
        .spawn()
        .context("failed to launch Codex; start Codex Desktop manually")?;
    Ok(())
}

fn default_bundle_path(generated_at_unix: u64) -> PathBuf {
    let directory = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|path| path.join("Desktop"))
        .filter(|path| path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    directory.join(format!(
        "CodexImageFix-Diagnostics-{generated_at_unix}-{}.json",
        std::process::id()
    ))
}

fn file_sha256(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| image::sha256(&bytes))
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("diagnostic output has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create diagnostic package at {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(windows)]
pub fn running_codex_processes() -> Result<Vec<RunningProcess>> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("failed to enumerate processes");
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes = Vec::new();
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let length = entry
            .szExeFile
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(entry.szExeFile.len());
        let executable = String::from_utf16_lossy(&entry.szExeFile[..length]);
        if is_codex_process(&executable) {
            processes.push(RunningProcess {
                process_id: entry.th32ProcessID,
                executable,
            });
        }
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    processes.sort_by_key(|process| process.process_id);
    Ok(processes)
}

#[cfg(not(windows))]
pub fn running_codex_processes() -> Result<Vec<RunningProcess>> {
    Ok(Vec::new())
}

fn is_codex_process(executable: &str) -> bool {
    let executable = executable.to_ascii_lowercase();
    matches!(
        executable.as_str(),
        "codex.exe" | "codex-desktop.exe" | "codexdesktop.exe" | "codex++.exe" | "chatgpt.exe"
    )
}

pub fn require_bundle_is_redacted(path: &Path) -> Result<()> {
    let text = fs::read_to_string(path)?;
    for forbidden in ["OPENAI_API_KEY", "http_headers", "env_http_headers"] {
        if text.contains(forbidden) {
            bail!("diagnostic package contains forbidden secret field {forbidden}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_codex_related_processes() {
        assert!(is_codex_process("Codex.exe"));
        assert!(is_codex_process("ChatGPT.exe"));
        assert!(!is_codex_process("Code.exe"));
    }
}
