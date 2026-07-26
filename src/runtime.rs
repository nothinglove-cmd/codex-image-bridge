use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const RUNTIME_VERSION: u32 = 1;
const RUNTIME_DIRECTORY: &str = "runtime";
const RESTART_MARKER: &str = "restart-required.marker";
const MAX_RECORD_BYTES: u64 = 8 * 1024;
const HEARTBEAT_INTERVAL_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeKind {
    Proxy,
    Guardian,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRecord {
    version: u32,
    pub kind: RuntimeKind,
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub program_version: String,
    pub started_at_unix_ms: u64,
    pub last_request_unix_ms: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActiveRuntime {
    pub proxy_pids: Vec<u32>,
    pub guardian_pids: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProcessIdentity {
    pid: u32,
    started_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestartMarker {
    version: u32,
    repaired_at_unix_ms: u64,
    affected_processes: Vec<ProcessIdentity>,
}

pub struct RuntimeRegistration {
    path: PathBuf,
    record: Mutex<RuntimeRecord>,
}

impl RuntimeRegistration {
    pub fn register(kind: RuntimeKind) -> Result<Self> {
        let directory = runtime_directory()?;
        fs::create_dir_all(&directory)?;
        let pid = std::process::id();
        let now = unix_time_millis();
        let record = RuntimeRecord {
            version: RUNTIME_VERSION,
            kind,
            pid,
            parent_pid: parent_process_id(pid),
            program_version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at_unix_ms: process_start_unix_ms(pid).unwrap_or(now),
            last_request_unix_ms: now,
        };
        let path = directory.join(format!("{pid}.json"));
        write_json(&path, &record)?;
        if kind == RuntimeKind::Proxy {
            acknowledge_restart_for_proxy(record.started_at_unix_ms);
        }
        Ok(Self {
            path,
            record: Mutex::new(record),
        })
    }

    pub fn heartbeat(&self) {
        let now = unix_time_millis();
        let Ok(mut record) = self.record.lock() else {
            return;
        };
        if now.saturating_sub(record.last_request_unix_ms) < HEARTBEAT_INTERVAL_MS {
            return;
        }
        if !self.path.parent().is_some_and(Path::is_dir) {
            return;
        }
        record.last_request_unix_ms = now;
        let _ = write_json(&self.path, &*record);
    }
}

impl Drop for RuntimeRegistration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn active_runtime(expected_executable: &Path) -> Result<ActiveRuntime> {
    let directory = runtime_directory()?;
    active_runtime_in(&directory, expected_executable)
}

fn active_runtime_in(directory: &Path, expected_executable: &Path) -> Result<ActiveRuntime> {
    if !directory.is_dir() {
        return Ok(ActiveRuntime::default());
    }
    let mut active = ActiveRuntime::default();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let record = read_runtime_record(&path).ok();
        let valid = record.as_ref().is_some_and(|record| {
            record.version == RUNTIME_VERSION
                && path.file_stem().and_then(|value| value.to_str())
                    == Some(record.pid.to_string().as_str())
                && process_matches_record(record, expected_executable)
        });
        if !valid {
            let _ = fs::remove_file(path);
            continue;
        }
        let record = record.expect("record was checked above");
        match record.kind {
            RuntimeKind::Proxy => active.proxy_pids.push(record.pid),
            RuntimeKind::Guardian => active.guardian_pids.push(record.pid),
        }
    }
    active.proxy_pids.sort_unstable();
    active.guardian_pids.sort_unstable();
    Ok(active)
}

pub fn mark_restart_required(process_ids: &[u32]) -> Result<()> {
    let path = restart_marker_path()?;
    let affected_processes = process_ids
        .iter()
        .filter_map(|pid| {
            process_start_unix_ms(*pid).map(|started_at_unix_ms| ProcessIdentity {
                pid: *pid,
                started_at_unix_ms,
            })
        })
        .collect::<Vec<_>>();
    if affected_processes.is_empty() {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    let marker = RestartMarker {
        version: RUNTIME_VERSION,
        repaired_at_unix_ms: unix_time_millis(),
        affected_processes,
    };
    fs::create_dir_all(path.parent().context("restart marker has no parent")?)?;
    write_json(&path, &marker)
}

pub fn restart_required() -> Result<bool> {
    let path = restart_marker_path()?;
    if !path.is_file() {
        return Ok(false);
    }
    let marker: RestartMarker = match read_bounded_json(&path) {
        Ok(marker) => marker,
        Err(_) => {
            let _ = fs::remove_file(path);
            return Ok(false);
        }
    };
    let required = marker.version == RUNTIME_VERSION
        && marker.affected_processes.iter().any(process_identity_alive);
    if !required {
        let _ = fs::remove_file(path);
    }
    Ok(required)
}

fn acknowledge_restart_for_proxy(proxy_started_at_unix_ms: u64) {
    let Ok(path) = restart_marker_path() else {
        return;
    };
    if !path.is_file() {
        return;
    }
    let marker: RestartMarker = match read_bounded_json(&path) {
        Ok(marker) => marker,
        Err(_) => {
            let _ = fs::remove_file(path);
            return;
        }
    };
    if marker.version != RUNTIME_VERSION
        || proxy_satisfies_restart(&marker, proxy_started_at_unix_ms)
    {
        let _ = fs::remove_file(path);
    }
}

fn proxy_satisfies_restart(marker: &RestartMarker, proxy_started_at_unix_ms: u64) -> bool {
    proxy_started_at_unix_ms >= marker.repaired_at_unix_ms
}

pub fn clear_all() -> Result<()> {
    let directory = runtime_directory()?;
    if directory.is_dir() {
        fs::remove_dir_all(&directory)
            .with_context(|| format!("failed to remove {}", directory.display()))?;
    }
    Ok(())
}

fn read_runtime_record(path: &Path) -> Result<RuntimeRecord> {
    read_bounded_json(path)
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_RECORD_BYTES {
        anyhow::bail!("runtime record is too large");
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let parent = path.parent().context("runtime record has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    {
        let mut file = fs::File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    replace_file(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("atomic runtime record replace failed");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).context("atomic runtime record replace failed")
}

fn runtime_directory() -> Result<PathBuf> {
    Ok(crate::install::fix_root()?.join(RUNTIME_DIRECTORY))
}

fn restart_marker_path() -> Result<PathBuf> {
    Ok(runtime_directory()?.join(RESTART_MARKER))
}

fn process_matches_record(record: &RuntimeRecord, expected_executable: &Path) -> bool {
    process_start_unix_ms(record.pid)
        .is_some_and(|started| started.abs_diff(record.started_at_unix_ms) <= 2_000)
        && process_image(record.pid)
            .is_some_and(|path| normalized_path(&path) == normalized_path(expected_executable))
}

fn process_identity_alive(identity: &ProcessIdentity) -> bool {
    process_start_unix_ms(identity.pid)
        .is_some_and(|started| started.abs_diff(identity.started_at_unix_ms) <= 2_000)
}

#[cfg(windows)]
fn process_image(pid: u32) -> Option<PathBuf> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let succeeded =
        unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe { CloseHandle(process) };
    (succeeded != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])))
}

#[cfg(not(windows))]
fn process_image(pid: u32) -> Option<PathBuf> {
    (pid == std::process::id())
        .then(std::env::current_exe)
        .and_then(Result::ok)
}

#[cfg(windows)]
fn process_start_unix_ms(pid: u32) -> Option<u64> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let succeeded =
        unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) };
    unsafe { CloseHandle(process) };
    if succeeded == 0 {
        return None;
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Some(ticks.saturating_sub(116_444_736_000_000_000) / 10_000)
}

