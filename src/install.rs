use std::{
    ffi::OsStr,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::image::{sanitize, sha256};

const STATE_VERSION: u32 = 1;
const TRANSACTION_VERSION: u32 = 1;
const FIX_DIRECTORY_NAME: &str = "CodexImageDisplayFix";
const TRANSACTION_FILE_NAME: &str = "install-transaction.json";
const ALIAS_FILE_NAME: &str = "features.code_mode_host=true.cmd";
const ALIAS_MARKER: &str = "CodexImageDisplayFix";
const GUARDIAN_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const GUARDIAN_RUN_VALUE: &str = "ComideaCodexImageBridge";
const REAL_CLI_ENV: &str = "CODEX_IMAGE_PROXY_REAL_CLI";
pub(crate) const HIDE_CONSOLE_ENV: &str = "CODEX_IMAGE_PROXY_HIDE_CONSOLE";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallState {
    version: u32,
    installed_at_unix: u64,
    real_cli: PathBuf,
    launcher: PathBuf,
    proxy: PathBuf,
    alias: PathBuf,
    installed_alias_sha256: String,
    installed_proxy_sha256: String,
    #[serde(default)]
    installed_launcher_sha256: String,
    original_codex_cli_path: RegistryBackup,
    original_alias: FileBackup,
    #[serde(default)]
    original_guardian_run: RegistryBackup,
    #[serde(default)]
    installed_guardian_command: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryBackup {
    existed: bool,
    value_type: Option<u32>,
    bytes_base64: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileBackup {
    existed: bool,
    bytes_base64: Option<String>,
    sha256: Option<String>,
    attributes: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum InstallTransactionPhase {
    Applying,
    Committed,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallTransaction {
    version: u32,
    phase: InstallTransactionPhase,
    state_path: PathBuf,
    alias_path: PathBuf,
    launcher_path: PathBuf,
    proxy_path: PathBuf,
    previous_state: FileBackup,
    previous_alias: FileBackup,
    previous_environment: RegistryBackup,
    #[serde(default)]
    previous_guardian_run: RegistryBackup,
    #[serde(default)]
    applied_guardian_command: String,
    applied_state_sha256: String,
    applied_alias_sha256: String,
    launcher_sha256: String,
    proxy_sha256: String,
    launcher_existed: bool,
    proxy_existed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyInstallState {
    #[serde(default)]
    alias_original_content: Option<String>,
    #[serde(default)]
    alias_existed: bool,
    #[serde(default)]
    original_codex_cli_path: Option<String>,
}

#[derive(Debug)]
struct InstallPaths {
    root: PathBuf,
    state: PathBuf,
    transaction: PathBuf,
    legacy_state: PathBuf,
    launcher: PathBuf,
    proxy: PathBuf,
    alias: PathBuf,
}

#[cfg(windows)]
trait InstallBackend {
    fn copy_file(&mut self, source: &Path, destination: &Path) -> Result<()>;
    fn validate_launcher(&mut self, path: &Path, expected_sha256: &str) -> Result<()>;
    fn write_file(&mut self, path: &Path, bytes: &[u8]) -> Result<()>;
    fn set_cli_path(&mut self, path: &Path) -> Result<()>;
    fn set_guardian_run(&mut self, command: &str) -> Result<()>;
    fn broadcast_environment_change(&mut self);
    fn verify_launcher(&mut self, path: &Path) -> Result<()>;
    fn write_transaction(&mut self, path: &Path, transaction: &InstallTransaction) -> Result<()>;
}

#[cfg(windows)]
trait EnvironmentBackend {
    fn read_cli_path_backup(&mut self) -> Result<RegistryBackup>;
    fn current_cli_path(&mut self) -> Result<Option<String>>;
    fn restore_cli_path(&mut self, backup: &RegistryBackup) -> Result<()>;
    fn read_guardian_run_backup(&mut self) -> Result<RegistryBackup>;
    fn current_guardian_run(&mut self) -> Result<Option<String>>;
    fn restore_guardian_run(&mut self, backup: &RegistryBackup) -> Result<()>;
    fn broadcast_environment_change(&mut self);
}

#[cfg(windows)]
struct SystemInstallBackend;

#[cfg(windows)]
struct SystemEnvironmentBackend;

#[cfg(windows)]
impl InstallBackend for SystemInstallBackend {
    fn copy_file(&mut self, source: &Path, destination: &Path) -> Result<()> {
        atomic_copy(source, destination)
    }

    fn validate_launcher(&mut self, path: &Path, expected_sha256: &str) -> Result<()> {
        validate_expected_sha256(path, expected_sha256, "launcher")
    }

    fn write_file(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        atomic_write(path, bytes)
    }

    fn set_cli_path(&mut self, path: &Path) -> Result<()> {
        set_codex_cli_path(path)
    }

    fn set_guardian_run(&mut self, command: &str) -> Result<()> {
        set_guardian_run(command)
    }

    fn broadcast_environment_change(&mut self) {
        broadcast_environment_change();
    }

    fn verify_launcher(&mut self, path: &Path) -> Result<()> {
        verify_launcher(path)
    }

    fn write_transaction(&mut self, path: &Path, transaction: &InstallTransaction) -> Result<()> {
        write_install_transaction(path, transaction)
    }
}

#[cfg(windows)]
impl EnvironmentBackend for SystemEnvironmentBackend {
    fn read_cli_path_backup(&mut self) -> Result<RegistryBackup> {
        read_codex_cli_path_backup()
    }

    fn current_cli_path(&mut self) -> Result<Option<String>> {
        current_codex_cli_path()
    }

    fn restore_cli_path(&mut self, backup: &RegistryBackup) -> Result<()> {
        restore_codex_cli_path(backup)
    }

    fn read_guardian_run_backup(&mut self) -> Result<RegistryBackup> {
        read_guardian_run_backup()
    }

    fn current_guardian_run(&mut self) -> Result<Option<String>> {
        current_guardian_run()
    }

    fn restore_guardian_run(&mut self, backup: &RegistryBackup) -> Result<()> {
        restore_guardian_run(backup)
    }

    fn broadcast_environment_change(&mut self) {
        broadcast_environment_change();
    }
}

#[cfg(windows)]
pub(crate) struct OperationLock(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl OperationLock {
    pub(crate) fn acquire() -> Result<Self> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{CreateMutexW, WaitForSingleObject},
        };

        let name: Vec<u16> = "Local\\comidea.CodexImageFix.InstallOperation"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("failed to create operation lock");
        }
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            WAIT_TIMEOUT => {
                unsafe { CloseHandle(handle) };
                bail!("another Comidea Codex Image Bridge operation is already running");
            }
            _ => {
                let error = std::io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                Err(error).context("failed to acquire operation lock")
            }
        }
    }
}

#[cfg(windows)]
impl Drop for OperationLock {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0);
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallationHealth {
    NotInstalled,
    Healthy,
    Broken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeState {
    NotInstalled,
    Ready,
    Connected,
    RestartRequired,
    Broken,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusReport {
    pub health: InstallationHealth,
    pub fix_root: PathBuf,
    pub state_present: bool,
    pub launcher_present: bool,
    pub proxy_present: bool,
    pub alias_present: bool,
    pub codex_cli_path: Option<String>,
    pub real_cli: Option<PathBuf>,
    pub real_cli_error: Option<String>,
    pub environment_healthy: bool,
    pub alias_healthy: bool,
    pub proxy_healthy: bool,
    pub launcher_healthy: bool,
    pub guardian_installed: bool,
    pub guardian_running: bool,
    pub codex_running: bool,
    pub proxy_running: bool,
    pub restart_required: bool,
    pub runtime_state: RuntimeState,
}

#[derive(Clone, Debug, Default)]
pub struct UninstallOutcome {
    pub model_config_restored: bool,
    pub model_config_warning: Option<String>,
    pub files_pending_cleanup: bool,
}

pub fn install(real_cli: Option<&Path>) -> Result<()> {
    #[cfg(not(windows))]
    {
        let _ = real_cli;
        bail!("installation is only supported on Windows");
    }
    #[cfg(windows)]
    install_windows(real_cli)
}

pub fn uninstall() -> Result<UninstallOutcome> {
    #[cfg(not(windows))]
    {
        bail!("uninstallation is only supported on Windows");
    }
    #[cfg(windows)]
    uninstall_windows()
}

pub fn repair() -> Result<()> {
    if status_report()?.health == InstallationHealth::NotInstalled {
        bail!("image compatibility layer is not installed");
    }
    install(None)
}

pub fn fix_root() -> Result<PathBuf> {
    Ok(install_paths()?.root)
}

pub fn print_status() -> Result<()> {
    let report = status_report()?;
    print!("{}", format_status_report(&report));
    Ok(())
}

pub fn format_status_report(report: &StatusReport) -> String {
    let mut output = String::new();
    output.push_str(&format!("fix root: {}\n", report.fix_root.display()));
    output.push_str(&format!("state: {}\n", present(report.state_present)));
    output.push_str(&format!("launcher: {}\n", present(report.launcher_present)));
    output.push_str(&format!("proxy: {}\n", present(report.proxy_present)));
    output.push_str(&format!("alias: {}\n", present(report.alias_present)));
    match &report.codex_cli_path {
        Some(value) => output.push_str(&format!("user CODEX_CLI_PATH: {value}\n")),
        None => output.push_str("user CODEX_CLI_PATH: missing\n"),
    }
    match &report.real_cli {
        Some(path) => output.push_str(&format!("real Codex CLI: {}\n", path.display())),
        None => output.push_str(&format!(
            "real Codex CLI: unavailable ({})\n",
            report.real_cli_error.as_deref().unwrap_or("unknown error")
        )),
    }
    if report.state_present {
        output.push_str(&format!(
            "environment integration: {}\n",
            healthy(report.environment_healthy)
        ));
        output.push_str(&format!(
            "alias integration: {}\n",
            healthy(report.alias_healthy)
        ));
        output.push_str(&format!(
            "proxy integrity: {}\n",
            healthy(report.proxy_healthy)
        ));
        output.push_str(&format!(
            "launcher integrity: {}\n",
            healthy(report.launcher_healthy)
        ));
        output.push_str(&format!(
            "guardian startup: {}\n",
            healthy(report.guardian_installed)
        ));
    }
    output.push_str(&format!("guardian running: {}\n", report.guardian_running));
    output.push_str(&format!("Codex running: {}\n", report.codex_running));
    output.push_str(&format!("proxy connected: {}\n", report.proxy_running));
    output.push_str(&format!("restart required: {}\n", report.restart_required));
    output.push_str(&format!("runtime state: {:?}\n", report.runtime_state));
    output
}

pub fn status_report() -> Result<StatusReport> {
    #[cfg(windows)]
    let _operation_lock = OperationLock::acquire()?;
    let paths = install_paths()?;
    #[cfg(windows)]
    recover_pending_install(&paths)?;
    let state_present = paths.state.is_file();
    let state = state_present
        .then(|| read_state(&paths.state))
        .transpose()?;
    let launcher_present = paths.launcher.is_file();
    let proxy_path = state
        .as_ref()
        .map(|state| state.proxy.as_path())
        .unwrap_or(&paths.proxy);
    let proxy_present = proxy_path.is_file();
    let alias_present = paths.alias.is_file();
    let codex_cli_path = current_codex_cli_path()?;
    #[cfg(windows)]
    let guardian_run = current_guardian_run()?;
    #[cfg(not(windows))]
    let guardian_run: Option<String> = None;
    let (real_cli, real_cli_error) = match resolve_real_cli(None) {
        Ok(path) => (Some(path), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };

    let mut environment_healthy = false;
    let mut alias_healthy = false;
    let mut proxy_healthy = false;
    let mut launcher_healthy = false;
    let mut guardian_installed = false;
    if let Some(state) = state.as_ref() {
        environment_healthy =
            codex_cli_path.as_deref() == Some(state.launcher.to_string_lossy().as_ref());
        alias_healthy = file_sha256(&paths.alias).ok().as_deref()
            == Some(state.installed_alias_sha256.as_str());
        proxy_healthy =
            file_sha256(proxy_path).ok().as_deref() == Some(state.installed_proxy_sha256.as_str());
        launcher_healthy = if state.installed_launcher_sha256.is_empty() {
            launcher_present
        } else {
            file_sha256(&paths.launcher).ok().as_deref()
                == Some(state.installed_launcher_sha256.as_str())
        };
        guardian_installed = !state.installed_guardian_command.is_empty()
            && guardian_run.as_deref() == Some(state.installed_guardian_command.as_str());
    }

    let integration_points_to_launcher =
        codex_cli_path.as_deref() == Some(paths.launcher.to_string_lossy().as_ref());
    let alias_is_managed_without_state = !state_present
        && fs::read_to_string(&paths.alias).is_ok_and(|contents| contents.contains(ALIAS_MARKER));
    let health = if state_present
        && launcher_present
        && proxy_present
        && alias_present
        && environment_healthy
        && alias_healthy
        && proxy_healthy
        && launcher_healthy
        && guardian_installed
    {
        InstallationHealth::Healthy
    } else if !state_present && !integration_points_to_launcher && !alias_is_managed_without_state {
        InstallationHealth::NotInstalled
    } else {
        InstallationHealth::Broken
    };

    let active_runtime = state
        .as_ref()
        .filter(|_| proxy_path.is_file())
        .map(|_| crate::runtime::active_runtime(proxy_path))
        .transpose()?
        .unwrap_or_default();
    let guardian_running = !active_runtime.guardian_pids.is_empty();
    let proxy_running = !active_runtime.proxy_pids.is_empty();
    let codex_running = !crate::diagnostics::running_codex_processes()?.is_empty();
    let restart_required = state_present && crate::runtime::restart_required()?;
    let runtime_state = runtime_state_for(
        health,
        guardian_running,
        codex_running,
        proxy_running,
        restart_required,
    );

    Ok(StatusReport {
        health,
        fix_root: paths.root,
        state_present,
        launcher_present,
        proxy_present,
        alias_present,
        codex_cli_path,
        real_cli,
        real_cli_error,
        environment_healthy,
        alias_healthy,
        proxy_healthy,
        launcher_healthy,
        guardian_installed,
        guardian_running,
        codex_running,
        proxy_running,
        restart_required,
        runtime_state,
    })
}

fn runtime_state_for(
    health: InstallationHealth,
    guardian_running: bool,
    codex_running: bool,
    proxy_running: bool,
    restart_required: bool,
) -> RuntimeState {
    if health == InstallationHealth::NotInstalled {
        RuntimeState::NotInstalled
    } else if health == InstallationHealth::Broken || !guardian_running {
        RuntimeState::Broken
    } else if restart_required {
        RuntimeState::RestartRequired
    } else if codex_running && proxy_running {
        RuntimeState::Connected
    } else if codex_running {
        RuntimeState::Broken
    } else {
        RuntimeState::Ready
    }
}

#[cfg(windows)]
pub fn repair_missing_integration_entries() -> Result<bool> {
    let _operation_lock = OperationLock::acquire()?;
    let paths = install_paths()?;
    recover_pending_install(&paths)?;
    let state = read_state(&paths.state).context("image compatibility layer is not installed")?;
    let previous_environment = read_codex_cli_path_backup()?;
    let previous_guardian_run = read_guardian_run_backup()?;
    let cli_repair_needed = automatic_registry_entry_repair_needed(
        "CODEX_CLI_PATH",
        current_codex_cli_path()?.as_deref(),
        state.launcher.to_string_lossy().as_ref(),
    )?;
    let guardian_repair_needed = automatic_registry_entry_repair_needed(
        GUARDIAN_RUN_VALUE,
        current_guardian_run()?.as_deref(),
        &state.installed_guardian_command,
    )?;
    if !cli_repair_needed && !guardian_repair_needed {
        return Ok(false);
    }
    validate_state_for_integration_repair(&paths, &state)?;

    let repair_result = (|| -> Result<()> {
        if cli_repair_needed {
            set_codex_cli_path(&state.launcher)?;
        }
        if guardian_repair_needed {
            set_guardian_run(&state.installed_guardian_command)?;
        }
        if cli_repair_needed {
            broadcast_environment_change();
            let codex_pids = crate::diagnostics::running_codex_processes()
                .unwrap_or_default()
                .into_iter()
                .map(|process| process.process_id)
                .collect::<Vec<_>>();
            crate::runtime::mark_restart_required(&codex_pids)
                .context("failed to record the required Codex restart")?;
        }
        Ok(())
    })();
    if let Err(error) = repair_result {
        let environment_rollback = cli_repair_needed
            .then(|| restore_codex_cli_path(&previous_environment))
            .transpose();
        let guardian_rollback = guardian_repair_needed
            .then(|| restore_guardian_run(&previous_guardian_run))
            .transpose();
        if cli_repair_needed {
            broadcast_environment_change();
        }
        if let Err(rollback_error) = environment_rollback.and(guardian_rollback) {
            return Err(error).context(format!(
                "automatic integration repair failed and rollback also failed: {rollback_error:#}"
            ));
        }
        return Err(error).context("automatic integration repair failed; previous values restored");
    }
    Ok(true)
}

fn automatic_registry_entry_repair_needed(
    label: &str,
    current: Option<&str>,
    expected: &str,
) -> Result<bool> {
    if expected.trim().is_empty() {
        bail!("{label} has no managed value in installation state");
    }
    match current.map(str::trim) {
        None | Some("") => Ok(true),
        Some(current) if current == expected => Ok(false),
        Some(_) => {
            bail!("{label} points to another program; refusing to overwrite it automatically")
        }
    }
}

#[cfg(not(windows))]
pub fn repair_missing_integration_entries() -> Result<bool> {
    bail!("automatic entry repair is only supported on Windows")
}

#[cfg(windows)]
fn validate_state_for_integration_repair(paths: &InstallPaths, state: &InstallState) -> Result<()> {
    if state.installed_guardian_command.trim().is_empty() {
        bail!("installation state predates guarded startup verification; run a manual update");
    }
    if state.launcher != paths.launcher
        || state.alias != paths.alias
        || state.proxy.parent() != Some(paths.root.as_path())
    {
        bail!("installation state targets unexpected paths");
    }
    if state.installed_launcher_sha256.is_empty() {
        bail!("installation state predates guarded launcher verification; run a manual update");
    }
    for (path, expected, label) in [
        (
            state.launcher.as_path(),
            state.installed_launcher_sha256.as_str(),
            "launcher",
        ),
        (
            state.alias.as_path(),
            state.installed_alias_sha256.as_str(),
            "alias",
        ),
        (
            state.proxy.as_path(),
            state.installed_proxy_sha256.as_str(),
            "proxy",
        ),
    ] {
        if !path.is_file() || file_sha256(path)? != expected {
            bail!("{label} integrity check failed; automatic repair was refused");
        }
    }
    Ok(())
}

pub fn verify_chat(thread_id: &str) -> Result<()> {
    let paths = install_paths()?;
    if !paths.launcher.is_file() {
        bail!(
            "installed launcher is missing: {}",
            paths.launcher.display()
        );
    }
    let debug_log = paths.root.join(format!(
        "verify-debug-{}-{}.log",
        std::process::id(),
        sanitize(thread_id)
    ));
    let _ = fs::remove_file(&debug_log);
    let mut child = hidden_command(&paths.launcher)
        .args([
            "-c",
            "features.code_mode_host=true",
            "app-server",
            "--analytics-default-enabled",
        ])
        .env("CODEX_IMAGE_PROXY_DEBUG", &debug_log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start {}", paths.launcher.display()))?;
    let mut input = child
        .stdin
        .take()
        .context("app-server stdin is unavailable")?;
    let output = child
        .stdout
        .take()
        .context("app-server stdout is unavailable")?;
    let (sender, receiver) = mpsc::channel::<Result<serde_json::Value>>();
    let reader = thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let result = line
                .map_err(anyhow::Error::from)
                .and_then(|line| serde_json::from_str(&line).map_err(anyhow::Error::from));
            if sender.send(result).is_err() {
                break;
            }
        }
    });

    write_json_request(
        &mut input,
        &serde_json::json!({
            "id": 9000,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "codex-image-fix-verifier",
                    "title": "Comidea Codex Image Bridge Verifier",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
    )?;

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut resume_requested = false;
    let mut read_requested = false;
    let result = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break Err(anyhow::anyhow!(
                "installed app-server verification timed out"
            ));
        }
        let message = match receiver.recv_timeout(remaining) {
            Ok(Ok(message)) => message,
            Ok(Err(error)) => break Err(error.context("app-server emitted non-JSON stdout")),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                break Err(anyhow::anyhow!(
                    "installed app-server verification timed out"
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow::anyhow!("installed app-server closed stdout"));
            }
        };
        if message.get("id").and_then(serde_json::Value::as_i64) == Some(9000) && !resume_requested
        {
            write_json_request(
                &mut input,
                &serde_json::json!({"method": "initialized", "params": {}}),
            )?;
            write_json_request(
                &mut input,
                &serde_json::json!({
                    "id": 9001,
                    "method": "thread/resume",
                    "params": {"threadId": thread_id, "includeTurns": true}
                }),
            )?;
            resume_requested = true;
        } else if message.get("id").and_then(serde_json::Value::as_i64) == Some(9001)
            && !read_requested
        {
            if let Some(error) = message.get("error") {
                break Err(anyhow::anyhow!("thread/resume failed: {error}"));
            }
            write_json_request(
                &mut input,
                &serde_json::json!({
                    "id": 9002,
                    "method": "thread/read",
                    "params": {"threadId": thread_id, "includeTurns": true}
                }),
            )?;
            read_requested = true;
        } else if message.get("id").and_then(serde_json::Value::as_i64) == Some(9002) {
            break summarize_chat_verification(&message, thread_id);
        }
    };

    cleanup_verification_processes(&mut child, &debug_log);
    drop(receiver);
    drop(reader);
    if let Err(error) = result {
        let debug = fs::read_to_string(&debug_log).unwrap_or_else(|_| "no proxy input log".into());
        return Err(error).context(format!("proxy input diagnostics: {}", debug.trim()));
    }
    let _ = fs::remove_file(debug_log);
    Ok(())
}

fn cleanup_verification_processes(child: &mut std::process::Child, debug_log: &Path) {
    #[cfg(windows)]
    if let Some(proxy_pid) = proxy_pid_from_debug_log(debug_log) {
        let _ = hidden_command("taskkill")
            .args(["/PID", &proxy_pid.to_string(), "/T", "/F"])
            .output();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn proxy_pid_from_debug_log(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        line.strip_prefix("pid=")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

fn write_json_request(writer: &mut impl Write, value: &serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn summarize_chat_verification(message: &serde_json::Value, expected_thread: &str) -> Result<()> {
    if let Some(error) = message.get("error") {
        bail!("thread/read failed: {error}");
    }
    let thread = message
        .pointer("/result/thread")
        .context("thread/read response has no thread")?;
    let actual_thread = thread
        .get("id")
        .and_then(serde_json::Value::as_str)
        .context("thread/read response has no thread id")?;
    if actual_thread != expected_thread {
        bail!("thread/read returned unexpected thread {actual_thread}");
    }
    let mut images = 0;
    let mut empty_results = 0;
    let mut existing_paths = 0;
    for item in thread
        .get("turns")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|turn| turn.get("items").and_then(serde_json::Value::as_array))
        .flatten()
        .filter(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("imageGeneration")
        })
    {
        images += 1;
        if item.get("result").and_then(serde_json::Value::as_str) == Some("") {
            empty_results += 1;
        }
        if item
            .get("savedPath")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|path| Path::new(path).is_file())
        {
            existing_paths += 1;
        }
    }
    println!("thread: {actual_thread}");
    println!("imageGeneration items: {images}");
    println!("empty results: {empty_results}");
    println!("existing saved paths: {existing_paths}");
    if images == 0 || empty_results != images || existing_paths != images {
        bail!("chat image verification failed");
    }
    Ok(())
}

pub fn resolve_real_cli(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit.filter(|path| path.is_file()) {
        return canonical_file(path);
    }
    if let Some(path) = std::env::var_os(REAL_CLI_ENV)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return canonical_file(&path);
    }
    if let Ok(paths) = install_paths() {
        if let Ok(state) = read_state(&paths.state) {
            if state.real_cli.is_file() {
                return canonical_file(&state.real_cli);
            }
        }
    }

    let local_app_data = local_app_data()?;
    let bin_root = local_app_data.join("OpenAI").join("Codex").join("bin");
    let mut candidates = Vec::new();
    if bin_root.is_dir() {
        for entry in fs::read_dir(&bin_root)? {
            let path = entry?.path().join("codex.exe");
            if path.is_file() {
                candidates.push(path);
            }
        }
    }
    candidates.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    let path = candidates
        .pop()
        .context("no real Codex CLI found under %LOCALAPPDATA%\\OpenAI\\Codex\\bin")?;
    canonical_file(&path)
}

#[cfg(windows)]
fn validated_existing_launcher_sha256(
    launcher: &Path,
    state: Option<&InstallState>,
    mut validate_unmanaged: impl FnMut(&Path) -> Result<()>,
) -> Result<Option<String>> {
    if !launcher.exists() {
        return Ok(None);
    }
    if !launcher.is_file() {
        bail!(
            "launcher path is not a regular file: {}",
            launcher.display()
        );
    }

    let current_sha256 = file_sha256(launcher)?;
    let managed_sha256 = state
        .filter(|state| state.launcher == launcher)
        .map(|state| state.installed_launcher_sha256.as_str())
        .filter(|sha256| !sha256.is_empty());
    if let Some(expected_sha256) = managed_sha256 {
        if current_sha256 != expected_sha256 {
            bail!("managed launcher changed after installation; refusing to reuse it");
        }
        return Ok(Some(current_sha256));
    }

    validate_unmanaged(launcher)?;
    Ok(Some(current_sha256))
}

#[cfg(windows)]
fn install_windows(explicit_real_cli: Option<&Path>) -> Result<()> {
    let _operation_lock = OperationLock::acquire()?;
    let paths = install_paths()?;
    recover_pending_install(&paths)?;
    let real_cli = resolve_real_cli(explicit_real_cli)?;
    validate_codex_distribution(&real_cli)?;

    let system_powershell = system_powershell()?;
    validate_signature(&system_powershell, "Microsoft")?;
    let current_exe = canonical_file(&std::env::current_exe()?)?;
    if current_exe == real_cli {
        bail!("refusing to install the real Codex CLI as the proxy");
    }

    fs::create_dir_all(&paths.root)?;
    let original_state = if paths.state.is_file() {
        Some(read_state(&paths.state)?)
    } else {
        None
    };
    let existing_launcher_sha256 =
        validated_existing_launcher_sha256(&paths.launcher, original_state.as_ref(), |launcher| {
            validate_signature(launcher, "Microsoft")
        })?;
    verify_alias_is_replaceable(&paths, original_state.as_ref())?;
    let previous_state_file = capture_file_backup(&paths.state)?;
    let previous_alias_file = capture_file_backup(&paths.alias)?;
    let previous_environment = read_codex_cli_path_backup()?;
    let previous_guardian_run = read_guardian_run_backup()?;

    let legacy_backups = if original_state.is_none() {
        read_legacy_backups(&paths)?
    } else {
        None
    };
    let original_environment = original_state
        .as_ref()
        .map(|state| state.original_codex_cli_path.clone())
        .or_else(|| legacy_backups.as_ref().map(|backups| backups.0.clone()))
        .unwrap_or_else(|| previous_environment.clone());
    let original_alias = original_state
        .as_ref()
        .map(|state| state.original_alias.clone())
        .or_else(|| legacy_backups.as_ref().map(|backups| backups.1.clone()))
        .unwrap_or_else(|| previous_alias_file.clone());
    let original_guardian_run = original_state
        .as_ref()
        .filter(|state| !state.installed_guardian_command.is_empty())
        .map(|state| state.original_guardian_run.clone())
        .unwrap_or_else(|| previous_guardian_run.clone());

    let installed_proxy_sha256 = file_sha256(&current_exe)?;
    let installed_proxy = versioned_proxy_path(&paths.root, &installed_proxy_sha256);
    let proxy_existed = installed_proxy.is_file();
    if proxy_existed && file_sha256(&installed_proxy)? != installed_proxy_sha256 {
        bail!(
            "versioned proxy path contains unexpected content: {}",
            installed_proxy.display()
        );
    }
    let launcher_existed = existing_launcher_sha256.is_some();
    let launcher_sha256 = match existing_launcher_sha256 {
        Some(sha256) => sha256,
        None => file_sha256(&system_powershell)?,
    };
    let proxy_file_name = installed_proxy
        .file_name()
        .and_then(OsStr::to_str)
        .context("installed proxy has no UTF-8 file name")?;
    let alias_bytes = alias_contents(proxy_file_name).into_bytes();
    let installed_alias_sha256 = sha256(&alias_bytes);
    let installed_guardian_command = guardian_command(&installed_proxy);

    let state = InstallState {
        version: STATE_VERSION,
        installed_at_unix: unix_time_seconds(),
        real_cli,
        launcher: paths.launcher.clone(),
        proxy: installed_proxy.clone(),
        alias: paths.alias.clone(),
        installed_alias_sha256: installed_alias_sha256.clone(),
        installed_proxy_sha256: installed_proxy_sha256.clone(),
        installed_launcher_sha256: launcher_sha256.clone(),
        original_codex_cli_path: original_environment,
        original_alias,
        original_guardian_run,
        installed_guardian_command: installed_guardian_command.clone(),
    };
    let state_bytes = serde_json::to_vec_pretty(&state)?;
    let mut transaction = InstallTransaction {
        version: TRANSACTION_VERSION,
        phase: InstallTransactionPhase::Applying,
        state_path: paths.state.clone(),
        alias_path: paths.alias.clone(),
        launcher_path: paths.launcher.clone(),
        proxy_path: installed_proxy.clone(),
        previous_state: previous_state_file,
        previous_alias: previous_alias_file,
        previous_environment,
        previous_guardian_run,
        applied_guardian_command: installed_guardian_command,
        applied_state_sha256: sha256(&state_bytes),
        applied_alias_sha256: installed_alias_sha256,
        launcher_sha256,
        proxy_sha256: installed_proxy_sha256,
        launcher_existed,
        proxy_existed,
    };
    write_install_transaction(&paths.transaction, &transaction)?;

    let install_result = apply_install_transaction(
        &mut SystemInstallBackend,
        &current_exe,
        &system_powershell,
        &state_bytes,
        &alias_bytes,
        &paths.transaction,
        &mut transaction,
    );
    if let Err(error) = install_result {
        return match rollback_install_transaction(&paths, &transaction) {
            Ok(()) => Err(error).context("installation failed; previous installation was restored"),
            Err(rollback_error) => Err(error).context(format!(
                "installation failed and rollback also failed: {rollback_error:#}"
            )),
        };
    }

    if let Some(previous) = original_state
        .as_ref()
        .map(|state| state.proxy.as_path())
        .filter(|previous| *previous != state.proxy)
    {
        let previous_owned = original_state.as_ref().is_some_and(|state| {
            file_sha256(previous).ok().as_deref() == Some(state.installed_proxy_sha256.as_str())
        });
        if previous_owned {
            let _ = fs::remove_file(previous);
        }
    }
    let _ = fs::remove_file(&paths.transaction);

    let codex_pids = crate::diagnostics::running_codex_processes()?
        .into_iter()
        .map(|process| process.process_id)
        .collect::<Vec<_>>();
    if let Err(error) = crate::runtime::mark_restart_required(&codex_pids) {
        eprintln!("codex-image-fix: failed to record required Codex restart: {error:#}");
    }

    if let Err(error) = crate::guardian::restart_installed(&state.proxy) {
        eprintln!("codex-image-fix: failed to start status guardian: {error:#}");
    }

    println!("installed Codex image display proxy");
    println!("launcher: {}", paths.launcher.display());
    println!("real CLI: {}", state.real_cli.display());
    println!("restart Codex Desktop completely before testing");
    Ok(())
}

#[cfg(windows)]
fn apply_install_transaction(
    backend: &mut impl InstallBackend,
    current_exe: &Path,
    system_launcher: &Path,
    state_bytes: &[u8],
    alias_bytes: &[u8],
    transaction_path: &Path,
    transaction: &mut InstallTransaction,
) -> Result<()> {
    if !transaction.proxy_existed && current_exe != transaction.proxy_path {
        backend.copy_file(current_exe, &transaction.proxy_path)?;
    }
    if !transaction.launcher_existed {
        backend.copy_file(system_launcher, &transaction.launcher_path)?;
    }
    backend.validate_launcher(&transaction.launcher_path, &transaction.launcher_sha256)?;
    backend.write_file(&transaction.state_path, state_bytes)?;
    backend.write_file(&transaction.alias_path, alias_bytes)?;
    backend.set_cli_path(&transaction.launcher_path)?;
    backend.set_guardian_run(&transaction.applied_guardian_command)?;
    backend.broadcast_environment_change();
    backend
        .verify_launcher(&transaction.launcher_path)
        .context("installation self-check failed")?;
    transaction.phase = InstallTransactionPhase::Committed;
    backend.write_transaction(transaction_path, transaction)
}

#[cfg(windows)]
fn uninstall_windows() -> Result<UninstallOutcome> {
    let _operation_lock = OperationLock::acquire()?;
    let paths = install_paths()?;
    recover_pending_install(&paths)?;
    let state = read_state(&paths.state).context("no installation state found")?;
    let state_sha256 = file_sha256(&paths.state)?;
    crate::guardian::stop_existing(Duration::from_secs(5))?;
    let (model_config_restored, mut model_config_warning) =
        match crate::model_config::restore_managed_config() {
            Ok(restored) => (restored, None),
            Err(error) => (false, Some(format!("{error:#}"))),
        };
    let preserve_model_states = match crate::model_config::has_managed_configs() {
        Ok(preserve) => preserve,
        Err(error) => {
            let warning =
                format!("failed to inspect remaining model configuration backups: {error:#}");
            model_config_warning = Some(match model_config_warning.take() {
                Some(existing) => format!("{existing}; {warning}"),
                None => warning,
            });
            true
        }
    };
    if preserve_model_states && model_config_warning.is_none() {
        model_config_warning =
            Some("managed configuration backups for another CODEX_HOME were preserved".to_owned());
    }
    restore_integration(&state)?;
    broadcast_environment_change();
    crate::runtime::clear_all()?;

    let files_pending_cleanup = if preserve_model_states || model_config_warning.is_some() {
        remove_proxy_files_preserving_model_state(&paths, &state, &state_sha256)?
    } else {
        remove_all_installation_files(&paths, &state, &state_sha256)?
    };
    if let Some(warning) = model_config_warning.as_deref() {
        println!("uninstalled Codex image display proxy; model configuration was preserved");
        eprintln!("codex-image-fix: model configuration restore was skipped: {warning}");
    } else {
        println!("uninstalled Codex image display proxy");
    }
    if files_pending_cleanup {
        println!("running Codex processes are holding residual files; integration is removed");
        println!(
            "residual files can be deleted after Codex exits: {}",
            paths.root.display()
        );
    }
    println!("restart Codex Desktop completely");
    Ok(UninstallOutcome {
        model_config_restored,
        model_config_warning,
        files_pending_cleanup,
    })
}

#[cfg(windows)]
fn remove_proxy_files_preserving_model_state(
    paths: &InstallPaths,
    state: &InstallState,
    expected_state_sha256: &str,
) -> Result<bool> {
    remove_proxy_files_preserving_model_state_with_validator(
        paths,
        state,
        expected_state_sha256,
        |launcher| {
            validate_expected_sha256(
                launcher,
                &state.installed_launcher_sha256,
                "installed launcher",
            )
        },
    )
}

#[cfg(windows)]
fn remove_proxy_files_preserving_model_state_with_validator<F>(
    paths: &InstallPaths,
    state: &InstallState,
    expected_state_sha256: &str,
    validate_launcher: F,
) -> Result<bool>
where
    F: FnOnce(&Path) -> Result<()>,
{
    validate_managed_install_files_with_validator(
        paths,
        state,
        expected_state_sha256,
        validate_launcher,
    )?;

    fs::remove_file(&paths.state)
        .with_context(|| format!("failed to remove {}", paths.state.display()))?;
    let mut pending = remove_managed_file(&state.proxy)?;
    pending |= remove_managed_file(&paths.launcher)?;
    if paths.legacy_state.is_file() {
        fs::remove_file(&paths.legacy_state)
            .with_context(|| format!("failed to remove {}", paths.legacy_state.display()))?;
    }
    match fs::remove_dir(&paths.root) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
            ) => {}
        Err(error) => return Err(error).context("failed to clean installation directory"),
    }
    Ok(pending)
}

#[cfg(windows)]
fn remove_all_installation_files(
    paths: &InstallPaths,
    state: &InstallState,
    expected_state_sha256: &str,
) -> Result<bool> {
    validate_managed_install_files(paths, state, expected_state_sha256)?;
    fs::remove_file(&paths.state)
        .with_context(|| format!("failed to remove {}", paths.state.display()))?;
    match fs::remove_dir_all(&paths.root) {
        Ok(()) => Ok(false),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(error.kind() == std::io::ErrorKind::PermissionDenied)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove {}", paths.root.display()))
        }
    }
}

#[cfg(windows)]
fn validate_managed_install_files(
    paths: &InstallPaths,
    state: &InstallState,
    expected_state_sha256: &str,
) -> Result<()> {
    validate_managed_install_files_with_validator(paths, state, expected_state_sha256, |launcher| {
        validate_expected_sha256(
            launcher,
            &state.installed_launcher_sha256,
            "installed launcher",
        )
    })
}

#[cfg(windows)]
fn validate_managed_install_files_with_validator<F>(
    paths: &InstallPaths,
    state: &InstallState,
    expected_state_sha256: &str,
    validate_launcher: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    if state.launcher != paths.launcher
        || state.alias != paths.alias
        || state.proxy.parent() != Some(paths.root.as_path())
    {
        bail!("installation state targets unexpected paths");
    }
    if file_sha256(&paths.state)? != expected_state_sha256 {
        bail!("installation state changed during uninstall; refusing to delete it");
    }
    if state.proxy.is_file() {
        if file_sha256(&state.proxy)? != state.installed_proxy_sha256 {
            bail!("installed proxy changed after installation; refusing to delete it");
        }
    } else if state.proxy.exists() {
        bail!("installed proxy path is no longer a regular file");
    }
    if paths.launcher.is_file() {
        validate_launcher(&paths.launcher)?;
    } else if paths.launcher.exists() {
        bail!("launcher path is no longer a regular file");
    }
    Ok(())
}

#[cfg(windows)]
fn remove_managed_file(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(true),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

#[cfg(windows)]
fn restore_integration(state: &InstallState) -> Result<()> {
    let current_environment = current_codex_cli_path()?;
    if current_environment.as_deref() == Some(state.launcher.to_string_lossy().as_ref()) {
        restore_codex_cli_path(&state.original_codex_cli_path)?;
    } else {
        eprintln!(
            "codex-image-fix: CODEX_CLI_PATH changed after installation; leaving it untouched"
        );
    }

    if state.alias.is_file() {
        let current_hash = file_sha256(&state.alias)?;
        if current_hash == state.installed_alias_sha256 {
            restore_alias(&state.alias, &state.original_alias)?;
        } else {
            eprintln!("codex-image-fix: alias changed after installation; leaving it untouched");
        }
    }

    if !state.installed_guardian_command.is_empty() {
        let current_guardian = current_guardian_run()?;
        if guardian_run_is_owned(
            current_guardian.as_deref(),
            &state.installed_guardian_command,
        ) {
            restore_guardian_run(&state.original_guardian_run)?;
        } else {
            eprintln!(
                "codex-image-fix: guardian startup command changed after installation; leaving it untouched"
            );
        }
    }
    Ok(())
}

fn guardian_run_is_owned(current: Option<&str>, installed_command: &str) -> bool {
    !installed_command.is_empty() && current == Some(installed_command)
}

#[cfg(windows)]
fn recover_pending_install(paths: &InstallPaths) -> Result<()> {
    recover_pending_install_with(paths, &mut SystemEnvironmentBackend)
}

#[cfg(windows)]
fn recover_pending_install_with(
    paths: &InstallPaths,
    environment: &mut impl EnvironmentBackend,
) -> Result<()> {
    if !paths.transaction.exists() {
        return Ok(());
    }
    if !paths.transaction.is_file() {
        bail!(
            "installation transaction path is not a file: {}",
            paths.transaction.display()
        );
    }
    let transaction = read_install_transaction(&paths.transaction)?;
    validate_transaction_paths(paths, &transaction)?;
    if transaction.phase == InstallTransactionPhase::Committed {
        fs::remove_file(&paths.transaction)
            .context("failed to finalize committed installation transaction")?;
        return Ok(());
    }
    rollback_install_transaction_with(paths, &transaction, environment)
        .context("failed to recover interrupted installation")
}

#[cfg(windows)]
fn validate_transaction_paths(
    paths: &InstallPaths,
    transaction: &InstallTransaction,
) -> Result<()> {
    if transaction.state_path != paths.state
        || transaction.alias_path != paths.alias
        || transaction.launcher_path != paths.launcher
        || transaction.proxy_path.parent() != Some(paths.root.as_path())
    {
        bail!("installation transaction targets unexpected paths");
    }
    Ok(())
}

#[cfg(windows)]
fn restore_transaction_environment_with(
    environment: &mut impl EnvironmentBackend,
    transaction: &InstallTransaction,
) -> Result<()> {
    let current = environment.read_cli_path_backup()?;
    let current_path = environment.current_cli_path()?;
    if !transaction_environment_needs_restore(&current, current_path.as_deref(), transaction)? {
        return Ok(());
    }
    environment.restore_cli_path(&transaction.previous_environment)
}

#[cfg(windows)]
fn restore_transaction_guardian_with(
    environment: &mut impl EnvironmentBackend,
    transaction: &InstallTransaction,
) -> Result<()> {
    if transaction.applied_guardian_command.is_empty() {
        return Ok(());
    }
    let current = environment.read_guardian_run_backup()?;
    if current == transaction.previous_guardian_run {
        return Ok(());
    }
    if environment.current_guardian_run()?.as_deref()
        != Some(transaction.applied_guardian_command.as_str())
    {
        bail!(
            "guardian startup command changed while installation was in progress; refusing to overwrite it"
        );
    }
    environment.restore_guardian_run(&transaction.previous_guardian_run)
}

fn transaction_environment_needs_restore(
    current: &RegistryBackup,
    current_path: Option<&str>,
    transaction: &InstallTransaction,
) -> Result<bool> {
    if current == &transaction.previous_environment {
        return Ok(false);
    }
    if current_path != Some(transaction.launcher_path.to_string_lossy().as_ref()) {
        bail!(
            "CODEX_CLI_PATH changed while installation was in progress; refusing to overwrite it"
        );
    }
    Ok(true)
}

#[cfg(windows)]
fn rollback_install_transaction(
    paths: &InstallPaths,
    transaction: &InstallTransaction,
) -> Result<()> {
    rollback_install_transaction_with(paths, transaction, &mut SystemEnvironmentBackend)
}

#[cfg(windows)]
fn rollback_install_transaction_with(
    paths: &InstallPaths,
    transaction: &InstallTransaction,
    environment: &mut impl EnvironmentBackend,
) -> Result<()> {
    let file_result = rollback_transaction_files(transaction);
    let environment_result = restore_transaction_environment_with(environment, transaction);
    let guardian_result = restore_transaction_guardian_with(environment, transaction);
    let mut failures = Vec::new();
    if let Err(error) = file_result {
        failures.push(format!("{error:#}"));
    }
    if let Err(error) = environment_result {
        failures.push(format!("{error:#}"));
    }
    if let Err(error) = guardian_result {
        failures.push(format!("{error:#}"));
    }
    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    fs::remove_file(&paths.transaction)
        .context("failed to remove completed installation rollback journal")?;
    environment.broadcast_environment_change();
    Ok(())
}

#[cfg(windows)]
fn validate_codex_distribution(real_cli: &Path) -> Result<()> {
    validate_signature(real_cli, "OpenAI")?;
    let directory = real_cli
        .parent()
        .context("real CLI has no parent directory")?;
    validate_optional_codex_helpers(directory, |helper| validate_signature(helper, "OpenAI"))?;
    let output = hidden_command(real_cli).arg("--version").output()?;
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout).starts_with("codex-cli ")
    {
        bail!("real Codex CLI failed its version check");
    }
    Ok(())
}

#[cfg(windows)]
fn validate_optional_codex_helpers(
    directory: &Path,
    mut validate: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    for name in [
        "codex-code-mode-host.exe",
        "codex-command-runner.exe",
        "codex-windows-sandbox-setup.exe",
    ] {
        let helper = directory.join(name);
        if !helper.exists() {
            continue;
        }
        if !helper.is_file() {
            bail!("Codex helper is not a regular file: {}", helper.display());
        }
        validate(&helper)?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_signature(path: &Path, expected_signer: &str) -> Result<()> {
    let powershell = system_powershell()?;
    let escaped = path.to_string_lossy().replace('\'', "''");
    let command = format!(
        "$s=Get-AuthenticodeSignature -LiteralPath '{}'; Write-Output ($s.Status.ToString() + '|' + $s.SignerCertificate.Subject)",
        escaped
    );
    let output = hidden_command(powershell)
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .output()?;
    let result = String::from_utf8_lossy(&output.stdout);
    let result = result.trim();
    if !output.status.success()
        || !result.starts_with("Valid|")
        || !result
            .to_ascii_lowercase()
            .contains(&expected_signer.to_ascii_lowercase())
    {
        bail!(
            "invalid Authenticode signature for {}: {}",
            path.display(),
            result
        );
    }
    Ok(())
}

#[cfg(windows)]
fn verify_launcher(launcher: &Path) -> Result<()> {
    let output = hidden_command(launcher)
        .args(["-c", "features.code_mode_host=true", "--version"])
        .output()
        .with_context(|| format!("failed to start launcher {}", launcher.display()))?;
    if !output.status.success() {
        bail!("launcher exited with {}", output.status);
    }
    let stdout = String::from_utf8(output.stdout).context("launcher output is not UTF-8")?;
    let lines: Vec<_> = stdout.lines().collect();
    if lines.len() != 1 || !lines[0].starts_with("codex-cli ") {
        bail!("launcher stdout is polluted: {stdout:?}");
    }
    Ok(())
}

fn verify_alias_is_replaceable(paths: &InstallPaths, state: Option<&InstallState>) -> Result<()> {
    if !paths.alias.is_file() {
        return Ok(());
    }
    let bytes = fs::read(&paths.alias)?;
    let recognized = String::from_utf8_lossy(&bytes).contains(ALIAS_MARKER);
    let matches_state = state
        .map(|state| sha256(&bytes) == state.installed_alias_sha256)
        .unwrap_or(false);
    if !recognized && !matches_state {
        bail!(
            "refusing to overwrite foreign alias {}",
            paths.alias.display()
        );
    }
    Ok(())
}

fn alias_contents(proxy_file_name: &str) -> String {
    format!(
        "@echo off\r\nsetlocal DisableDelayedExpansion\r\nrem CodexImageDisplayFix Rust\r\nchcp 65001 >nul\r\nset \"{HIDE_CONSOLE_ENV}=1\"\r\n\"%LOCALAPPDATA%\\CodexImageDisplayFix\\{proxy_file_name}\" -c features.code_mode_host=true %*\r\nexit /b %ERRORLEVEL%\r\n"
    )
}

fn versioned_proxy_path(root: &Path, sha256: &str) -> PathBuf {
    root.join(format!("codex-image-fix-{sha256}.exe"))
}

fn guardian_command(proxy: &Path) -> String {
    format!("\"{}\" guardian", proxy.display())
}

fn hidden_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn install_paths() -> Result<InstallPaths> {
    let local_app_data = local_app_data()?;
    let root = local_app_data.join(FIX_DIRECTORY_NAME);
    Ok(InstallPaths {
        state: root.join("state.json"),
        transaction: root.join(TRANSACTION_FILE_NAME),
        legacy_state: root.join("backup.json"),
        launcher: root.join("codex.exe"),
        proxy: root.join("codex-image-fix.exe"),
        alias: local_app_data
            .join("Microsoft")
            .join("WindowsApps")
            .join(ALIAS_FILE_NAME),
        root,
    })
}

fn local_app_data() -> Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is not set")
}

fn system_powershell() -> Result<PathBuf> {
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .context("SystemRoot is not set")?;
    let path = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !path.is_file() {
        bail!("Windows PowerShell was not found at {}", path.display());
    }
    Ok(path)
}

fn canonical_file(path: &Path) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("file does not exist: {}", path.display());
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    Ok(strip_verbatim_prefix(canonical))
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{stripped}"))
    } else if let Some(stripped) = text.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

fn atomic_copy(source: &Path, destination: &Path) -> Result<()> {
    let bytes = fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    atomic_write(destination, &bytes)
}

fn atomic_write(destination: &Path, bytes: &[u8]) -> Result<()> {
    let parent = destination.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = destination.with_extension(format!("{}.tmp", std::process::id()));
    {
        let mut file = fs::File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    replace_file(&temp, destination).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let succeeded = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error()).context("atomic file replace failed");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).context("atomic file replace failed")
}

