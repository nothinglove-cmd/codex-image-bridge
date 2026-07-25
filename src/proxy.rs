use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::{
    image::{self, default_output_dir},
    install,
    session::{
        IncrementalSessionReader, SessionCache, SessionImage, SessionLocator, SessionSnapshot,
    },
};

const SEEN_CACHE_CAPACITY: usize = 4096;
const RETRY_DELAYS_MS: [u64; 3] = [75, 200, 500];
const TURN_GUARD_TTL_MS: u64 = 60 * 60 * 1000;
const PROXY_ENVIRONMENT_VARIABLES: [&str; 4] = [
    "CODEX_CLI_PATH",
    "CODEX_IMAGE_PROXY_REAL_CLI",
    "CODEX_IMAGE_PROXY_DEBUG",
    install::HIDE_CONSOLE_ENV,
];
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

type RequestTracker = Arc<Mutex<HashMap<String, TrackedRequest>>>;
type SharedSessionLocator = Arc<Mutex<SessionLocator>>;
type PendingTurnStarts = Arc<Mutex<HashMap<String, VecDeque<TurnStart>>>>;

#[derive(Clone)]
struct TrackedRequest {
    method: String,
    thread_id: Option<String>,
}

#[derive(Clone)]
struct TurnStart {
    thread_id: String,
    started_at_ms: u64,
    session_path: Option<PathBuf>,
    session_offset: Option<u64>,
}

struct TurnGuard {
    started_at_ms: u64,
    reader: Option<IncrementalSessionReader>,
}

pub fn run(args: &[OsString]) -> Result<()> {
    hide_parent_console_if_requested();
    debug_event("start");
    let real_cli = install::resolve_real_cli(None)?;
    if args
        .iter()
        .any(|argument| argument == OsStr::new("app-server"))
    {
        run_app_server(&real_cli, args)
    } else {
        run_passthrough(&real_cli, args)
    }
}

fn run_passthrough(real_cli: &Path, args: &[OsString]) -> Result<()> {
    let status = child_command(real_cli, args)
        .status()
        .with_context(|| format!("failed to start real Codex CLI at {}", real_cli.display()))?;
    exit_with_child_status(status)
}

fn run_app_server(real_cli: &Path, args: &[OsString]) -> Result<()> {
    let mut child = child_command(real_cli, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start app-server at {}", real_cli.display()))?;

    let child_stdin = child.stdin.take().context("child stdin is unavailable")?;
    let child_stdout = child.stdout.take().context("child stdout is unavailable")?;
    let requests = Arc::new(Mutex::new(HashMap::new()));
    let sessions = Arc::new(Mutex::new(SessionLocator::default()));
    let pending_turns = Arc::new(Mutex::new(HashMap::new()));
    let request_thread = {
        let requests = Arc::clone(&requests);
        let sessions = Arc::clone(&sessions);
        let pending_turns = Arc::clone(&pending_turns);
        thread::spawn(move || {
            pump_requests(
                std::io::stdin(),
                child_stdin,
                requests,
                sessions,
                pending_turns,
            )
        })
    };

    let mut processor = ResponseProcessor::new(requests, sessions, pending_turns);
    let mut reader = BufReader::new(child_stdout);
    let mut writer = BufWriter::new(std::io::stdout());
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        processor.process_line(&line, &mut writer)?;
        writer.flush()?;
    }
    writer.flush()?;

    let status = child.wait()?;
    if request_thread.is_finished() {
        match request_thread.join() {
            Ok(result) => result?,
            Err(_) => anyhow::bail!("stdin forwarding thread panicked"),
        }
    }
    exit_with_child_status(status)
}

fn child_command(real_cli: &Path, args: &[OsString]) -> Command {
    let mut command = Command::new(real_cli);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.args(args);
    for variable in PROXY_ENVIRONMENT_VARIABLES {
        command.env_remove(variable);
    }
    command
}

