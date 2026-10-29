use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result};
use chrono::DateTime;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{
    config::{Config, source_enabled},
    domain::{Signal, matches_cwd},
    spaces::Resolver,
};

#[derive(Default)]
struct Session {
    offset: u64,
    prefix: Vec<u8>,
    activity: i64,
    cwd: String,
    cwd_time: i64,
    command_cwd: String,
    command_time: i64,
}

pub struct TranscriptCollector {
    claude: HashMap<PathBuf, Session>,
    codex: HashMap<PathBuf, Session>,
    opencode_retry_at: i64,
    spaces: Resolver,
}

impl Default for TranscriptCollector {
    fn default() -> Self {
        Self::new(Resolver::default_path())
    }
}

impl TranscriptCollector {
    pub fn new(space_file: PathBuf) -> Self {
        Self {
            claude: HashMap::new(),
            codex: HashMap::new(),
            opencode_retry_at: 0,
            spaces: Resolver::new(space_file),
        }
    }

    /// Source failures are isolated: one inaccessible transcript or a changed
    /// opencode schema must not stop other sources or the accounting worker.
    pub fn collect(&mut self, config: &Config, patterns: &[String], now: i64) -> Vec<Signal> {
        self.collect_with_spaces(config, patterns, &HashSet::new(), now)
    }

    pub fn collect_with_spaces(
        &mut self,
        config: &Config,
        patterns: &[String],
        bound_spaces: &HashSet<String>,
        now: i64,
    ) -> Vec<Signal> {
        if !config.transcript_tracking {
            return Vec::new();
        }
        let grace = match config.grace_duration() {
            Ok(value) => value.as_secs().max(1) as i64,
            Err(err) => {
                eprintln!("transcripts: {err:#}");
                return Vec::new();
            }
        };
        let mut context = CollectContext {
            patterns,
            bound_spaces,
            spaces: &mut self.spaces,
            now,
            grace,
        };
        let mut out = Vec::new();
        if source_enabled(&config.transcript_root) {
            let files = claude_files(&config.transcript_root);
            collect_files(&mut self.claude, files, false, &mut context, &mut out);
        }
        if source_enabled(&config.codex_root) {
            // Session directories use their start date. Checking modification
            // time instead of imposing a date cutoff preserves long sessions.
            let files = WalkDir::new(&config.codex_root)
                .max_depth(4)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| match entry {
                    Ok(entry)
                        if entry.file_type().is_file()
                            && entry.path().extension().is_some_and(|s| s == "jsonl")
                            && entry.file_name().to_string_lossy().starts_with("rollout-") =>
                    {
                        Some(entry.into_path())
                    }
                    Ok(_) => None,
                    Err(err) => {
                        if err
                            .io_error()
                            .is_none_or(|e| e.kind() != std::io::ErrorKind::NotFound)
                        {
                            eprintln!("codex: {err}");
                        }
                        None
                    }
                })
                .collect();
            collect_files(&mut self.codex, files, true, &mut context, &mut out);
        }
        if source_enabled(&config.opencode_db)
            && now >= self.opencode_retry_at
            && touched_since(&config.opencode_db, now - grace)
        {
            match collect_opencode(&config.opencode_db, patterns, now, grace) {
                Ok(signals) => out.extend(signals),
                Err(err) => {
                    eprintln!("opencode: {err:#} (retry in 60s)");
                    self.opencode_retry_at = now + 60;
                }
            }
        }
        out
    }
}

struct CollectContext<'a> {
    patterns: &'a [String],
    bound_spaces: &'a HashSet<String>,
    spaces: &'a mut Resolver,
    now: i64,
    grace: i64,
}