fn file_sha256(path: &Path) -> Result<String> {
    Ok(sha256(&fs::read(path)?))
}

#[cfg(windows)]
fn validate_expected_sha256(path: &Path, expected_sha256: &str, label: &str) -> Result<()> {
    if !path.is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    if expected_sha256.is_empty() || file_sha256(path)? != expected_sha256 {
        bail!("{label} integrity check failed: {}", path.display());
    }
    Ok(())
}

fn read_state(path: &Path) -> Result<InstallState> {
    let state: InstallState = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("failed to read {}", path.display()))?,
    )?;
    if state.version != STATE_VERSION {
        bail!("unsupported installation state version {}", state.version);
    }
    Ok(state)
}

fn read_install_transaction(path: &Path) -> Result<InstallTransaction> {
    let transaction: InstallTransaction = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("failed to read {}", path.display()))?,
    )?;
    if transaction.version != TRANSACTION_VERSION {
        bail!(
            "unsupported installation transaction version {}",
            transaction.version
        );
    }
    Ok(transaction)
}

fn write_install_transaction(path: &Path, transaction: &InstallTransaction) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(transaction)?)
}

fn capture_file_backup(path: &Path) -> Result<FileBackup> {
    if !path.is_file() {
        return Ok(FileBackup::default());
    }
    let bytes = fs::read(path)?;
    #[cfg(windows)]
    let attributes = {
        use std::os::windows::fs::MetadataExt;
        Some(fs::metadata(path)?.file_attributes())
    };
    #[cfg(not(windows))]
    let attributes = None;
    Ok(FileBackup {
        existed: true,
        bytes_base64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
        sha256: Some(sha256(&bytes)),
        attributes,
    })
}

