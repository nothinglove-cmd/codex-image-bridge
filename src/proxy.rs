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
    guardian,
    image::{self, default_output_dir},
    install, model_config,
    runtime::{RuntimeKind, RuntimeRegistration},
    session::{
        IncrementalSessionReader, SessionCache, SessionImage, SessionLocator, SessionSnapshot,
    },
};

const SEEN_CACHE_CAPACITY: usize = 4096;
const RETRY_DELAYS_MS: [u64; 3] = [75, 200, 500];
const TURN_GUARD_TTL_MS: u64 = 60 * 60 * 1000;
const IMAGE_MODEL_PICKER_ALIAS: &str = "gpt-5.3-codex";
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
    turn_id: Option<String>,
    initial_page: bool,
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
    best_effort_guardian_start(guardian::ensure_started);
    if let Err(error) = model_config::sync_model_cache() {
        eprintln!("codex-image-fix: model cache sync skipped: {error:#}");
    }
    let runtime = RuntimeRegistration::register(RuntimeKind::Proxy)
        .ok()
        .map(Arc::new);
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
        let runtime = runtime.clone();
        thread::spawn(move || {
            pump_requests(
                std::io::stdin(),
                child_stdin,
                requests,
                sessions,
                pending_turns,
                runtime,
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

fn best_effort_guardian_start(start: impl FnOnce() -> Result<()>) {
    let _ = start();
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
    runtime: Option<Arc<RuntimeRegistration>>,
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
        if let Some(runtime) = runtime.as_ref() {
            runtime.heartbeat();
        }
        let forwarded = if first_line {
            first_line = false;
            line.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&line)
        } else {
            &line
        };
        debug_bytes("stdin", forwarded);
        let mut rewritten = None;
        if let Ok(mut value) = serde_json::from_slice::<Value>(forwarded) {
            if let (Some(id), Some(method)) = (request_id(&value), value["method"].as_str()) {
                let method = method.to_owned();
                let thread_id = turn_start_thread_id(&value).map(str::to_owned);
                let turn_id = value
                    .pointer("/params/turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let initial_page = value.pointer("/params/cursor").is_none_or(Value::is_null);
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
                            method: method.clone(),
                            thread_id,
                            turn_id,
                            initial_page,
                        },
                    );
                }
                let mut changed = method == "model/list" && enable_hidden_models(&mut value);
                match rewrite_image_model_alias(&mut value) {
                    Ok(alias_changed) => changed |= alias_changed,
                    Err(error) => {
                        eprintln!("codex-image-fix: image model alias rewrite skipped: {error:#}")
                    }
                }
                if changed {
                    let mut bytes = serde_json::to_vec(&value)?;
                    bytes.push(b'\n');
                    rewritten = Some(bytes);
                }
            }
        }
        let forwarded = rewritten.as_deref().unwrap_or(forwarded);
        child_stdin.write_all(forwarded)?;
        child_stdin.flush()?;
    }
    Ok(())
}

fn enable_hidden_models(request: &mut Value) -> bool {
    if request.get("method").and_then(Value::as_str) != Some("model/list") {
        return false;
    }
    if !request.get("params").is_some_and(Value::is_object) {
        request["params"] = json!({});
    }
    if request.pointer("/params/includeHidden") == Some(&Value::Bool(true)) {
        return false;
    }
    request["params"]["includeHidden"] = Value::Bool(true);
    true
}

fn rewrite_image_model_alias(request: &mut Value) -> Result<bool> {
    if !matches!(
        request.get("method").and_then(Value::as_str),
        Some("thread/start" | "turn/start")
    ) {
        return Ok(false);
    }
    let paths = ["/params/model", "/params/collaborationMode/settings/model"];
    if !paths
        .iter()
        .any(|path| request.pointer(path).and_then(Value::as_str) == Some(IMAGE_MODEL_PICKER_ALIAS))
    {
        return Ok(false);
    }
    let Some(image_model) = configured_image_model()? else {
        return Ok(false);
    };
    Ok(rewrite_image_model_alias_to(request, &image_model))
}