#[cfg(windows)]
fn hide_parent_console_if_requested() {
    use windows_sys::Win32::{
        System::Console::{
            AttachConsole, GetConsoleWindow, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS,
            STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        },
        UI::WindowsAndMessaging::{ShowWindow, SW_HIDE},
    };

    if std::env::var_os(install::HIDE_CONSOLE_ENV).as_deref() != Some(OsStr::new("1")) {
        return;
    }

    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE);
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        let error = GetStdHandle(STD_ERROR_HANDLE);
        let mut window = GetConsoleWindow();
        if window.is_null() && AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            let _ = SetStdHandle(STD_INPUT_HANDLE, input);
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, output);
            let _ = SetStdHandle(STD_ERROR_HANDLE, error);
            window = GetConsoleWindow();
        }
        if !window.is_null() {
            ShowWindow(window, SW_HIDE);
        }
    }
}

#[cfg(not(windows))]
fn hide_parent_console_if_requested() {}

fn pump_requests(
    input: impl Read,
    mut child_stdin: ChildStdin,
    requests: RequestTracker,
    sessions: SharedSessionLocator,
    pending_turns: PendingTurnStarts,
) -> Result<()> {
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    let mut first_line = true;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let received_at_ms = unix_time_millis();
        let forwarded = if first_line {
            first_line = false;
            line.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&line)
        } else {
            &line
        };
        debug_bytes("stdin", forwarded);
        if let Ok(value) = serde_json::from_slice::<Value>(forwarded) {
            if let (Some(id), Some(method)) = (request_id(&value), value["method"].as_str()) {
                let thread_id = turn_start_thread_id(&value).map(str::to_owned);
                if method == "turn/start" {
                    if let Some(start) =
                        capture_turn_start(&value, received_at_ms, &sessions, thread_id.as_deref())
                    {
                        remember_pending_turn(&pending_turns, start);
                    }
                }
                if let Ok(mut tracker) = requests.lock() {
                    tracker.insert(
                        id,
                        TrackedRequest {
                            method: method.to_owned(),
                            thread_id,
                        },
                    );
                }
            }
        }
        child_stdin.write_all(forwarded)?;
        child_stdin.flush()?;
    }
    Ok(())
}

fn capture_turn_start(
    request: &Value,
    started_at_ms: u64,
    sessions: &SharedSessionLocator,
    known_thread_id: Option<&str>,
) -> Option<TurnStart> {
    let thread_id = known_thread_id.or_else(|| turn_start_thread_id(request))?;
    let hinted_path = request
        .pointer("/params/thread/path")
        .or_else(|| request.pointer("/params/path"))
        .and_then(Value::as_str)
        .map(Path::new);
    let session_path = sessions
        .lock()
        .ok()
        .and_then(|mut sessions| sessions.locate_fast(thread_id, hinted_path).ok().flatten());
    let session_offset = session_path
        .as_ref()
        .and_then(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len());
    Some(TurnStart {
        thread_id: thread_id.to_owned(),
        started_at_ms,
        session_path,
        session_offset,
    })
}

fn remember_pending_turn(pending_turns: &PendingTurnStarts, start: TurnStart) {
    let Ok(mut pending) = pending_turns.lock() else {
        return;
    };
    let cutoff = start.started_at_ms.saturating_sub(TURN_GUARD_TTL_MS);
    pending.retain(|_, queue| {
        while queue
            .front()
            .is_some_and(|item| item.started_at_ms < cutoff)
        {
            queue.pop_front();
        }
        !queue.is_empty()
    });
    let queue = pending.entry(start.thread_id.clone()).or_default();
    queue.push_back(start);
    while queue.len() > 8 {
        queue.pop_front();
    }
}

fn debug_bytes(label: &str, bytes: &[u8]) {
    let Some(path) = std::env::var_os("CODEX_IMAGE_PROXY_DEBUG") else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "pid={} {label} bytes={}",
            std::process::id(),
            bytes.len()
        );
    }
}