fn restore_transaction_file(
    path: &Path,
    backup: &FileBackup,
    applied_sha256: &str,
    label: &str,
) -> Result<()> {
    if path.is_file() {
        let current_sha256 = file_sha256(path)?;
        let is_previous = backup.sha256.as_deref() == Some(current_sha256.as_str());
        if current_sha256 != applied_sha256 && !is_previous {
            bail!("{label} changed while installation was in progress; refusing to overwrite it");
        }
    } else if path.exists() {
        bail!("{label} is no longer a regular file");
    }
    restore_file_backup(path, backup)
}

fn remove_transaction_created_file(
    path: &Path,
    existed_before: bool,
    expected_sha256: &str,
    label: &str,
) -> Result<()> {
    if existed_before || !path.exists() {
        return Ok(());
    }
    if !path.is_file() {
        bail!("new {label} is no longer a regular file");
    }
    if file_sha256(path)? != expected_sha256 {
        bail!("new {label} changed while installation was in progress; refusing to delete it");
    }
    fs::remove_file(path).with_context(|| format!("failed to remove new {label}"))
}

fn rollback_transaction_files(transaction: &InstallTransaction) -> Result<()> {
    let operations = [
        restore_transaction_file(
            &transaction.state_path,
            &transaction.previous_state,
            &transaction.applied_state_sha256,
            "installation state",
        ),
        restore_transaction_file(
            &transaction.alias_path,
            &transaction.previous_alias,
            &transaction.applied_alias_sha256,
            "command alias",
        ),
        remove_transaction_created_file(
            &transaction.launcher_path,
            transaction.launcher_existed,
            &transaction.launcher_sha256,
            "launcher",
        ),
        remove_transaction_created_file(
            &transaction.proxy_path,
            transaction.proxy_existed,
            &transaction.proxy_sha256,
            "proxy",
        ),
    ];
    let failures = operations
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        bail!("installation file rollback failed: {}", failures.join("; "));
    }
    Ok(())
}

