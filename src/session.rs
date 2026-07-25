use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use crate::image::{codex_home, MAX_ENCODED_BYTES};

const MAX_JSONL_LINE_BYTES: usize = MAX_ENCODED_BYTES + 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SessionImage {
    pub turn_id: Option<String>,
    pub id: String,
    pub status: String,
    pub revised_prompt: Option<String>,
    pub result: Option<String>,
    pub saved_path: Option<PathBuf>,
}

impl SessionImage {
    pub fn is_ready(&self) -> bool {
        self.result.as_ref().is_some_and(|value| !value.is_empty())
            || self.saved_path.as_ref().is_some_and(|path| path.is_file())
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionSnapshot {
    pub thread_id: Option<String>,
    pub images: Vec<SessionImage>,
}

impl SessionSnapshot {
    pub fn ready_images(&self) -> impl Iterator<Item = &SessionImage> {
        self.images.iter().filter(|image| image.is_ready())
    }

    pub fn turn_images(&self, turn_id: &str) -> Vec<&SessionImage> {
        self.images
            .iter()
            .filter(|image| image.turn_id.as_deref() == Some(turn_id))
            .collect()
    }
}

pub fn read_session(path: &Path) -> Result<SessionSnapshot> {
    let mut reader = IncrementalSessionReader::from_start(path.to_path_buf());
    reader.refresh()?;
    Ok(reader.snapshot().clone())
}

#[derive(Default)]
pub struct SessionCache {
    readers: HashMap<PathBuf, IncrementalSessionReader>,
}

impl SessionCache {
    pub fn read(&mut self, path: &Path) -> Result<SessionSnapshot> {
        let reader = self
            .readers
            .entry(path.to_path_buf())
            .or_insert_with(|| IncrementalSessionReader::from_start(path.to_path_buf()));
        reader.refresh()?;
        Ok(reader.snapshot().clone())
    }
}

pub struct IncrementalSessionReader {
    path: PathBuf,
    offset: u64,
    minimum_offset: u64,
    parser: SessionParser,
}

impl IncrementalSessionReader {
    pub fn from_start(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            minimum_offset: 0,
            parser: SessionParser::default(),
        }
    }

    pub fn from_offset(path: PathBuf, offset: u64, turn_id: String) -> Self {
        Self {
            path,
            offset,
            minimum_offset: offset,
            parser: SessionParser {
                current_turn_id: Some(turn_id),
                ..SessionParser::default()
            },
        }
    }

    pub fn refresh(&mut self) -> Result<()> {
        let length = fs::metadata(&self.path)
            .with_context(|| format!("failed to stat session {}", self.path.display()))?
            .len();
        if length < self.offset {
            if self.minimum_offset != 0 {
                bail!("session was truncated after the turn started");
            }
            self.offset = 0;
            self.parser = SessionParser::default();
        }

        let mut file = File::open(&self.path)
            .with_context(|| format!("failed to open session {}", self.path.display()))?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            let bytes_read = read_bounded_line(&mut reader, &mut line)?;
            if bytes_read == 0 {
                break;
            }
            if line.last() != Some(&b'\n') {
                break;
            }
            self.offset = self
                .offset
                .checked_add(bytes_read as u64)
                .context("session offset overflow")?;
            self.parser.process(&line, &self.path);
        }
        Ok(())
    }

    pub fn snapshot(&self) -> &SessionSnapshot {
        &self.parser.snapshot
    }

    #[cfg(test)]
    fn offset(&self) -> u64 {
        self.offset
    }
}

#[derive(Default)]
struct SessionParser {
    snapshot: SessionSnapshot,
    current_turn_id: Option<String>,
}