fn claude_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let dirs = match fs::read_dir(root) {
        Ok(dirs) => dirs,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("claude {}: {err}", root.display());
            }
            return files;
        }
    };
    for directory in dirs.flatten() {
        if !directory.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let entries = match fs::read_dir(directory.path()) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("claude {}: {err}", directory.path().display());
                continue;
            }
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file())
                && entry.path().extension().is_some_and(|e| e == "jsonl")
            {
                files.push(entry.path());
            }
        }
    }
    files
}

fn collect_files(
    states: &mut HashMap<PathBuf, Session>,
    files: Vec<PathBuf>,
    codex: bool,
    context: &mut CollectContext<'_>,
    out: &mut Vec<Signal>,
) {
    let now = context.now;
    let grace = context.grace;
    let live: HashSet<_> = files.iter().cloned().collect();
    states.retain(|path, _| live.contains(path));
    for path in files {
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(err) => {
                eprintln!("transcript {}: {err}", path.display());
                continue;
            }
        };
        if let Ok(modified) = metadata
            .modified()
            .and_then(|t| t.duration_since(UNIX_EPOCH).map_err(std::io::Error::other))
            && now.saturating_sub(modified.as_secs() as i64) >= grace
        {
            // Do not allocate state for all historical sessions. When an
            // old file becomes active again, its initial cwd is read too.
            states.remove(&path);
            continue;
        }
        let state = states.entry(path.clone()).or_default();
        if let Err(err) = read_complete_lines(&path, state, codex, now) {
            eprintln!("transcript {}: {err:#}", path.display());
            continue;
        }
        if state.activity <= 0 || state.activity > now || now - state.activity >= grace {
            continue;
        }
        let cwd = if codex && !state.command_cwd.is_empty() {
            &state.command_cwd
        } else {
            &state.cwd
        };
        // Claude's encoded project directory is lossy (literal '-' and path
        // separators collide), so only an authoritative transcript cwd counts.
        let mut session_id = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if codex && session_id.len() > 36 {
            session_id = session_id
                .get(session_id.len() - 36..)
                .unwrap_or(&session_id)
                .to_owned();
        }
        let space_id = context.spaces.space_for_session(&session_id);
        if !context
            .patterns
            .iter()
            .any(|pattern| matches_cwd(pattern, cwd))
            && (space_id.is_empty() || !context.bound_spaces.contains(&space_id))
        {
            continue;
        }
        out.push(Signal {
            source: "transcript".into(),
            binary_name: if codex { "codex" } else { "" }.into(),
            cwd: cwd.clone(),
            window_title: session_id,
            space_id,
            ..Signal::default()
        });
    }
}

fn read_complete_lines(path: &Path, state: &mut Session, codex: bool, now: i64) -> Result<()> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut prefix = vec![0; size.min(128) as usize];
    file.read_exact(&mut prefix)?;
    // A shorter file is truncation; a changed prefix also catches replacement
    // and truncate/rewrite when the replacement has already exceeded the offset.
    if size < state.offset || !prefix.starts_with(&state.prefix) {
        *state = Session::default();
    }
    state.prefix = prefix;
    if size <= state.offset {
        return Ok(());
    }
    file.seek(SeekFrom::Start(state.offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let Some(end) = bytes.iter().rposition(|&b| b == b'\n').map(|pos| pos + 1) else {
        return Ok(());
    };
    for line in bytes[..end].split(|&b| b == b'\n') {
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let timestamp = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp());
        // Never let a bad clock pin this session as active or displace a valid
        // cwd. Later, valid records must still be able to revive the session.
        if timestamp.is_some_and(|ts| ts > now) {
            continue;
        }
        let ts = timestamp.unwrap_or(0);
        if let Some(timestamp) = timestamp {
            state.activity = state.activity.max(timestamp);
        }
        if codex {
            parse_codex_entry(&entry, state, ts);
        } else if let Some(cwd) = entry
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            && ts >= state.cwd_time
        {
            state.cwd = decode_file_url(cwd);
            state.cwd_time = ts;
        }
    }
    // Leave an incomplete final line in the file for the next read.
    state.offset += end as u64;
    Ok(())
}