fn read_legacy_backups(paths: &InstallPaths) -> Result<Option<(RegistryBackup, FileBackup)>> {
    if !paths.legacy_state.is_file() || !paths.alias.is_file() {
        return Ok(None);
    }
    let alias = fs::read_to_string(&paths.alias)?;
    if !alias.contains(ALIAS_MARKER) {
        return Ok(None);
    }
    let legacy_bytes = fs::read(&paths.legacy_state)?;
    let legacy_bytes = legacy_bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(&legacy_bytes);
    let legacy: LegacyInstallState = serde_json::from_slice(legacy_bytes)?;
    let environment = legacy
        .original_codex_cli_path
        .as_deref()
        .map(registry_string_backup)
        .unwrap_or_default();
    let alias = if legacy.alias_existed {
        let bytes = legacy
            .alias_original_content
            .context("legacy state says alias existed but has no content")?
            .into_bytes();
        FileBackup {
            existed: true,
            bytes_base64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            sha256: Some(sha256(&bytes)),
            attributes: None,
        }
    } else {
        FileBackup::default()
    };
    Ok(Some((environment, alias)))
}

fn registry_string_backup(value: &str) -> RegistryBackup {
    #[cfg(windows)]
    {
        use winreg::enums::REG_SZ;

        let mut bytes = Vec::with_capacity((value.encode_utf16().count() + 1) * 2);
        for unit in value.encode_utf16().chain(Some(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        RegistryBackup {
            existed: true,
            value_type: Some(REG_SZ as u32),
            bytes_base64: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        }
    }
    #[cfg(not(windows))]
    {
        RegistryBackup {
            existed: true,
            value_type: Some(1),
            bytes_base64: Some(base64::engine::general_purpose::STANDARD.encode(value.as_bytes())),
        }
    }
}

fn restore_alias(path: &Path, backup: &FileBackup) -> Result<()> {
    restore_file_backup(path, backup)
}

fn restore_file_backup(path: &Path, backup: &FileBackup) -> Result<()> {
    if !backup.existed {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    let encoded = backup
        .bytes_base64
        .as_deref()
        .context("file backup has no content")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    if backup.sha256.as_deref() != Some(sha256(&bytes).as_str()) {
        bail!("file backup hash mismatch");
    }
    atomic_write(path, &bytes)?;
    #[cfg(windows)]
    if let Some(attributes) = backup.attributes {
        set_file_attributes(path, attributes)?;
    }
    Ok(())
}

#[cfg(windows)]
fn set_file_attributes(path: &Path, attributes: u32) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let succeeded = unsafe { SetFileAttributesW(path.as_ptr(), attributes) };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error()).context("failed to restore file attributes");
    }
    Ok(())
}