fn debug_event(message: &str) {
    let Some(path) = std::env::var_os("CODEX_IMAGE_PROXY_DEBUG") else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "pid={} {message}", std::process::id());
    }
}

struct ResponseProcessor {
    requests: RequestTracker,
    sessions: SharedSessionLocator,
    pending_turns: PendingTurnStarts,
    history: SessionCache,
    output_root: PathBuf,
    seen: SeenCache,
    turn_guards: HashMap<String, TurnGuard>,
}

impl ResponseProcessor {
    fn new(
        requests: RequestTracker,
        sessions: SharedSessionLocator,
        pending_turns: PendingTurnStarts,
    ) -> Self {
        Self {
            requests,
            sessions,
            pending_turns,
            history: SessionCache::default(),
            output_root: default_output_dir(),
            seen: SeenCache::new(SEEN_CACHE_CAPACITY),
            turn_guards: HashMap::new(),
        }
    }

    fn process_line(&mut self, raw: &[u8], writer: &mut impl Write) -> Result<()> {
        let Ok(mut value) = serde_json::from_slice::<Value>(raw) else {
            writer.write_all(raw)?;
            return Ok(());
        };

        let tracked_request = request_id(&value).and_then(|id| {
            self.requests
                .lock()
                .ok()
                .and_then(|mut requests| requests.remove(&id))
        });
        self.observe_turn_start(&value, tracked_request.as_ref());
        let mut changed = false;

        match self.sanitize_live_images(&mut value) {
            Ok(sanitized) => changed |= sanitized > 0,
            Err(error) => {
                eprintln!("codex-image-fix: official image normalization skipped: {error:#}")
            }
        }

        if tracked_request
            .as_ref()
            .is_some_and(|request| is_history_method(&request.method))
        {
            match self.inject_history(&mut value) {
                Ok(injected) => changed |= injected > 0,
                Err(error) => eprintln!("codex-image-fix: history injection skipped: {error:#}"),
            }
        }

        self.observe_official_image(&value);
        if value.get("method").and_then(Value::as_str) == Some("turn/completed") {
            if let Err(error) = self.inject_realtime(&value, writer) {
                eprintln!("codex-image-fix: realtime injection skipped: {error:#}");
            }
        }

        if changed {
            serde_json::to_writer(&mut *writer, &value)?;
            writer.write_all(b"\n")?;
        } else {
            writer.write_all(raw)?;
        }
        Ok(())
    }

    fn sanitize_live_images(&self, value: &mut Value) -> Result<usize> {
        let method = value.get("method").and_then(Value::as_str);
        if matches!(method, Some("item/started" | "item/completed")) {
            let thread_id = value
                .pointer("/params/threadId")
                .and_then(Value::as_str)
                .context("image item notification has no threadId")?
                .to_owned();
            let turn_id = value
                .pointer("/params/turnId")
                .and_then(Value::as_str)
                .context("image item notification has no turnId")?
                .to_owned();
            let Some(item) = value.pointer_mut("/params/item") else {
                return Ok(0);
            };
            return materialize_image_item(&self.output_root, &thread_id, &turn_id, item)
                .map(usize::from);
        }
        if method != Some("turn/completed") {
            return Ok(0);
        }

        let thread_id = value
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .context("turn/completed has no threadId")?
            .to_owned();
        let turn_id = value
            .pointer("/params/turn/id")
            .and_then(Value::as_str)
            .context("turn/completed has no turn id")?
            .to_owned();
        let Some(items) = value
            .pointer_mut("/params/turn/items")
            .and_then(Value::as_array_mut)
        else {
            return Ok(0);
        };
        let mut sanitized = 0;
        for item in items {
            sanitized += usize::from(materialize_image_item(
                &self.output_root,
                &thread_id,
                &turn_id,
                item,
            )?);
        }
        Ok(sanitized)
    }