fn parse_codex_entry(entry: &Value, state: &mut Session, ts: i64) {
    let payload = &entry["payload"];
    if matches!(
        entry["type"].as_str(),
        Some("session_meta" | "turn_context")
    ) && let Some(cwd) = payload["cwd"].as_str().filter(|s| !s.is_empty())
        && ts >= state.cwd_time
    {
        state.cwd = decode_file_url(cwd);
        state.cwd_time = ts;
    }
    let mut command = payload["item"]["cwd"].as_str().map(str::to_owned);
    // Current Codex rollouts also store exec_command's actual directory in
    // the function-call arguments rather than an item_completed event.
    if command.is_none()
        && payload["type"] == "function_call"
        && matches!(
            payload["name"].as_str(),
            Some("exec_command" | "shell_command" | "shell")
        )
    {
        let arguments = payload["arguments"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        command = arguments
            .as_ref()
            .and_then(|a| a["workdir"].as_str())
            .map(str::to_owned);
    }
    if let Some(command) = command {
        let command = decode_file_url(&command);
        if !command.is_empty() && !same_directory(&command, &state.cwd) && ts >= state.command_time
        {
            state.command_cwd = command;
            state.command_time = ts;
        }
    }
}

fn same_directory(a: &str, b: &str) -> bool {
    if a.contains('\\')
        || b.contains('\\')
        || a.as_bytes().get(1) == Some(&b':')
        || b.as_bytes().get(1) == Some(&b':')
    {
        a.replace('\\', "/")
            .trim_end_matches('/')
            .eq_ignore_ascii_case(b.replace('\\', "/").trim_end_matches('/'))
    } else {
        a.trim_end_matches('/') == b.trim_end_matches('/')
    }
}

/// Decode Unix, drive-letter, and UNC file URLs regardless of the host OS.
pub fn decode_file_url(value: &str) -> String {
    let Ok(url) = url::Url::parse(value) else {
        return value.into();
    };
    if url.scheme() != "file" {
        return value.into();
    }
    let bytes = url.path().as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| (byte as char).to_digit(16).map(|v| v as u8);
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    let path = String::from_utf8_lossy(&decoded);
    if let Some(host) = url
        .host_str()
        .filter(|h| !h.is_empty() && *h != "localhost")
    {
        return format!(
            "\\\\{}\\{}",
            host,
            path.trim_start_matches('/').replace('/', "\\")
        );
    }
    if path.starts_with('/') && path.as_bytes().get(2) == Some(&b':') {
        return path[1..].replace('/', "\\");
    }
    path.into_owned()
}

fn touched_since(path: &Path, since: i64) -> bool {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    [path.to_owned(), PathBuf::from(wal)].iter().any(|path| {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .is_some_and(|m| m.as_secs() as i64 >= since)
    })
}

fn collect_opencode(path: &Path, patterns: &[String], now: i64, grace: i64) -> Result<Vec<Signal>> {
    // READ_ONLY never creates the database and allows observing its live WAL.
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("open database read-only")?;
    conn.busy_timeout(std::time::Duration::from_millis(250))?;
    let mut statement = conn.prepare("SELECT s.id,s.directory FROM session_message m JOIN session_v2 s ON s.id=m.session_id WHERE m.time_created > ?1 AND m.time_created <= ?2 GROUP BY s.id,s.directory")?;
    let rows = statement.query_map(
        [
            (now - grace).saturating_mul(1000),
            now.saturating_mul(1000).saturating_add(999),
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (id, directory) = row?;
        let cwd = decode_file_url(&directory);
        if patterns.iter().any(|p| matches_cwd(p, &cwd)) {
            out.push(Signal {
                source: "transcript".into(),
                binary_name: "opencode".into(),
                cwd,
                window_title: id,
                ..Signal::default()
            });
        }
    }
    Ok(out)
}