#[cfg(windows)]
fn read_codex_cli_path_backup() -> Result<RegistryBackup> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let environment = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Environment")?;
    match environment.get_raw_value("CODEX_CLI_PATH") {
        Ok(value) => Ok(RegistryBackup {
            existed: true,
            value_type: Some(value.vtype.clone() as u32),
            bytes_base64: Some(base64::engine::general_purpose::STANDARD.encode(value.bytes)),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RegistryBackup::default()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn set_codex_cli_path(path: &Path) -> Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let environment = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
        "Environment",
        winreg::enums::KEY_READ | winreg::enums::KEY_WRITE,
    )?;
    environment.set_value("CODEX_CLI_PATH", &path.to_string_lossy().as_ref())?;
    Ok(())
}

#[cfg(windows)]
fn current_codex_cli_path() -> Result<Option<String>> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let environment = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Environment")?;
    match environment.get_value("CODEX_CLI_PATH") {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(windows))]
fn current_codex_cli_path() -> Result<Option<String>> {
    Ok(std::env::var("CODEX_CLI_PATH").ok())
}

#[cfg(windows)]
fn restore_codex_cli_path(backup: &RegistryBackup) -> Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey, RegValue};

    let environment = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
        "Environment",
        winreg::enums::KEY_READ | winreg::enums::KEY_WRITE,
    )?;
    if !backup.existed {
        match environment.delete_value("CODEX_CLI_PATH") {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(
        backup
            .bytes_base64
            .as_deref()
            .context("registry backup has no content")?,
    )?;
    let value = RegValue {
        bytes,
        vtype: registry_type(
            backup
                .value_type
                .context("registry backup has no value type")?,
        )?,
    };
    environment.set_raw_value("CODEX_CLI_PATH", &value)?;
    Ok(())
}

#[cfg(windows)]
fn read_guardian_run_backup() -> Result<RegistryBackup> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let root = RegKey::predef(HKEY_CURRENT_USER);
    let run = match root.open_subkey(GUARDIAN_RUN_KEY) {
        Ok(key) => key,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RegistryBackup::default())
        }
        Err(error) => return Err(error.into()),
    };
    match run.get_raw_value(GUARDIAN_RUN_VALUE) {
        Ok(value) => Ok(RegistryBackup {
            existed: true,
            value_type: Some(value.vtype.clone() as u32),
            bytes_base64: Some(base64::engine::general_purpose::STANDARD.encode(value.bytes)),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RegistryBackup::default()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn current_guardian_run() -> Result<Option<String>> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let root = RegKey::predef(HKEY_CURRENT_USER);
    let run = match root.open_subkey(GUARDIAN_RUN_KEY) {
        Ok(key) => key,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match run.get_value(GUARDIAN_RUN_VALUE) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn set_guardian_run(command: &str) -> Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let root = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = root.create_subkey(GUARDIAN_RUN_KEY)?;
    run.set_value(GUARDIAN_RUN_VALUE, &command)?;
    Ok(())
}

#[cfg(windows)]
fn restore_guardian_run(backup: &RegistryBackup) -> Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey, RegValue};

    let root = RegKey::predef(HKEY_CURRENT_USER);
    if !backup.existed {
        let run = match root.open_subkey_with_flags(
            GUARDIAN_RUN_KEY,
            winreg::enums::KEY_READ | winreg::enums::KEY_WRITE,
        ) {
            Ok(key) => key,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        return match run.delete_value(GUARDIAN_RUN_VALUE) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        };
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(
        backup
            .bytes_base64
            .as_deref()
            .context("guardian registry backup has no content")?,
    )?;
    let value = RegValue {
        bytes,
        vtype: registry_type(
            backup
                .value_type
                .context("guardian registry backup has no value type")?,
        )?,
    };
    let (run, _) = root.create_subkey(GUARDIAN_RUN_KEY)?;
    run.set_raw_value(GUARDIAN_RUN_VALUE, &value)?;
    Ok(())
}

#[cfg(windows)]
fn registry_type(value: u32) -> Result<winreg::enums::RegType> {
    use winreg::enums::*;

    let value = match value {
        value if value == REG_NONE.clone() as u32 => REG_NONE,
        value if value == REG_SZ.clone() as u32 => REG_SZ,
        value if value == REG_EXPAND_SZ.clone() as u32 => REG_EXPAND_SZ,
        value if value == REG_BINARY.clone() as u32 => REG_BINARY,
        value if value == REG_DWORD.clone() as u32 => REG_DWORD,
        value if value == REG_DWORD_BIG_ENDIAN.clone() as u32 => REG_DWORD_BIG_ENDIAN,
        value if value == REG_LINK.clone() as u32 => REG_LINK,
        value if value == REG_MULTI_SZ.clone() as u32 => REG_MULTI_SZ,
        value if value == REG_RESOURCE_LIST.clone() as u32 => REG_RESOURCE_LIST,
        value if value == REG_FULL_RESOURCE_DESCRIPTOR.clone() as u32 => {
            REG_FULL_RESOURCE_DESCRIPTOR
        }
        value if value == REG_RESOURCE_REQUIREMENTS_LIST.clone() as u32 => {
            REG_RESOURCE_REQUIREMENTS_LIST
        }
        value if value == REG_QWORD.clone() as u32 => REG_QWORD,
        _ => bail!("unsupported registry type {value}"),
    };
    Ok(value)
}

#[cfg(windows)]
fn broadcast_environment_change() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };

    let environment: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
    let mut result = 0;
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            environment.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
}