    fn observe_turn_start(&mut self, value: &Value, request: Option<&TrackedRequest>) {
        if request.is_some_and(|request| request.method == "turn/start") {
            let thread_id = request.and_then(|request| request.thread_id.as_deref());
            let turn_id = response_turn_id(value);
            if let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) {
                self.register_turn_guard(thread_id, turn_id);
            }
        }

        if value.get("method").and_then(Value::as_str) != Some("turn/started") {
            return;
        }
        let Some(thread_id) = value
            .pointer("/params/threadId")
            .or_else(|| value.pointer("/params/thread/id"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(turn_id) = value
            .pointer("/params/turn/id")
            .or_else(|| value.pointer("/params/turnId"))
            .and_then(Value::as_str)
        else {
            return;
        };
        self.register_turn_guard(thread_id, turn_id);
    }

    fn register_turn_guard(&mut self, thread_id: &str, turn_id: &str) {
        let key = turn_key(thread_id, turn_id);
        if self.turn_guards.contains_key(&key) {
            return;
        }
        let start = self.take_pending_turn(thread_id).unwrap_or_else(|| {
            capture_turn_start(
                &json!({"params": {"threadId": thread_id}}),
                unix_time_millis(),
                &self.sessions,
                Some(thread_id),
            )
            .unwrap()
        });
        let reader = start
            .session_path
            .zip(start.session_offset)
            .map(|(path, offset)| {
                IncrementalSessionReader::from_offset(path, offset, turn_id.to_owned())
            });
        self.turn_guards.insert(
            key,
            TurnGuard {
                started_at_ms: start.started_at_ms,
                reader,
            },
        );
        let cutoff = unix_time_millis().saturating_sub(TURN_GUARD_TTL_MS);
        self.turn_guards
            .retain(|_, guard| guard.started_at_ms >= cutoff);
    }

    fn take_pending_turn(&self, thread_id: &str) -> Option<TurnStart> {
        let mut pending = self.pending_turns.lock().ok()?;
        let queue = pending.get_mut(thread_id)?;
        let start = queue.pop_front();
        if queue.is_empty() {
            pending.remove(thread_id);
        }
        start
    }

    fn inject_history(&mut self, response: &mut Value) -> Result<usize> {
        let thread = response
            .pointer_mut("/result/thread")
            .and_then(Value::as_object_mut)
            .context("thread response has no result.thread")?;
        let thread_id = thread
            .get("id")
            .and_then(Value::as_str)
            .context("thread response has no id")?
            .to_owned();
        let hinted_path = thread.get("path").and_then(Value::as_str).map(Path::new);
        let session_path = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| anyhow!("session locator lock is poisoned"))?;
            sessions.locate(&thread_id, hinted_path)?
        };
        let Some(session_path) = session_path else {
            return Ok(0);
        };
        let snapshot = self.history.read(&session_path)?;
        let Some(turns) = thread.get_mut("turns").and_then(Value::as_array_mut) else {
            return Ok(0);
        };