#[cfg(not(windows))]
fn process_start_unix_ms(pid: u32) -> Option<u64> {
    (pid == std::process::id()).then(unix_time_millis)
}

#[cfg(windows)]
fn parent_process_id(pid: u32) -> Option<u32> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = None;
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        if entry.th32ProcessID == pid {
            found = Some(entry.th32ParentProcessID);
            break;
        }
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    found
}

#[cfg(not(windows))]
fn parent_process_id(_pid: u32) -> Option<u32> {
    None
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('/', r"\")
        .to_ascii_lowercase()
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_record_contains_no_session_or_secret_fields() {
        let record = RuntimeRecord {
            version: RUNTIME_VERSION,
            kind: RuntimeKind::Proxy,
            pid: 42,
            parent_pid: Some(7),
            program_version: "0.4.0".to_owned(),
            started_at_unix_ms: 100,
            last_request_unix_ms: 200,
        };
        let json = serde_json::to_string(&record).unwrap();
        for forbidden in ["thread", "session", "prompt", "base64", "apiKey", "header"] {
            assert!(!json
                .to_ascii_lowercase()
                .contains(&forbidden.to_ascii_lowercase()));
        }
    }

    #[test]
    fn normalized_windows_paths_compare_case_insensitively() {
        assert_eq!(
            normalized_path(Path::new(r"\\?\C:\Fix\Proxy.exe")),
            normalized_path(Path::new(r"c:\fix\proxy.exe"))
        );
    }

    #[test]
    fn post_repair_proxy_acknowledges_restart_marker() {
        let root = std::env::temp_dir().join(format!(
            "codex-image-restart-{}-{}",
            std::process::id(),
            unix_time_millis()
        ));
        fs::create_dir_all(&root).unwrap();
        let marker_path = root.join(RESTART_MARKER);
        let marker = RestartMarker {
            version: RUNTIME_VERSION,
            repaired_at_unix_ms: 200,
            affected_processes: Vec::new(),
        };
        write_json(&marker_path, &marker).unwrap();

        let loaded: RestartMarker = read_bounded_json(&marker_path).unwrap();
        assert!(!proxy_satisfies_restart(&loaded, 199));
        assert!(proxy_satisfies_restart(&loaded, 200));
        assert!(proxy_satisfies_restart(&loaded, 201));

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn active_runtime_rejects_stale_process_records() {
        let root = std::env::temp_dir().join(format!(
            "codex-image-runtime-{}-{}",
            std::process::id(),
            unix_time_millis()
        ));
        fs::create_dir_all(&root).unwrap();
        let pid = std::process::id();
        let record = RuntimeRecord {
            version: RUNTIME_VERSION,
            kind: RuntimeKind::Proxy,
            pid,
            parent_pid: parent_process_id(pid),
            program_version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at_unix_ms: process_start_unix_ms(pid).unwrap(),
            last_request_unix_ms: unix_time_millis(),
        };
        write_json(&root.join(format!("{pid}.json")), &record).unwrap();
        let stale = RuntimeRecord {
            pid: u32::MAX,
            started_at_unix_ms: 1,
            ..record
        };
        let stale_path = root.join(format!("{}.json", u32::MAX));
        write_json(&stale_path, &stale).unwrap();

        let active = active_runtime_in(&root, &std::env::current_exe().unwrap()).unwrap();

        assert_eq!(active.proxy_pids, vec![pid]);
        assert!(!stale_path.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