fn present(value: bool) -> &'static str {
    if value {
        "present"
    } else {
        "missing"
    }
}

fn healthy(value: bool) -> &'static str {
    if value {
        "healthy"
    } else {
        "broken"
    }
}

fn unix_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "codex-image-fix-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    fn transaction_for(
        root: &Path,
        previous_state: FileBackup,
        previous_alias: FileBackup,
        launcher_existed: bool,
        proxy_existed: bool,
    ) -> InstallTransaction {
        InstallTransaction {
            version: TRANSACTION_VERSION,
            phase: InstallTransactionPhase::Applying,
            state_path: root.join("state.json"),
            alias_path: root.join("alias.cmd"),
            launcher_path: root.join("codex.exe"),
            proxy_path: root.join("codex-image-fix-new.exe"),
            previous_state,
            previous_alias,
            previous_environment: RegistryBackup::default(),
            previous_guardian_run: RegistryBackup::default(),
            applied_guardian_command: guardian_command(&root.join("codex-image-fix-new.exe")),
            applied_state_sha256: sha256(b"new state"),
            applied_alias_sha256: sha256(b"new alias"),
            launcher_sha256: sha256(b"launcher"),
            proxy_sha256: sha256(b"new proxy"),
            launcher_existed,
            proxy_existed,
        }
    }

    fn paths_for_transaction(root: &Path, transaction: &InstallTransaction) -> InstallPaths {
        InstallPaths {
            state: transaction.state_path.clone(),
            transaction: root.join(TRANSACTION_FILE_NAME),
            legacy_state: root.join("backup.json"),
            launcher: transaction.launcher_path.clone(),
            proxy: root.join("legacy-proxy.exe"),
            alias: transaction.alias_path.clone(),
            root: root.to_path_buf(),
        }
    }

    #[cfg(windows)]
    fn state_for_launcher(launcher: &Path, installed_launcher_sha256: &str) -> InstallState {
        InstallState {
            version: STATE_VERSION,
            installed_at_unix: 1,
            real_cli: launcher.with_file_name("real-codex.exe"),
            launcher: launcher.to_path_buf(),
            proxy: launcher.with_file_name("codex-image-fix-old.exe"),
            alias: launcher.with_file_name("alias.cmd"),
            installed_alias_sha256: String::new(),
            installed_proxy_sha256: String::new(),
            installed_launcher_sha256: installed_launcher_sha256.to_owned(),
            original_codex_cli_path: RegistryBackup::default(),
            original_alias: FileBackup::default(),
            original_guardian_run: RegistryBackup::default(),
            installed_guardian_command: String::new(),
        }
    }

    #[cfg(windows)]
    #[test]
    fn optional_codex_helpers_support_single_binary_distributions() {
        let root = test_root("optional-codex-helpers");
        fs::create_dir_all(&root).unwrap();
        let mut validated = Vec::new();
        validate_optional_codex_helpers(&root, |path| {
            validated.push(path.file_name().unwrap().to_owned());
            Ok(())
        })
        .unwrap();
        assert!(validated.is_empty());

        fs::write(root.join("codex-command-runner.exe"), b"signed helper").unwrap();
        validate_optional_codex_helpers(&root, |path| {
            validated.push(path.file_name().unwrap().to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0], OsStr::new("codex-command-runner.exe"));

        fs::create_dir(root.join("codex-windows-sandbox-setup.exe")).unwrap();
        assert!(validate_optional_codex_helpers(&root, |_| Ok(())).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn managed_launcher_uses_state_hash_when_catalog_signature_is_unavailable() {
        let root = test_root("managed-launcher-catalog");
        fs::create_dir_all(&root).unwrap();
        let launcher = root.join("codex.exe");
        fs::write(&launcher, b"managed launcher").unwrap();
        let expected_sha256 = file_sha256(&launcher).unwrap();
        let state = state_for_launcher(&launcher, &expected_sha256);
        let mut signature_checked = false;

        let actual_sha256 = validated_existing_launcher_sha256(&launcher, Some(&state), |_| {
            signature_checked = true;
            bail!("catalog signature is unavailable")
        })
        .unwrap()
        .unwrap();

        assert_eq!(actual_sha256, expected_sha256);
        assert!(!signature_checked);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn managed_launcher_hash_mismatch_is_rejected_without_signature_fallback() {
        let root = test_root("managed-launcher-mismatch");
        fs::create_dir_all(&root).unwrap();
        let launcher = root.join("codex.exe");
        fs::write(&launcher, b"changed launcher").unwrap();
        let state = state_for_launcher(&launcher, &sha256(b"expected launcher"));
        let mut signature_checked = false;

        let error = validated_existing_launcher_sha256(&launcher, Some(&state), |_| {
            signature_checked = true;
            Ok(())
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("managed launcher changed"));
        assert!(!signature_checked);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn unmanaged_launcher_still_requires_signature_validation() {
        let root = test_root("unmanaged-launcher");
        fs::create_dir_all(&root).unwrap();
        let launcher = root.join("codex.exe");
        fs::write(&launcher, b"unmanaged launcher").unwrap();
        let mut signature_checked = false;

        let error = validated_existing_launcher_sha256(&launcher, None, |_| {
            signature_checked = true;
            bail!("signature validation failed")
        })
        .unwrap_err();

        assert!(signature_checked);
        assert!(format!("{error:#}").contains("signature validation failed"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailurePoint {
        ProxyCopy,
        LauncherCopy,
        StateWrite,
        AliasWrite,
        RegistryWrite,
        GuardianRegistryWrite,
        SelfCheck,
    }

    #[cfg(windows)]
    struct FaultBackend {
        failure: FailurePoint,
        proxy_path: PathBuf,
        launcher_path: PathBuf,
        state_path: PathBuf,
        alias_path: PathBuf,
        environment: RegistryBackup,
        current_cli_path: Option<String>,
        guardian_run: RegistryBackup,
        current_guardian_run: Option<String>,
    }

    #[cfg(windows)]
    impl FaultBackend {
        fn fail_at(&self, point: FailurePoint) -> Result<()> {
            if self.failure == point {
                bail!("injected failure at {point:?}");
            }
            Ok(())
        }

        fn restore_environment(&mut self, transaction: &InstallTransaction) -> Result<()> {
            restore_transaction_environment_with(self, transaction)?;
            restore_transaction_guardian_with(self, transaction)
        }
    }

    #[cfg(windows)]
    impl InstallBackend for FaultBackend {
        fn copy_file(&mut self, source: &Path, destination: &Path) -> Result<()> {
            if destination == self.proxy_path {
                self.fail_at(FailurePoint::ProxyCopy)?;
            } else if destination == self.launcher_path {
                self.fail_at(FailurePoint::LauncherCopy)?;
            }
            atomic_copy(source, destination)
        }

        fn validate_launcher(&mut self, path: &Path, expected_sha256: &str) -> Result<()> {
            validate_expected_sha256(path, expected_sha256, "launcher")
        }

        fn write_file(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
            if path == self.state_path {
                self.fail_at(FailurePoint::StateWrite)?;
            } else if path == self.alias_path {
                self.fail_at(FailurePoint::AliasWrite)?;
            }
            atomic_write(path, bytes)
        }

        fn set_cli_path(&mut self, path: &Path) -> Result<()> {
            self.fail_at(FailurePoint::RegistryWrite)?;
            self.environment = registry_string_backup(path.to_string_lossy().as_ref());
            self.current_cli_path = Some(path.to_string_lossy().into_owned());
            Ok(())
        }

        fn set_guardian_run(&mut self, command: &str) -> Result<()> {
            self.fail_at(FailurePoint::GuardianRegistryWrite)?;
            self.guardian_run = registry_string_backup(command);
            self.current_guardian_run = Some(command.to_owned());
            Ok(())
        }

        fn broadcast_environment_change(&mut self) {}

        fn verify_launcher(&mut self, _path: &Path) -> Result<()> {
            self.fail_at(FailurePoint::SelfCheck)
        }

        fn write_transaction(
            &mut self,
            path: &Path,
            transaction: &InstallTransaction,
        ) -> Result<()> {
            write_install_transaction(path, transaction)
        }
    }

    #[cfg(windows)]
    impl EnvironmentBackend for FaultBackend {
        fn read_cli_path_backup(&mut self) -> Result<RegistryBackup> {
            Ok(self.environment.clone())
        }

        fn current_cli_path(&mut self) -> Result<Option<String>> {
            Ok(self.current_cli_path.clone())
        }

        fn restore_cli_path(&mut self, backup: &RegistryBackup) -> Result<()> {
            self.environment = backup.clone();
            self.current_cli_path = Some("old-cli".to_owned());
            Ok(())
        }

        fn read_guardian_run_backup(&mut self) -> Result<RegistryBackup> {
            Ok(self.guardian_run.clone())
        }

        fn current_guardian_run(&mut self) -> Result<Option<String>> {
            Ok(self.current_guardian_run.clone())
        }

        fn restore_guardian_run(&mut self, backup: &RegistryBackup) -> Result<()> {
            self.guardian_run = backup.clone();
            self.current_guardian_run = backup.existed.then(|| "old-guardian".to_owned());
            Ok(())
        }

        fn broadcast_environment_change(&mut self) {}
    }

    #[cfg(windows)]
    fn fault_backend(failure: FailurePoint, transaction: &InstallTransaction) -> FaultBackend {
        FaultBackend {
            failure,
            proxy_path: transaction.proxy_path.clone(),
            launcher_path: transaction.launcher_path.clone(),
            state_path: transaction.state_path.clone(),
            alias_path: transaction.alias_path.clone(),
            environment: transaction.previous_environment.clone(),
            current_cli_path: Some("old-cli".to_owned()),
            guardian_run: transaction.previous_guardian_run.clone(),
            current_guardian_run: Some("old-guardian".to_owned()),
        }
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let root = test_root("atomic-write");
        let destination = root.join("state.json");
        atomic_write(&destination, b"first").unwrap();
        atomic_write(&destination, b"second").unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"second");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn alias_requests_console_hiding_before_starting_proxy() {
        let alias = alias_contents("codex-image-fix-test.exe");
        let marker = alias.find(HIDE_CONSOLE_ENV).unwrap();
        let proxy = alias.find("codex-image-fix-test.exe").unwrap();
        assert!(marker < proxy);
    }

    #[test]
    fn versioned_proxy_path_uses_content_hash() {
        let root = Path::new(r"C:\fix");
        assert_eq!(
            versioned_proxy_path(root, "abc"),
            root.join("codex-image-fix-abc.exe")
        );
    }

    #[test]
    fn automatic_registry_repair_only_accepts_missing_or_empty_values() {
        let expected = r"C:\fix\codex.exe";
        assert!(automatic_registry_entry_repair_needed("entry", None, expected).unwrap());
        assert!(automatic_registry_entry_repair_needed("entry", Some("  "), expected).unwrap());
        assert!(
            !automatic_registry_entry_repair_needed("entry", Some(expected), expected).unwrap()
        );
        let error =
            automatic_registry_entry_repair_needed("entry", Some(r"C:\other\codex.exe"), expected)
                .unwrap_err();
        assert!(format!("{error:#}").contains("refusing to overwrite"));
        assert!(automatic_registry_entry_repair_needed("entry", None, "").is_err());
    }

    #[test]
    fn guardian_ownership_requires_the_exact_installed_command() {
        let installed = r#""C:\fix\proxy.exe" guardian"#;
        assert!(guardian_run_is_owned(Some(installed), installed));
        assert!(!guardian_run_is_owned(
            Some(r#""C:\other\agent.exe" guardian"#),
            installed
        ));
        assert!(!guardian_run_is_owned(None, installed));
        assert!(!guardian_run_is_owned(Some(""), ""));
    }

    #[test]
    fn runtime_state_distinguishes_ready_connected_restart_and_broken() {
        assert_eq!(
            runtime_state_for(InstallationHealth::Healthy, true, false, false, false),
            RuntimeState::Ready
        );
        assert_eq!(
            runtime_state_for(InstallationHealth::Healthy, true, true, true, false),
            RuntimeState::Connected
        );
        assert_eq!(
            runtime_state_for(InstallationHealth::Healthy, true, true, false, true),
            RuntimeState::RestartRequired
        );
        assert_eq!(
            runtime_state_for(InstallationHealth::Healthy, true, true, false, false),
            RuntimeState::Broken
        );
        assert_eq!(
            runtime_state_for(InstallationHealth::Healthy, false, false, false, false),
            RuntimeState::Broken
        );
    }

    #[test]
    fn v03_state_defaults_guardian_fields_during_upgrade() {
        let current = InstallState {
            version: STATE_VERSION,
            installed_at_unix: 1,
            real_cli: r"C:\real\codex.exe".into(),
            launcher: r"C:\fix\codex.exe".into(),
            proxy: r"C:\fix\proxy.exe".into(),
            alias: r"C:\alias.cmd".into(),
            installed_alias_sha256: "alias".to_owned(),
            installed_proxy_sha256: "proxy".to_owned(),
            installed_launcher_sha256: "launcher".to_owned(),
            original_codex_cli_path: RegistryBackup::default(),
            original_alias: FileBackup::default(),
            original_guardian_run: RegistryBackup::default(),
            installed_guardian_command: "guardian".to_owned(),
        };
        let mut value = serde_json::to_value(current).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("installedLauncherSha256");
        object.remove("originalGuardianRun");
        object.remove("installedGuardianCommand");

        let legacy: InstallState = serde_json::from_value(value).unwrap();

        assert!(legacy.installed_launcher_sha256.is_empty());
        assert_eq!(legacy.original_guardian_run, RegistryBackup::default());
        assert!(legacy.installed_guardian_command.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn operation_lock_is_recursive_and_cross_thread_exclusive() {
        let first = OperationLock::acquire().unwrap();
        let nested = OperationLock::acquire().unwrap();
        drop(nested);

        let refused = thread::spawn(|| OperationLock::acquire().is_err())
            .join()
            .unwrap();
        assert!(refused);

        drop(first);
        OperationLock::acquire().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn fresh_install_failure_matrix_restores_files_and_environment() {
        for failure in [
            FailurePoint::ProxyCopy,
            FailurePoint::LauncherCopy,
            FailurePoint::StateWrite,
            FailurePoint::AliasWrite,
            FailurePoint::RegistryWrite,
            FailurePoint::GuardianRegistryWrite,
            FailurePoint::SelfCheck,
        ] {
            let root = test_root(&format!("fresh-{failure:?}"));
            fs::create_dir_all(&root).unwrap();
            let proxy_source = root.join("proxy-source.exe");
            let launcher_source = root.join("launcher-source.exe");
            fs::write(&proxy_source, b"new proxy").unwrap();
            fs::write(&launcher_source, b"launcher").unwrap();
            let mut transaction = transaction_for(
                &root,
                FileBackup::default(),
                FileBackup::default(),
                false,
                false,
            );
            transaction.previous_environment = registry_string_backup("old-cli");
            transaction.previous_guardian_run = registry_string_backup("old-guardian");
            let transaction_path = root.join(TRANSACTION_FILE_NAME);
            write_install_transaction(&transaction_path, &transaction).unwrap();
            let mut backend = fault_backend(failure, &transaction);

            let error = apply_install_transaction(
                &mut backend,
                &proxy_source,
                &launcher_source,
                b"new state",
                b"new alias",
                &transaction_path,
                &mut transaction,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("injected failure"));
            rollback_transaction_files(&transaction).unwrap();
            backend.restore_environment(&transaction).unwrap();

            assert!(!transaction.state_path.exists(), "failure={failure:?}");
            assert!(!transaction.alias_path.exists(), "failure={failure:?}");
            assert!(!transaction.launcher_path.exists(), "failure={failure:?}");
            assert!(!transaction.proxy_path.exists(), "failure={failure:?}");
            assert_eq!(backend.environment, transaction.previous_environment);
            assert_eq!(backend.current_cli_path.as_deref(), Some("old-cli"));
            assert_eq!(backend.guardian_run, transaction.previous_guardian_run);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn install_transaction_rejects_launcher_hash_mismatch() {
        let root = test_root("launcher-transaction-hash");
        fs::create_dir_all(&root).unwrap();
        let proxy_source = root.join("proxy-source.exe");
        let launcher_source = root.join("launcher-source.exe");
        fs::write(&proxy_source, b"new proxy").unwrap();
        fs::write(&launcher_source, b"unexpected launcher").unwrap();
        let mut transaction = transaction_for(
            &root,
            FileBackup::default(),
            FileBackup::default(),
            false,
            false,
        );
        let transaction_path = root.join(TRANSACTION_FILE_NAME);
        let mut backend = fault_backend(FailurePoint::SelfCheck, &transaction);

        let error = apply_install_transaction(
            &mut backend,
            &proxy_source,
            &launcher_source,
            b"new state",
            b"new alias",
            &transaction_path,
            &mut transaction,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("launcher integrity check failed"));
        assert!(!transaction.state_path.exists());
        assert!(!transaction.alias_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn upgrade_failure_matrix_restores_previous_installation() {
        for failure in [
            FailurePoint::ProxyCopy,
            FailurePoint::StateWrite,
            FailurePoint::AliasWrite,
            FailurePoint::RegistryWrite,
            FailurePoint::GuardianRegistryWrite,
            FailurePoint::SelfCheck,
        ] {
            let root = test_root(&format!("upgrade-{failure:?}"));
            fs::create_dir_all(&root).unwrap();
            let proxy_source = root.join("proxy-source.exe");
            let launcher_source = root.join("launcher-source.exe");
            let state_path = root.join("state.json");
            let alias_path = root.join("alias.cmd");
            let launcher_path = root.join("codex.exe");
            let previous_proxy_path = root.join("codex-image-fix-old.exe");
            fs::write(&proxy_source, b"new proxy").unwrap();
            fs::write(&launcher_source, b"launcher").unwrap();
            fs::write(&state_path, b"old state").unwrap();
            fs::write(&alias_path, b"old alias").unwrap();
            fs::write(&launcher_path, b"launcher").unwrap();
            fs::write(&previous_proxy_path, b"old proxy").unwrap();
            let mut transaction = transaction_for(
                &root,
                capture_file_backup(&state_path).unwrap(),
                capture_file_backup(&alias_path).unwrap(),
                true,
                false,
            );
            transaction.previous_environment = registry_string_backup("old-cli");
            transaction.previous_guardian_run = registry_string_backup("old-guardian");
            let transaction_path = root.join(TRANSACTION_FILE_NAME);
            write_install_transaction(&transaction_path, &transaction).unwrap();
            let mut backend = fault_backend(failure, &transaction);

            let error = apply_install_transaction(
                &mut backend,
                &proxy_source,
                &launcher_source,
                b"new state",
                b"new alias",
                &transaction_path,
                &mut transaction,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("injected failure"));
            rollback_transaction_files(&transaction).unwrap();
            backend.restore_environment(&transaction).unwrap();

            assert_eq!(fs::read(&state_path).unwrap(), b"old state");
            assert_eq!(fs::read(&alias_path).unwrap(), b"old alias");
            assert_eq!(fs::read(&launcher_path).unwrap(), b"launcher");
            assert_eq!(fs::read(&previous_proxy_path).unwrap(), b"old proxy");
            assert!(!transaction.proxy_path.exists(), "failure={failure:?}");
            assert_eq!(backend.environment, transaction.previous_environment);
            assert_eq!(backend.current_cli_path.as_deref(), Some("old-cli"));
            assert_eq!(backend.guardian_run, transaction.previous_guardian_run);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn environment_rollback_refuses_external_registry_changes() {
        let root = Path::new(r"C:\fix");
        let mut transaction = transaction_for(
            root,
            FileBackup::default(),
            FileBackup::default(),
            false,
            false,
        );
        transaction.previous_environment = registry_string_backup("old-cli");
        let external = registry_string_backup("external-cli");

        let error =
            transaction_environment_needs_restore(&external, Some("external-cli"), &transaction)
                .unwrap_err();

        assert!(format!("{error:#}").contains("refusing to overwrite"));
    }

    #[cfg(windows)]
    #[test]
    fn guardian_rollback_refuses_external_registry_changes() {
        let root = Path::new(r"C:\fix");
        let mut transaction = transaction_for(
            root,
            FileBackup::default(),
            FileBackup::default(),
            false,
            false,
        );
        transaction.previous_guardian_run = registry_string_backup("old-guardian");
        let mut backend = fault_backend(FailurePoint::SelfCheck, &transaction);
        backend.guardian_run = registry_string_backup("external-guardian");
        backend.current_guardian_run = Some("external-guardian".to_owned());

        let error = restore_transaction_guardian_with(&mut backend, &transaction).unwrap_err();

        assert!(format!("{error:#}").contains("refusing to overwrite"));
        assert_eq!(
            backend.current_guardian_run.as_deref(),
            Some("external-guardian")
        );
    }

    #[cfg(windows)]
    #[test]
    fn interrupted_applying_transaction_recovers_on_next_start() {
        let root = test_root("interrupted-applying");
        fs::create_dir_all(&root).unwrap();
        let mut transaction = transaction_for(
            &root,
            FileBackup::default(),
            FileBackup::default(),
            false,
            false,
        );
        transaction.previous_environment = registry_string_backup("old-cli");
        let paths = paths_for_transaction(&root, &transaction);
        fs::write(&transaction.state_path, b"new state").unwrap();
        fs::write(&transaction.alias_path, b"new alias").unwrap();
        fs::write(&transaction.launcher_path, b"launcher").unwrap();
        fs::write(&transaction.proxy_path, b"new proxy").unwrap();
        write_install_transaction(&paths.transaction, &transaction).unwrap();
        let mut backend = fault_backend(FailurePoint::SelfCheck, &transaction);
        backend.environment =
            registry_string_backup(transaction.launcher_path.to_string_lossy().as_ref());
        backend.current_cli_path = Some(transaction.launcher_path.to_string_lossy().into_owned());

        recover_pending_install_with(&paths, &mut backend).unwrap();

        assert!(!paths.transaction.exists());
        assert!(!transaction.state_path.exists());
        assert!(!transaction.alias_path.exists());
        assert!(!transaction.launcher_path.exists());
        assert!(!transaction.proxy_path.exists());
        assert_eq!(backend.environment, transaction.previous_environment);
        assert_eq!(backend.current_cli_path.as_deref(), Some("old-cli"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn interrupted_recovery_preserves_external_changes_and_journal() {
        let root = test_root("interrupted-external-change");
        fs::create_dir_all(&root).unwrap();
        let mut transaction = transaction_for(
            &root,
            FileBackup::default(),
            FileBackup::default(),
            false,
            false,
        );
        transaction.previous_environment = registry_string_backup("old-cli");
        let paths = paths_for_transaction(&root, &transaction);
        fs::write(&transaction.state_path, b"external state").unwrap();
        fs::write(&transaction.alias_path, b"new alias").unwrap();
        fs::write(&transaction.launcher_path, b"launcher").unwrap();
        fs::write(&transaction.proxy_path, b"new proxy").unwrap();
        write_install_transaction(&paths.transaction, &transaction).unwrap();
        let mut backend = fault_backend(FailurePoint::SelfCheck, &transaction);
        backend.environment =
            registry_string_backup(transaction.launcher_path.to_string_lossy().as_ref());
        backend.current_cli_path = Some(transaction.launcher_path.to_string_lossy().into_owned());

        let error = recover_pending_install_with(&paths, &mut backend).unwrap_err();

        assert!(format!("{error:#}").contains("refusing to overwrite"));
        assert_eq!(
            fs::read(&transaction.state_path).unwrap(),
            b"external state"
        );
        assert!(paths.transaction.is_file());
        assert_eq!(backend.environment, transaction.previous_environment);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_install_rollback_removes_all_created_files() {
        let root = test_root("fresh-rollback");
        fs::create_dir_all(&root).unwrap();
        let transaction = transaction_for(
            &root,
            FileBackup::default(),
            FileBackup::default(),
            false,
            false,
        );
        fs::write(&transaction.state_path, b"new state").unwrap();
        fs::write(&transaction.alias_path, b"new alias").unwrap();
        fs::write(&transaction.launcher_path, b"launcher").unwrap();
        fs::write(&transaction.proxy_path, b"new proxy").unwrap();

        rollback_transaction_files(&transaction).unwrap();

        assert!(!transaction.state_path.exists());
        assert!(!transaction.alias_path.exists());
        assert!(!transaction.launcher_path.exists());
        assert!(!transaction.proxy_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgrade_rollback_restores_previous_installation() {
        let root = test_root("upgrade-rollback");
        fs::create_dir_all(&root).unwrap();
        let state_path = root.join("state.json");
        let alias_path = root.join("alias.cmd");
        let launcher_path = root.join("codex.exe");
        let previous_proxy_path = root.join("codex-image-fix-old.exe");
        fs::write(&state_path, b"old state").unwrap();
        fs::write(&alias_path, b"old alias").unwrap();
        fs::write(&launcher_path, b"launcher").unwrap();
        fs::write(&previous_proxy_path, b"old proxy").unwrap();
        let previous_state = capture_file_backup(&state_path).unwrap();
        let previous_alias = capture_file_backup(&alias_path).unwrap();
        let transaction = transaction_for(&root, previous_state, previous_alias, true, false);
        fs::write(&transaction.state_path, b"new state").unwrap();
        fs::write(&transaction.alias_path, b"new alias").unwrap();
        fs::write(&transaction.proxy_path, b"new proxy").unwrap();

        rollback_transaction_files(&transaction).unwrap();

        assert_eq!(fs::read(&state_path).unwrap(), b"old state");
        assert_eq!(fs::read(&alias_path).unwrap(), b"old alias");
        assert_eq!(fs::read(&launcher_path).unwrap(), b"launcher");
        assert_eq!(fs::read(&previous_proxy_path).unwrap(), b"old proxy");
        assert!(!transaction.proxy_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn committed_transaction_is_finalized_without_rollback() {
        let root = test_root("committed-transaction");
        fs::create_dir_all(&root).unwrap();
        let mut transaction = transaction_for(
            &root,
            FileBackup::default(),
            FileBackup::default(),
            false,
            false,
        );
        transaction.phase = InstallTransactionPhase::Committed;
        fs::write(&transaction.state_path, b"new state").unwrap();
        let paths = InstallPaths {
            state: transaction.state_path.clone(),
            transaction: root.join(TRANSACTION_FILE_NAME),
            legacy_state: root.join("backup.json"),
            launcher: transaction.launcher_path.clone(),
            proxy: root.join("legacy-proxy.exe"),
            alias: transaction.alias_path.clone(),
            root: root.clone(),
        };
        write_install_transaction(&paths.transaction, &transaction).unwrap();

        recover_pending_install(&paths).unwrap();

        assert!(!paths.transaction.exists());
        assert_eq!(fs::read(&paths.state).unwrap(), b"new state");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollback_refuses_to_overwrite_external_changes() {
        let root = test_root("external-change");
        fs::create_dir_all(&root).unwrap();
        let state_path = root.join("state.json");
        fs::write(&state_path, b"old state").unwrap();
        let previous_state = capture_file_backup(&state_path).unwrap();
        let transaction =
            transaction_for(&root, previous_state, FileBackup::default(), false, false);
        fs::write(&transaction.state_path, b"external state").unwrap();

        let error = rollback_transaction_files(&transaction).unwrap_err();

        assert!(format!("{error:#}").contains("refusing to overwrite"));
        assert_eq!(fs::read(&state_path).unwrap(), b"external state");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn partial_uninstall_preserves_model_state_and_removes_proxy_files() {
        let root = test_root("partial-uninstall");
        fs::create_dir_all(&root).unwrap();
        let paths = InstallPaths {
            state: root.join("state.json"),
            transaction: root.join(TRANSACTION_FILE_NAME),
            legacy_state: root.join("backup.json"),
            launcher: root.join("codex.exe"),
            proxy: root.join("legacy-proxy.exe"),
            alias: root.join("alias.cmd"),
            root: root.clone(),
        };
        let proxy = root.join("codex-image-fix-new.exe");
        fs::write(&proxy, b"new proxy").unwrap();
        fs::copy(system_powershell().unwrap(), &paths.launcher).unwrap();
        let state = InstallState {
            version: STATE_VERSION,
            installed_at_unix: 1,
            real_cli: root.join("real-codex.exe"),
            launcher: paths.launcher.clone(),
            proxy: proxy.clone(),
            alias: paths.alias.clone(),
            installed_alias_sha256: sha256(b"alias"),
            installed_proxy_sha256: sha256(b"new proxy"),
            installed_launcher_sha256: file_sha256(&paths.launcher).unwrap(),
            original_codex_cli_path: RegistryBackup::default(),
            original_alias: FileBackup::default(),
            original_guardian_run: RegistryBackup::default(),
            installed_guardian_command: guardian_command(&proxy),
        };
        fs::write(&paths.state, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        let state_sha256 = file_sha256(&paths.state).unwrap();
        let model_state = root.join("model-config-state.json");
        fs::write(&model_state, b"model restore snapshot").unwrap();

        remove_proxy_files_preserving_model_state_with_validator(
            &paths,
            &state,
            &state_sha256,
            |_| Ok(()),
        )
        .unwrap();

        assert!(model_state.is_file());
        assert!(!paths.state.exists());
        assert!(!paths.launcher.exists());
        assert!(!proxy.exists());
        assert!(root.is_dir());
        fs::remove_dir_all(root).unwrap();
    }
}