        let mut injected = 0;
        for turn in turns {
            let Some(turn_id) = turn.get("id").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            let Some(items) = turn.get_mut("items").and_then(Value::as_array_mut) else {
                continue;
            };
            for item in items.iter_mut() {
                match materialize_image_item(&self.output_root, &thread_id, &turn_id, item) {
                    Ok(true) => injected += 1,
                    Ok(false) => {}
                    Err(error) => eprintln!(
                        "codex-image-fix: official history image normalization skipped: {error:#}"
                    ),
                }
            }
            let mut turn_hashes = HashSet::new();
            for image in snapshot
                .turn_images(&turn_id)
                .into_iter()
                .filter(|image| image.is_ready())
            {
                if contains_image(items, &image.id) {
                    continue;
                }
                let saved = match image::decode_and_save(&self.output_root, &thread_id, image) {
                    Ok(saved) => saved,
                    Err(error) => {
                        eprintln!(
                            "codex-image-fix: image {} skipped during history restore: {error:#}",
                            image.id
                        );
                        continue;
                    }
                };
                if !turn_hashes.insert(saved.sha256.clone()) {
                    continue;
                }
                let item = completed_item(image, &saved.path);
                let position = items
                    .iter()
                    .rposition(|item| {
                        item.get("type").and_then(Value::as_str) == Some("agentMessage")
                    })
                    .unwrap_or(items.len());
                items.insert(position, item);
                injected += 1;
            }
        }
        Ok(injected)
    }

    fn inject_realtime(&mut self, notification: &Value, writer: &mut impl Write) -> Result<()> {
        let thread_id = notification
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .context("turn/completed has no threadId")?;
        let turn_id = notification
            .pointer("/params/turn/id")
            .and_then(Value::as_str)
            .context("turn/completed has no turn id")?;
        let official_ids: HashSet<&str> = notification
            .pointer("/params/turn/items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("imageGeneration"))
            .filter_map(|item| item.get("id").and_then(Value::as_str))
            .collect();

        let key = turn_key(thread_id, turn_id);
        let mut guard = self.turn_guards.remove(&key);
        let started_at_ms = guard
            .as_ref()
            .map_or_else(unix_time_millis, |guard| guard.started_at_ms);
        let snapshot = if let Some(reader) = guard.as_mut().and_then(|guard| guard.reader.as_mut())
        {
            load_turn_with_retries(turn_id, || {
                reader.refresh()?;
                Ok(reader.snapshot().clone())
            })?
        } else {
            let session_path = {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| anyhow!("session locator lock is poisoned"))?;
                sessions.locate(thread_id, None)?
            };
            let Some(session_path) = session_path else {
                return Ok(());
            };
            load_turn_with_retries(turn_id, || self.history.read(&session_path))?
        };

        for image in snapshot
            .turn_images(turn_id)
            .into_iter()
            .filter(|image| image.is_ready())
        {
            let id_key = image_key(thread_id, turn_id, &image.id);
            if official_ids.contains(image.id.as_str()) || self.seen.contains(&id_key) {
                continue;
            }
            let saved = match image::decode_and_save(&self.output_root, thread_id, image) {
                Ok(saved) => saved,
                Err(error) => {
                    eprintln!(
                        "codex-image-fix: image {} skipped during realtime restore: {error:#}",
                        image.id
                    );
                    continue;
                }
            };
            let hash_key = image_hash_key(thread_id, turn_id, &saved.sha256);
            if self.seen.contains(&hash_key) {
                continue;
            }
            let completed_at_ms = unix_time_millis();
            let started = json!({
                "method": "item/started",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": {
                        "type": "imageGeneration",
                        "id": image.id,
                        "status": "in_progress",
                        "revisedPrompt": Value::Null,
                        "result": ""
                    },
                    "startedAtMs": started_at_ms
                }
            });
            let completed = json!({
                "method": "item/completed",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": completed_item(image, &saved.path),
                    "completedAtMs": completed_at_ms
                }
            });
            write_json_line(writer, &started)?;
            write_json_line(writer, &completed)?;
            self.seen.insert(id_key);
            self.seen.insert(hash_key);
        }
        Ok(())
    }

    fn observe_official_image(&mut self, value: &Value) {
        let method = value.get("method").and_then(Value::as_str);
        if !matches!(method, Some("item/started" | "item/completed")) {
            return;
        }
        if value.pointer("/params/item/type").and_then(Value::as_str) != Some("imageGeneration") {
            return;
        }
        let Some(thread_id) = value.pointer("/params/threadId").and_then(Value::as_str) else {
            return;
        };
        let Some(turn_id) = value.pointer("/params/turnId").and_then(Value::as_str) else {
            return;
        };
        let Some(image_id) = value.pointer("/params/item/id").and_then(Value::as_str) else {
            return;
        };
        self.seen.insert(image_key(thread_id, turn_id, image_id));
    }
}