fn rewrite_image_model_alias_to(request: &mut Value, image_model: &str) -> bool {
    let paths = ["/params/model", "/params/collaborationMode/settings/model"];
    let mut changed = false;
    for path in paths {
        if request.pointer(path).and_then(Value::as_str) == Some(IMAGE_MODEL_PICKER_ALIAS) {
            if let Some(model) = request.pointer_mut(path) {
                *model = Value::String(image_model.to_owned());
                changed = true;
            }
        }
    }
    changed
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

#[derive(Default)]
struct HistoryInjection {
    injected: usize,
    notifications: Vec<Value>,
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
        let mut deferred_notifications = Vec::new();

        match self.sanitize_live_images(&mut value) {
            Ok(sanitized) => changed |= sanitized > 0,
            Err(error) => {
                eprintln!("codex-image-fix: official image normalization skipped: {error:#}")
            }
        }

        if let Some(request) = tracked_request.as_ref() {
            let history_result = match request.method.as_str() {
                method if is_thread_history_method(method) => Some(self.inject_history(&mut value)),
                "thread/turns/list" => request
                    .thread_id
                    .as_deref()
                    .map(|thread_id| self.inject_turns_page(&mut value, thread_id)),
                "thread/items/list" => request
                    .thread_id
                    .as_deref()
                    .zip(request.turn_id.as_deref())
                    .map(|(thread_id, turn_id)| {
                        self.inject_items_page(&mut value, thread_id, turn_id, request.initial_page)
                    }),
                _ => None,
            };
            if let Some(history_result) = history_result {
                match history_result {
                    Ok(injection) => {
                        changed |= injection.injected > 0;
                        deferred_notifications = injection.notifications;
                    }
                    Err(error) => {
                        eprintln!("codex-image-fix: history injection skipped: {error:#}")
                    }
                }
            }
        }

        if tracked_request
            .as_ref()
            .is_some_and(|request| request.method == "model/list")
        {
            match configured_image_model().and_then(|model| {
                model.map_or(Ok(false), |model| inject_model_catalog(&mut value, &model))
            }) {
                Ok(injected) => changed |= injected,
                Err(error) => {
                    eprintln!("codex-image-fix: image model catalog injection skipped: {error:#}")
                }
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
        for notification in deferred_notifications {
            write_json_line(writer, &notification)?;
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

    fn inject_history(&mut self, response: &mut Value) -> Result<HistoryInjection> {
        let thread = response
            .pointer("/result/thread")
            .and_then(Value::as_object)
            .context("thread response has no result.thread")?;
        let thread_id = thread
            .get("id")
            .and_then(Value::as_str)
            .context("thread response has no id")?
            .to_owned();
        let hinted_path = thread
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let Some(snapshot) = self.history_snapshot(&thread_id, hinted_path.as_deref())? else {
            return Ok(HistoryInjection::default());
        };

        let mut injection = HistoryInjection::default();
        if let Some(turns) = response
            .pointer_mut("/result/thread/turns")
            .and_then(Value::as_array_mut)
        {
            self.inject_turn_page(&snapshot, &thread_id, turns, true, &mut injection);
        }
        if let Some(turns) = response
            .pointer_mut("/result/initialTurnsPage/data")
            .and_then(Value::as_array_mut)
        {
            self.inject_turn_page(&snapshot, &thread_id, turns, false, &mut injection);
        }
        Ok(injection)
    }

    fn inject_turns_page(
        &mut self,
        response: &mut Value,
        thread_id: &str,
    ) -> Result<HistoryInjection> {
        let Some(snapshot) = self.history_snapshot(thread_id, None)? else {
            return Ok(HistoryInjection::default());
        };
        let turns = response
            .pointer_mut("/result/data")
            .and_then(Value::as_array_mut)
            .context("thread/turns/list response has no result.data array")?;
        let mut injection = HistoryInjection::default();
        self.inject_turn_page(&snapshot, thread_id, turns, false, &mut injection);
        Ok(injection)
    }

    fn inject_turn_page(
        &mut self,
        snapshot: &SessionSnapshot,
        thread_id: &str,
        turns: &mut [Value],
        replay_notifications: bool,
        injection: &mut HistoryInjection,
    ) {
        for turn in turns {
            let Some(turn_id) = turn.get("id").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            let Some(items) = turn.get_mut("items").and_then(Value::as_array_mut) else {
                continue;
            };
            self.inject_turn_items(
                snapshot,
                thread_id,
                &turn_id,
                items,
                replay_notifications,
                injection,
            );
        }
    }

    fn inject_turn_items(
        &mut self,
        snapshot: &SessionSnapshot,
        thread_id: &str,
        turn_id: &str,
        items: &mut Vec<Value>,
        replay_notifications: bool,
        injection: &mut HistoryInjection,
    ) {
        for item in items.iter_mut() {
            match materialize_image_item(&self.output_root, thread_id, turn_id, item) {
                Ok(true) => injection.injected += 1,
                Ok(false) => {}
                Err(error) => eprintln!(
                    "codex-image-fix: official history image normalization skipped: {error:#}"
                ),
            }
        }
        let mut turn_hashes = HashSet::new();
        for image in snapshot
            .turn_images(turn_id)
            .into_iter()
            .filter(|image| image.is_ready())
        {
            let saved = match image::decode_and_save(&self.output_root, thread_id, image) {
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
            if let Some(position) = image_position(items, &image.id) {
                items[position] = item;
            } else {
                let position = items
                    .iter()
                    .rposition(|item| {
                        item.get("type").and_then(Value::as_str) == Some("agentMessage")
                    })
                    .map_or(items.len(), |position| position + 1);
                items.insert(position, item);
            }
            injection.injected += 1;

            if !replay_notifications {
                continue;
            }
            let id_key = image_key(thread_id, turn_id, &image.id);
            let hash_key = image_hash_key(thread_id, turn_id, &saved.sha256);
            if self.seen.contains(&id_key) || self.seen.contains(&hash_key) {
                continue;
            }
            injection.notifications.extend(history_notifications(
                thread_id,
                turn_id,
                image,
                &saved.path,
            ));
            self.seen.insert(id_key);
            self.seen.insert(hash_key);
        }
    }

    fn inject_items_page(
        &mut self,
        response: &mut Value,
        thread_id: &str,
        turn_id: &str,
        initial_page: bool,
    ) -> Result<HistoryInjection> {
        let Some(snapshot) = self.history_snapshot(thread_id, None)? else {
            return Ok(HistoryInjection::default());
        };
        let entries = response
            .pointer_mut("/result/data")
            .and_then(Value::as_array_mut)
            .context("thread/items/list response has no result.data array")?;
        let mut injection = HistoryInjection::default();
        for entry in entries.iter_mut() {
            if entry.get("turnId").and_then(Value::as_str) != Some(turn_id) {
                continue;
            }
            let Some(item) = entry.get_mut("item") else {
                continue;
            };
            match materialize_image_item(&self.output_root, thread_id, turn_id, item) {
                Ok(true) => injection.injected += 1,
                Ok(false) => {}
                Err(error) => eprintln!(
                    "codex-image-fix: official history image normalization skipped: {error:#}"
                ),
            }
        }

        let mut turn_hashes = HashSet::new();
        for image in snapshot
            .turn_images(turn_id)
            .into_iter()
            .filter(|image| image.is_ready())
        {
            let saved = match image::decode_and_save(&self.output_root, thread_id, image) {
                Ok(saved) => saved,
                Err(error) => {
                    eprintln!(
                        "codex-image-fix: image {} skipped during history restore: {error:#}",
                        image.id
                    );
                    continue;
                }
            };
            if !turn_hashes.insert(saved.sha256) {
                continue;
            }
            let item = completed_item(image, &saved.path);
            if let Some(position) = image_entry_position(entries, &image.id) {
                entries[position]["item"] = item;
            } else if initial_page {
                entries.insert(
                    0,
                    json!({
                        "turnId": turn_id,
                        "item": item
                    }),
                );
            } else {
                continue;
            }
            injection.injected += 1;
        }
        Ok(injection)
    }

    fn history_snapshot(
        &mut self,
        thread_id: &str,
        hinted_path: Option<&Path>,
    ) -> Result<Option<SessionSnapshot>> {
        let session_path = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| anyhow!("session locator lock is poisoned"))?;
            sessions.locate(thread_id, hinted_path)?
        };
        session_path
            .map(|session_path| self.history.read(&session_path))
            .transpose()
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

fn history_notifications(
    thread_id: &str,
    turn_id: &str,
    image: &SessionImage,
    saved_path: &Path,
) -> [Value; 2] {
    let timestamp = unix_time_millis();
    [
        json!({
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
                "startedAtMs": timestamp
            }
        }),
        json!({
            "method": "item/completed",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "item": completed_item(image, saved_path),
                "completedAtMs": timestamp
            }
        }),
    ]
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

fn image_position(items: &[Value], image_id: &str) -> Option<usize> {
    items.iter().position(|item| {
        item.get("type").and_then(Value::as_str) == Some("imageGeneration")
            && item.get("id").and_then(Value::as_str) == Some(image_id)
    })
}

fn image_entry_position(entries: &[Value], image_id: &str) -> Option<usize> {
    entries.iter().position(|entry| {
        entry.pointer("/item/type").and_then(Value::as_str) == Some("imageGeneration")
            && entry.pointer("/item/id").and_then(Value::as_str) == Some(image_id)
    })
}

fn configured_image_model() -> Result<Option<String>> {
    let settings = model_config::load_settings()?;
    Ok(settings
        .image_model_enabled
        .then_some(settings.image_model.clone()))
}

fn inject_model_catalog(response: &mut Value, image_model: &str) -> Result<bool> {
    let models = response
        .pointer_mut("/result/data")
        .and_then(Value::as_array_mut)
        .context("model/list response has no result.data array")?;
    let original = models.clone();
    models.retain(|model| !managed_model_entry(model, image_model));

    let entry = image_model_entry(image_model, image_model);
    models.insert(0, entry);
    if image_model != IMAGE_MODEL_PICKER_ALIAS {
        let alias = image_model_entry(IMAGE_MODEL_PICKER_ALIAS, image_model);
        models.insert(1, alias);
    }
    Ok(*models != original)
}

fn managed_model_entry(model: &Value, image_model: &str) -> bool {
    [image_model, IMAGE_MODEL_PICKER_ALIAS]
        .into_iter()
        .any(|candidate| {
            model.get("id").and_then(Value::as_str) == Some(candidate)
                || model.get("model").and_then(Value::as_str) == Some(candidate)
        })
}

fn image_model_entry(catalog_model: &str, image_model: &str) -> Value {
    let display_name = if image_model == model_config::IMAGE_MODEL {
        "GPT Image 2"
    } else {
        image_model
    };
    json!({
        "model": catalog_model,
        "id": catalog_model,
        "slug": catalog_model,
        "name": catalog_model,
        "displayName": display_name,
        "description": "Image generation model configured by Comidea Codex Image Bridge",
        "hidden": false,
        "isDefault": false,
        "defaultReasoningEffort": "medium",
        "supportedReasoningEfforts": default_reasoning_efforts()
    })
}

fn default_reasoning_efforts() -> Value {
    json!([
        {"reasoningEffort": "low", "description": "Fast image generation"},
        {"reasoningEffort": "medium", "description": "Balanced image generation"},
        {"reasoningEffort": "high", "description": "Detailed image generation"},
        {"reasoningEffort": "xhigh", "description": "Maximum image generation detail"}
    ])
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

fn is_thread_history_method(method: &str) -> bool {
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
    fn guardian_start_failure_does_not_stop_protocol_proxy() {
        let mut attempted = false;
        best_effort_guardian_start(|| {
            attempted = true;
            anyhow::bail!("guardian unavailable")
        });
        assert!(attempted);
    }

    #[test]
    fn model_list_includes_configured_image_model_once() {
        let mut response = json!({
            "result": {
                "data": [{
                    "id": "gpt-5.4",
                    "model": "gpt-5.4",
                    "displayName": "GPT-5.4",
                    "description": "Text model",
                    "hidden": false,
                    "supportedReasoningEfforts": [{
                        "reasoningEffort": "medium",
                        "description": "Balanced"
                    }],
                    "defaultReasoningEffort": "medium",
                    "inputModalities": ["text", "image"],
                    "supportsPersonality": true,
                    "isDefault": true
                }]
            }
        });

        assert!(inject_model_catalog(&mut response, "gpt-image-2").unwrap());
        assert!(!inject_model_catalog(&mut response, "gpt-image-2").unwrap());
        let models = response
            .pointer("/result/data")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0]["id"], "gpt-image-2");
        assert_eq!(models[0]["model"], "gpt-image-2");
        assert_eq!(models[0]["slug"], "gpt-image-2");
        assert_eq!(models[0]["name"], "gpt-image-2");
        assert_eq!(models[0]["displayName"], "GPT Image 2");
        assert_eq!(models[0]["hidden"], false);
        assert_eq!(models[0]["isDefault"], false);
        assert_eq!(models[0]["defaultReasoningEffort"], "medium");
        assert_eq!(
            models[0]["supportedReasoningEfforts"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert!(models[0].get("compHash").is_none());
        assert!(models[0].get("inputModalities").is_none());
        assert_eq!(models[1]["id"], IMAGE_MODEL_PICKER_ALIAS);
        assert_eq!(models[1]["model"], IMAGE_MODEL_PICKER_ALIAS);
        assert_eq!(models[1]["displayName"], "GPT Image 2");
        assert_eq!(models[1]["hidden"], false);
    }

    #[test]
    fn model_list_normalizes_existing_hidden_image_model() {
        let mut response = json!({
            "result": {
                "data": [
                    {
                        "id": "gpt-5.4",
                        "model": "gpt-5.4",
                        "displayName": "GPT-5.4",
                        "description": "Text model",
                        "hidden": false,
                        "supportedReasoningEfforts": [],
                        "defaultReasoningEffort": "medium",
                        "isDefault": true
                    },
                    {
                        "id": "gpt-image-2",
                        "model": "gpt-image-2",
                        "displayName": "stale",
                        "description": "stale",
                        "hidden": true,
                        "supportedReasoningEfforts": [],
                        "defaultReasoningEffort": null,
                        "isDefault": true
                    }
                ]
            }
        });

        assert!(inject_model_catalog(&mut response, "gpt-image-2").unwrap());
        assert!(!inject_model_catalog(&mut response, "gpt-image-2").unwrap());
        let model = &response["result"]["data"][0];
        assert_eq!(model["model"], "gpt-image-2");
        assert_eq!(model["displayName"], "GPT Image 2");
        assert_eq!(model["hidden"], false);
        assert_eq!(model["isDefault"], false);
        assert_eq!(model["defaultReasoningEffort"], "medium");
        assert_eq!(
            model["supportedReasoningEfforts"].as_array().unwrap().len(),
            4
        );
        assert!(model.get("compHash").is_none());
    }

    #[test]
    fn model_list_replaces_stale_picker_alias_once() {
        let mut response = json!({
            "result": {
                "data": [
                    {
                        "id": IMAGE_MODEL_PICKER_ALIAS,
                        "model": IMAGE_MODEL_PICKER_ALIAS,
                        "displayName": "Retired model",
                        "description": "stale",
                        "hidden": false,
                        "supportedReasoningEfforts": [{
                            "reasoningEffort": "medium",
                            "description": "Balanced"
                        }],
                        "defaultReasoningEffort": "medium",
                        "isDefault": false
                    },
                    {
                        "id": "gpt-5.6-sol",
                        "model": "gpt-5.6-sol",
                        "displayName": "GPT-5.6-Sol",
                        "description": "Text model",
                        "hidden": false,
                        "supportedReasoningEfforts": [{
                            "reasoningEffort": "low",
                            "description": "Fast"
                        }],
                        "defaultReasoningEffort": "low",
                        "isDefault": true
                    }
                ]
            }
        });

        assert!(inject_model_catalog(&mut response, "gpt-image-2").unwrap());
        assert!(!inject_model_catalog(&mut response, "gpt-image-2").unwrap());
        let models = response["result"]["data"].as_array().unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(
            models
                .iter()
                .filter(|model| model["model"] == IMAGE_MODEL_PICKER_ALIAS)
                .count(),
            1
        );
        assert_eq!(models[1]["displayName"], "GPT Image 2");
        assert_eq!(models[1]["hidden"], false);
    }

    #[test]
    fn model_list_request_includes_hidden_models() {
        let mut request = json!({"id": 1, "method": "model/list", "params": {}});
        assert!(enable_hidden_models(&mut request));
        assert_eq!(request["params"]["includeHidden"], true);
        assert!(!enable_hidden_models(&mut request));
    }

    #[test]
    fn picker_alias_is_rewritten_for_thread_and_turn_start() {
        let mut thread_start = json!({
            "id": 1,
            "method": "thread/start",
            "params": {"model": IMAGE_MODEL_PICKER_ALIAS}
        });
        assert!(rewrite_image_model_alias_to(
            &mut thread_start,
            "gpt-image-2"
        ));
        assert_eq!(thread_start["params"]["model"], "gpt-image-2");

        let mut turn_start = json!({
            "id": 2,
            "method": "turn/start",
            "params": {
                "threadId": "thread-1",
                "input": [],
                "model": IMAGE_MODEL_PICKER_ALIAS,
                "collaborationMode": {
                    "mode": "default",
                    "settings": {
                        "model": IMAGE_MODEL_PICKER_ALIAS,
                        "reasoning_effort": "low"
                    }
                }
            }
        });
        assert!(rewrite_image_model_alias_to(&mut turn_start, "gpt-image-2"));
        assert_eq!(turn_start["params"]["model"], "gpt-image-2");
        assert_eq!(
            turn_start["params"]["collaborationMode"]["settings"]["model"],
            "gpt-image-2"
        );
    }

    #[test]
    fn picker_alias_rewrite_ignores_unrelated_requests() {
        let mut model_list = json!({
            "id": 1,
            "method": "model/list",
            "params": {"model": IMAGE_MODEL_PICKER_ALIAS}
        });
        assert!(!rewrite_image_model_alias(&mut model_list).unwrap());
        assert_eq!(model_list["params"]["model"], IMAGE_MODEL_PICKER_ALIAS);
    }

    #[test]
    fn history_replaces_empty_official_image_placeholder() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-image-history-test-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let session_path = root.join("rollout-thread-1.jsonl");
        fs::write(
            &session_path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread-1\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"image_generation_end\",\"call_id\":\"image-1\",\"status\":\"generating\",\"result\":\"{PNG_BASE64}\"}}}}\n"
            ),
        )
        .unwrap();
        let requests = Arc::new(Mutex::new(HashMap::new()));
        let sessions = Arc::new(Mutex::new(SessionLocator::default()));
        sessions.lock().unwrap().remember("thread-1", session_path);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let mut processor = ResponseProcessor::new(requests, sessions, pending);
        processor.output_root = root.join("images");
        let mut response = json!({
            "result": {
                "thread": {
                    "id": "thread-1",
                    "turns": [{
                        "id": "turn-1",
                        "items": [{
                            "type": "imageGeneration",
                            "id": "image-1",
                            "status": "completed",
                            "result": ""
                        }]
                    }]
                }
            }
        });

        let injection = processor.inject_history(&mut response).unwrap();
        assert_eq!(injection.injected, 1);
        assert_eq!(injection.notifications.len(), 2);
        assert_eq!(injection.notifications[0]["method"], "item/started");
        assert_eq!(injection.notifications[1]["method"], "item/completed");
        assert_eq!(
            injection.notifications[1]["params"]["item"]["savedPath"],
            response["result"]["thread"]["turns"][0]["items"][0]["savedPath"]
        );
        let items = response
            .pointer("/result/thread/turns/0/items")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["result"], "");
        assert!(Path::new(items[0]["savedPath"].as_str().unwrap()).is_file());

        let repeated = processor.inject_history(&mut response).unwrap();
        assert_eq!(repeated.injected, 1);
        assert!(repeated.notifications.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_injects_initial_and_paginated_turn_pages() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-image-page-test-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let session_path = root.join("rollout-thread-1.jsonl");
        fs::write(
            &session_path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread-1\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"image_generation_end\",\"call_id\":\"image-1\",\"status\":\"completed\",\"result\":\"{PNG_BASE64}\"}}}}\n"
            ),
        )
        .unwrap();
        let requests = Arc::new(Mutex::new(HashMap::new()));
        let sessions = Arc::new(Mutex::new(SessionLocator::default()));
        sessions.lock().unwrap().remember("thread-1", session_path);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let mut processor = ResponseProcessor::new(requests, sessions, pending);
        processor.output_root = root.join("images");
        let mut initial = json!({
            "result": {
                "thread": {
                    "id": "thread-1",
                    "turns": []
                },
                "initialTurnsPage": {
                    "data": [{
                        "id": "turn-1",
                        "status": "completed",
                        "items": [{
                            "type": "agentMessage",
                            "id": "assistant-1",
                            "text": "done"
                        }]
                    }],
                    "nextCursor": null
                }
            }
        });

        let injection = processor.inject_history(&mut initial).unwrap();
        assert_eq!(injection.injected, 1);
        assert!(injection.notifications.is_empty());
        let initial_image = &initial["result"]["initialTurnsPage"]["data"][0]["items"][1];
        assert_eq!(initial_image["type"], "imageGeneration");
        assert!(Path::new(initial_image["savedPath"].as_str().unwrap()).is_file());

        let mut page = json!({
            "result": {
                "data": [{
                    "id": "turn-1",
                    "status": "completed",
                    "items": []
                }],
                "nextCursor": null
            }
        });
        let injection = processor.inject_turns_page(&mut page, "thread-1").unwrap();
        assert_eq!(injection.injected, 1);
        assert!(injection.notifications.is_empty());
        assert_eq!(
            page["result"]["data"][0]["items"][0]["type"],
            "imageGeneration"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_injects_only_the_initial_item_page() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codex-image-item-page-test-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let session_path = root.join("rollout-thread-1.jsonl");
        fs::write(
            &session_path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread-1\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}}}\n\
                 {{\"type\":\"response_item\",\"payload\":{{\"type\":\"image_generation_call\",\"id\":\"image-1\",\"status\":\"completed\",\"result\":\"{PNG_BASE64}\"}}}}\n"
            ),
        )
        .unwrap();
        let requests = Arc::new(Mutex::new(HashMap::new()));
        let sessions = Arc::new(Mutex::new(SessionLocator::default()));
        sessions.lock().unwrap().remember("thread-1", session_path);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let mut processor = ResponseProcessor::new(requests, sessions, pending);
        processor.output_root = root.join("images");
        let mut initial = json!({"result": {"data": [], "nextCursor": null}});

        let injection = processor
            .inject_items_page(&mut initial, "thread-1", "turn-1", true)
            .unwrap();
        assert_eq!(injection.injected, 1);
        assert_eq!(initial["result"]["data"][0]["turnId"], "turn-1");
        assert_eq!(
            initial["result"]["data"][0]["item"]["type"],
            "imageGeneration"
        );

        let mut later = json!({"result": {"data": [], "nextCursor": null}});
        let injection = processor
            .inject_items_page(&mut later, "thread-1", "turn-1", false)
            .unwrap();
        assert_eq!(injection.injected, 0);
        assert!(later["result"]["data"].as_array().unwrap().is_empty());

        fs::remove_dir_all(root).unwrap();
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