impl SessionParser {
    fn process(&mut self, line: &[u8], session_path: &Path) {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let record_type = record.get("type").and_then(Value::as_str);
        let payload = &record["payload"];
        let payload_type = payload.get("type").and_then(Value::as_str);

        match (record_type, payload_type) {
            (Some("session_meta"), _) => {
                self.snapshot.thread_id = payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            (Some("turn_context"), _) => self.set_current_turn(payload),
            (Some("event_msg"), Some("task_started")) => self.set_current_turn(payload),
            (Some("response_item"), Some("image_generation_call")) => {
                if let Some(image) = self.parse_image(payload, "id", session_path) {
                    self.upsert_image(image);
                }
            }
            (Some("event_msg"), Some("image_generation_end")) => {
                if let Some(image) = self.parse_image(payload, "call_id", session_path) {
                    self.upsert_image(image);
                }
            }
            (Some("event_msg"), Some("task_complete"))
                if payload.get("turn_id").and_then(Value::as_str)
                    == self.current_turn_id.as_deref() =>
            {
                self.current_turn_id = None;
            }
            _ => {}
        }
    }

    fn set_current_turn(&mut self, payload: &Value) {
        self.current_turn_id = payload
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }

    fn parse_image(
        &self,
        payload: &Value,
        id_field: &str,
        session_path: &Path,
    ) -> Option<SessionImage> {
        let id = payload.get(id_field).and_then(Value::as_str)?;
        let turn_id = payload
            .pointer("/internal_chat_message_metadata_passthrough/turn_id")
            .or_else(|| payload.get("turn_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.current_turn_id.clone());
        let saved_path = payload
            .get("saved_path")
            .or_else(|| payload.get("savedPath"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    session_path
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(path)
                }
            });
        Some(SessionImage {
            turn_id,
            id: id.to_owned(),
            status: payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            revised_prompt: payload
                .get("revised_prompt")
                .or_else(|| payload.get("revisedPrompt"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            result: payload
                .get("result")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            saved_path,
        })
    }

    fn upsert_image(&mut self, image: SessionImage) {
        let Some(existing) = self
            .snapshot
            .images
            .iter_mut()
            .find(|item| item.id == image.id)
        else {
            self.snapshot.images.push(image);
            return;
        };
        if image.turn_id.is_some() {
            existing.turn_id = image.turn_id;
        }
        if image.status != "unknown" {
            existing.status = image.status;
        }
        if image.revised_prompt.is_some() {
            existing.revised_prompt = image.revised_prompt;
        }
        if image.result.is_some() {
            existing.result = image.result;
        }
        if image.saved_path.is_some() {
            existing.saved_path = image.saved_path;
        }
    }
}

#[derive(Default)]
pub struct SessionLocator {
    cached: HashMap<String, PathBuf>,
}

impl SessionLocator {
    pub fn remember(&mut self, thread_id: impl Into<String>, path: PathBuf) {
        self.cached.insert(thread_id.into(), path);
    }

    pub fn locate_fast(
        &mut self,
        thread_id: &str,
        hinted_path: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        if let Some(path) = hinted_path.and_then(existing_jsonl) {
            self.cached.insert(thread_id.to_owned(), path.clone());
            return Ok(Some(path));
        }
        if let Some(path) = self
            .cached
            .get(thread_id)
            .and_then(|path| existing_jsonl(path))
        {
            return Ok(Some(path));
        }

        let root = codex_home();
        if let Some(path) = find_session_in_state_databases(&root, thread_id)? {
            self.cached.insert(thread_id.to_owned(), path.clone());
            return Ok(Some(path));
        }
        Ok(None)
    }

    pub fn locate(
        &mut self,
        thread_id: &str,
        hinted_path: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        if let Some(path) = self.locate_fast(thread_id, hinted_path)? {
            return Ok(Some(path));
        }

        let root = codex_home();
        for directory in [root.join("sessions"), root.join("archived_sessions")] {
            if let Some(path) = find_session_file(&directory, thread_id)? {
                self.cached.insert(thread_id.to_owned(), path.clone());
                return Ok(Some(path));
            }
        }
        Ok(None)
    }
}

fn find_session_in_state_databases(root: &Path, thread_id: &str) -> Result<Option<PathBuf>> {
    let mut databases = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|value| value.to_str()) == Some("sqlite")
                && path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.starts_with("state_"))
        })
        .collect::<Vec<_>>();
    databases.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });

    for database in databases.into_iter().rev() {
        let Ok(Some(path)) = query_rollout_path(&database, thread_id) else {
            continue;
        };
        if let Some(path) = allowed_session_path(root, &path) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn query_rollout_path(database: &Path, thread_id: &str) -> Result<Option<PathBuf>> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = match Connection::open_with_flags(database, flags) {
        Ok(connection) => connection,
        Err(_) => return Ok(None),
    };
    connection.busy_timeout(Duration::from_millis(200))?;
    connection.pragma_update(None, "query_only", true)?;
    let path = match connection
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1 LIMIT 1",
            [thread_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
    {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    Ok(path.map(PathBuf::from))
}

fn allowed_session_path(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let candidate = existing_jsonl(candidate)?;
    let candidate = candidate.canonicalize().ok()?;
    for directory in [root.join("sessions"), root.join("archived_sessions")] {
        let Ok(directory) = directory.canonicalize() else {
            continue;
        };
        if candidate.starts_with(directory) {
            return Some(candidate);
        }
    }
    None
}

fn existing_jsonl(path: &Path) -> Option<PathBuf> {
    (path.is_file() && path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .then(|| path.to_path_buf())
}

fn find_session_file(directory: &Path, thread_id: &str) -> Result<Option<PathBuf>> {
    if !directory.is_dir() {
        return Ok(None);
    }
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)
            .with_context(|| format!("failed to read {}", current.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl")
                && path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|name| name.contains(thread_id))
            {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn read_bounded_line(reader: &mut impl BufRead, output: &mut Vec<u8>) -> Result<usize> {
    output.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(output.len());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if output.len() + take > MAX_JSONL_LINE_BYTES {
            bail!("session JSONL line exceeds 129 MiB limit");
        }
        output.extend_from_slice(&available[..take]);
        reader.consume(take);
        if output.last() == Some(&b'\n') {
            return Ok(output.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        io::{Cursor, Write},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn bounded_reader_preserves_complete_lines() {
        let mut cursor = Cursor::new(b"one\ntwo\n".to_vec());
        let mut line = Vec::new();
        assert_eq!(read_bounded_line(&mut cursor, &mut line).unwrap(), 4);
        assert_eq!(line, b"one\n");
        assert_eq!(read_bounded_line(&mut cursor, &mut line).unwrap(), 4);
        assert_eq!(line, b"two\n");
    }

    #[test]
    fn parses_both_image_events_without_completed_status() {
        let root = test_directory("events");
        let path = root.join("rollout-thread.jsonl");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"image_generation_end\",\"call_id\":\"image-1\",\"status\":\"generating\",\"result\":\"abc\",\"saved_path\":\"image.png\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"image_generation_call\",\"id\":\"image-1\",\"status\":\"generating\",\"result\":\"abc\"}}\n"
            ),
        )
        .unwrap();

        let snapshot = read_session(&path).unwrap();
        assert_eq!(snapshot.images.len(), 1);
        let image = &snapshot.images[0];
        assert!(image.is_ready());
        assert_eq!(image.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(
            image.saved_path.as_deref(),
            Some(root.join("image.png").as_path())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_reader_only_parses_appended_complete_lines() {
        let root = test_directory("incremental");
        let path = root.join("rollout-thread.jsonl");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-1\"}}\n",
        )
        .unwrap();
        let mut reader = IncrementalSessionReader::from_start(path.clone());
        reader.refresh().unwrap();
        let first_offset = reader.offset();

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-1\"}}\n").unwrap();
        file.write_all(b"{\"type\":\"response_item\",\"payload\":{\"type\":\"image_generation_call\",\"id\":\"image-1\",\"status\":\"generating\",\"result\":\"abc\"}}").unwrap();
        file.flush().unwrap();
        reader.refresh().unwrap();
        assert!(reader.offset() > first_offset);
        assert!(reader.snapshot().images.is_empty());

        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        reader.refresh().unwrap();
        assert_eq!(reader.snapshot().images.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tail_reader_excludes_images_before_turn_start() {
        let root = test_directory("tail");
        let path = root.join("rollout-thread.jsonl");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &path,
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"image_generation_call\",\"id\":\"old\",\"result\":\"old\"}}\n",
        )
        .unwrap();
        let offset = fs::metadata(&path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"image_generation_end\",\"call_id\":\"new\",\"status\":\"generating\",\"result\":\"new\"}}\n").unwrap();
        file.flush().unwrap();

        let mut reader =
            IncrementalSessionReader::from_offset(path.clone(), offset, "turn-2".into());
        reader.refresh().unwrap();
        assert_eq!(reader.snapshot().images.len(), 1);
        assert_eq!(reader.snapshot().images[0].id, "new");
        assert_eq!(
            reader.snapshot().images[0].turn_id.as_deref(),
            Some("turn-2")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn locates_rollout_by_thread_id_through_read_only_state_database() {
        let root = test_directory("sqlite");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let rollout = sessions.join("rollout-thread-1.jsonl");
        fs::write(&rollout, b"{}\n").unwrap();
        let database = root.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path) VALUES (?1, ?2)",
                ("thread-1", rollout.to_string_lossy().as_ref()),
            )
            .unwrap();
        drop(connection);

        let found = find_session_in_state_databases(&root, "thread-1")
            .unwrap()
            .unwrap();
        assert_eq!(found, rollout.canonicalize().unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    fn test_directory(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("codex-image-fix-{label}-{unique}"))
    }
}