fn load_turn_with_retries(
    turn_id: &str,
    mut read: impl FnMut() -> Result<SessionSnapshot>,
) -> Result<SessionSnapshot> {
    let mut snapshot = read()?;
    if !turn_has_pending_image(&snapshot, turn_id) {
        return Ok(snapshot);
    }
    for delay in RETRY_DELAYS_MS {
        thread::sleep(Duration::from_millis(delay));
        snapshot = read()?;
        if !turn_has_pending_image(&snapshot, turn_id) {
            break;
        }
    }
    Ok(snapshot)
}

fn turn_has_pending_image(snapshot: &SessionSnapshot, turn_id: &str) -> bool {
    let images = snapshot.turn_images(turn_id);
    !images.is_empty() && images.iter().any(|image| !image.is_ready())
}

fn completed_item(image: &SessionImage, saved_path: &Path) -> Value {
    json!({
        "type": "imageGeneration",
        "id": image.id,
        "status": "completed",
        "revisedPrompt": image.revised_prompt,
        "result": "",
        "savedPath": saved_path
    })
}

fn materialize_image_item(
    output_root: &Path,
    thread_id: &str,
    turn_id: &str,
    item: &mut Value,
) -> Result<bool> {
    if item.get("type").and_then(Value::as_str) != Some("imageGeneration") {
        return Ok(false);
    }
    let Some(result) = item
        .get("result")
        .and_then(Value::as_str)
        .filter(|result| !result.is_empty())
        .map(str::to_owned)
    else {
        return Ok(false);
    };
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .context("official imageGeneration item has no id")?
        .to_owned();
    let image = SessionImage {
        turn_id: Some(turn_id.to_owned()),
        id,
        status: item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        revised_prompt: item
            .get("revisedPrompt")
            .or_else(|| item.get("revised_prompt"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        result: Some(result),
        saved_path: item
            .get("savedPath")
            .or_else(|| item.get("saved_path"))
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from),
    };
    let saved = image::decode_and_save(output_root, thread_id, &image)?;
    item["result"] = Value::String(String::new());
    item["savedPath"] = serde_json::to_value(saved.path)?;
    Ok(true)
}

fn contains_image(items: &[Value], image_id: &str) -> bool {
    items.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("imageGeneration")
            && item.get("id").and_then(Value::as_str) == Some(image_id)
    })
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn request_id(value: &Value) -> Option<String> {
    let id = value.get("id")?;
    if id.is_null() {
        None
    } else {
        serde_json::to_string(id).ok()
    }
}

fn turn_start_thread_id(value: &Value) -> Option<&str> {
    value
        .pointer("/params/threadId")
        .or_else(|| value.pointer("/params/thread/id"))
        .or_else(|| value.pointer("/params/thread_id"))
        .and_then(Value::as_str)
}

fn response_turn_id(value: &Value) -> Option<&str> {
    value
        .pointer("/result/turn/id")
        .or_else(|| value.pointer("/result/id"))
        .and_then(Value::as_str)
}

fn is_history_method(method: &str) -> bool {
    matches!(
        method,
        "thread/read" | "thread/resume" | "thread/fork" | "thread/rollback"
    )
}

fn turn_key(thread_id: &str, turn_id: &str) -> String {
    format!("{thread_id}\u{1f}{turn_id}")
}

fn image_key(thread_id: &str, turn_id: &str, image_id: &str) -> String {
    format!("id\u{1f}{thread_id}\u{1f}{turn_id}\u{1f}{image_id}")
}

fn image_hash_key(thread_id: &str, turn_id: &str, hash: &str) -> String {
    format!("sha256\u{1f}{thread_id}\u{1f}{turn_id}\u{1f}{hash}")
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn exit_with_child_status(status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    std::process::exit(status.code().unwrap_or(1));
}

struct SeenCache {
    capacity: usize,
    values: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            values: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    fn contains(&self, value: &str) -> bool {
        self.values.contains(value)
    }

    fn insert(&mut self, value: String) {
        if !self.values.insert(value.clone()) {
            return;
        }
        self.order.push_back(value);
        while self.order.len() > self.capacity {
            if let Some(expired) = self.order.pop_front() {
                self.values.remove(&expired);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn request_ids_keep_json_type() {
        assert_eq!(request_id(&json!({"id": 7})).as_deref(), Some("7"));
        assert_eq!(request_id(&json!({"id": "7"})).as_deref(), Some("\"7\""));
    }

    #[test]
    fn seen_cache_is_bounded() {
        let mut cache = SeenCache::new(2);
        cache.insert("one".into());
        cache.insert("two".into());
        cache.insert("three".into());
        assert!(!cache.contains("one"));
        assert!(cache.contains("three"));
    }

    #[test]
    fn official_image_item_is_only_normalized_after_valid_materialization() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-image-fix-item-test-{unique}"));
        let mut valid = json!({
            "type": "imageGeneration",
            "id": "image-1",
            "status": "generating",
            "result": PNG_BASE64
        });

        assert!(materialize_image_item(&root, "thread-1", "turn-1", &mut valid).unwrap());
        assert_eq!(valid["result"], "");
        assert!(Path::new(valid["savedPath"].as_str().unwrap()).is_file());

        let mut invalid = json!({
            "type": "imageGeneration",
            "id": "image-2",
            "status": "completed",
            "result": "not-an-image"
        });
        let original = invalid.clone();
        assert!(materialize_image_item(&root, "thread-1", "turn-1", &mut invalid).is_err());
        assert_eq!(invalid, original);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn realtime_uses_turn_start_offset_and_accepts_generating_status() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-image-fix-test-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let session_path = root.join("rollout-thread-1.jsonl");
        fs::write(
            &session_path,
            format!(
                "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"image_generation_call\",\"id\":\"old-image\",\"status\":\"completed\",\"result\":\"{PNG_BASE64}\"}}}}\n"
            ),
        )
        .unwrap();
        let start_offset = fs::metadata(&session_path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&session_path).unwrap();
        writeln!(
            file,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}"
        )
        .unwrap();
        writeln!(
            file,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"image_generation_end\",\"call_id\":\"image-1\",\"status\":\"generating\",\"result\":\"{PNG_BASE64}\"}}}}"
        )
        .unwrap();
        file.flush().unwrap();

        let requests = Arc::new(Mutex::new(HashMap::new()));
        let sessions = Arc::new(Mutex::new(SessionLocator::default()));
        sessions
            .lock()
            .unwrap()
            .remember("thread-1", session_path.clone());
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let mut processor = ResponseProcessor::new(requests, sessions, pending);
        processor.output_root = root.join("images");
        processor.turn_guards.insert(
            turn_key("thread-1", "turn-1"),
            TurnGuard {
                started_at_ms: 1234,
                reader: Some(IncrementalSessionReader::from_offset(
                    session_path,
                    start_offset,
                    "turn-1".into(),
                )),
            },
        );
        let notification = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread-1",
                "turn": {"id": "turn-1", "items": [], "status": "completed"}
            }
        });
        let raw = format!("{}\n", serde_json::to_string(&notification).unwrap());

        let mut first = Vec::new();
        processor.process_line(raw.as_bytes(), &mut first).unwrap();
        let messages: Vec<Value> = first
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["method"], "item/started");
        assert_eq!(messages[0]["params"]["startedAtMs"], 1234);
        assert_eq!(messages[1]["method"], "item/completed");
        assert_eq!(messages[2]["method"], "turn/completed");
        assert_eq!(messages[1]["params"]["item"]["id"], "image-1");
        assert_eq!(messages[1]["params"]["item"]["result"], "");
        let saved_path = messages[1]["params"]["item"]["savedPath"].as_str().unwrap();
        assert!(Path::new(saved_path).is_file());

        let mut second = Vec::new();
        processor.process_line(raw.as_bytes(), &mut second).unwrap();
        let messages: Vec<_> = second
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(messages.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
