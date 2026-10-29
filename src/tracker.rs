use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    path::Path,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{
    config::Config,
    domain::{Signal, match_rules, matches_cwd},
    platform::{Platform, ProcessSample},
    store::Store,
    transcripts::TranscriptCollector,
};

#[derive(Default)]
struct CpuState {
    name: String,
    start_time: u64,
    cpu: u64,
    above: u32,
    below: u32,
    working: bool,
}

#[derive(Default)]
pub struct CpuTracker {
    processes: HashMap<u32, CpuState>,
}

impl CpuTracker {
    pub fn collect(
        &mut self,
        samples: &[ProcessSample],
        config: &Config,
        user_present: bool,
        watched: &HashSet<String>,
        patterns: &[String],
    ) -> Vec<Signal> {
        let live: HashSet<_> = samples.iter().map(|sample| sample.pid).collect();
        self.processes.retain(|pid, _| live.contains(pid));
        let threshold = if user_present {
            config.agent_cpu_threshold
        } else {
            config.agent_cpu_threshold_idle
        };
        let mut out = Vec::new();
        for sample in samples {
            let previous = self.processes.get_mut(&sample.pid);
            let Some(state) = previous.filter(|state| {
                state.start_time == sample.start_time
                    && state.name == sample.binary_name
                    && sample.cpu_centiseconds >= state.cpu
            }) else {
                self.processes.insert(
                    sample.pid,
                    CpuState {
                        name: sample.binary_name.clone(),
                        start_time: sample.start_time,
                        cpu: sample.cpu_centiseconds,
                        ..CpuState::default()
                    },
                );
                continue;
            };
            let delta = sample.cpu_centiseconds - state.cpu;
            state.cpu = sample.cpu_centiseconds;
            if delta >= threshold {
                state.above = state.above.saturating_add(1);
                state.below = 0;
                if state.above >= config.agent_busy_rise_ticks.max(1) {
                    state.working = true;
                }
            } else {
                state.below = state.below.saturating_add(1);
                state.above = 0;
                if state.below >= config.agent_busy_fall_ticks.max(1) {
                    state.working = false;
                }
            }
            if !state.working {
                continue;
            }
            let Some(cwd) = sample.cwd.as_ref().filter(|cwd| !cwd.is_empty()) else {
                continue;
            };
            if !watched
                .iter()
                .any(|binary| identifier_eq(binary, &sample.binary_name))
                && !patterns.iter().any(|pattern| matches_cwd(pattern, cwd))
            {
                continue;
            }
            out.push(Signal {
                source: "agent".into(),
                binary_name: sample.binary_name.clone(),
                cwd: cwd.clone(),
                ..Signal::default()
            });
        }
        out
    }
}

fn identifier_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) || a.to_lowercase().ends_with(".exe") || b.to_lowercase().ends_with(".exe") {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

#[derive(Default)]
struct ArmState {
    streak: u32,
    suppressed_ticks: u64,
    last_tick: Option<i64>,
}

/// Runtime presence policy above the store's basic pause/dedup guarantees.
/// Arming is process-local, like the original daemon; CLI requests are queued
/// in SQLite so commands work while the worker is running or stopped.
#[derive(Default)]
pub struct ProjectGate {
    armed: HashMap<i64, ArmState>,
}

impl ProjectGate {
    pub fn record_signals(
        &mut self,
        store: &mut Store,
        timestamp: i64,
        signals: &[Signal],
        user_present: bool,
    ) -> Result<usize> {
        let timestamp = timestamp - timestamp.rem_euclid(5);
        let rules = store.rules()?;
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let projects: Vec<(i64, bool)> = {
            let mut statement = tx.prepare("SELECT id,paused FROM projects")?;
            statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let live: HashSet<_> = projects.iter().map(|(id, _)| *id).collect();
        let mut paused: HashSet<_> = projects
            .iter()
            .filter_map(|(id, paused)| paused.then_some(*id))
            .collect();
        self.armed.retain(|id, _| live.contains(id));
        let requests: Vec<(String, String)> = {
            let mut statement = tx.prepare("SELECT key,value FROM runtime_state WHERE key='arm_all' OR key GLOB 'arm_project:*'")?;
            statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (key, generation) in requests {
            if key == "arm_all" {
                for id in &live {
                    self.armed.insert(*id, ArmState::default());
                }
            } else if let Some(id) = key
                .strip_prefix("arm_project:")
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|id| live.contains(id))
            {
                self.armed.insert(id, ArmState::default());
            }
            // Compare the generation so a newer CLI command is never consumed
            // as a side effect of processing an earlier request.
            tx.execute(
                "DELETE FROM runtime_state WHERE key=?1 AND value=?2",
                [key, generation],
            )?;
        }
        let mut allowed = Vec::new();
        let mut counted = HashSet::new();
        let mut just_armed = HashSet::new();
        for signal in signals {
            // Suppressed signals still leave observations for diagnostics, but
            // earn no ticks. Ignored observations cannot resume/disarm projects.
            let observation_id: i64 = tx.query_row(
                "INSERT INTO observations(source,bundle_id,window_title,binary_name,cwd,space_id,first_seen) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(source,bundle_id,window_title,binary_name,cwd,space_id) DO UPDATE SET first_seen=first_seen RETURNING id",
                params![signal.source,signal.bundle_id,signal.window_title,signal.binary_name,signal.cwd,signal.space_id,timestamp],|r|r.get(0))?;
            let ignored: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ignored_observations WHERE observation_id=?1)",
                [observation_id],
                |r| r.get(0),
            )?;
            if ignored {
                continue;
            }
            let Some(project) = match_rules(signal, &rules) else {
                allowed.push(signal.clone());
                continue;
            };
            if matches!(signal.source.as_str(), "focus" | "transcript") {
                self.armed.remove(&project);
            }
            if paused.contains(&project) {
                if signal.source == "transcript" {
                    tx.execute("UPDATE projects SET paused=0 WHERE id=?1", [project])?;
                    paused.remove(&project);
                } else if signal.source == "agent" && user_present {
                    tx.execute("UPDATE projects SET paused=0 WHERE id=?1", [project])?;
                    paused.remove(&project);
                    self.armed.insert(project, ArmState::default());
                    just_armed.insert(project);
                    continue;
                } else {
                    continue;
                }
            }
            if signal.source == "agent" {
                if just_armed.contains(&project) {
                    continue;
                }
                if let Some(arm) = self.armed.get_mut(&project) {
                    if !counted.insert(project) {
                        continue;
                    }
                    if user_present {
                        if arm.last_tick != Some(timestamp - 5) {
                            arm.streak = 0;
                        }
                        arm.streak = arm.streak.saturating_add(1);
                        arm.last_tick = Some(timestamp);
                    } else {
                        arm.streak = 0;
                        arm.last_tick = None;
                    }
                    if arm.streak >= 12 {
                        self.armed.remove(&project);
                    } else {
                        arm.suppressed_ticks = arm.suppressed_ticks.saturating_add(1);
                        continue;
                    }
                }
            }
            allowed.push(signal.clone());
        }
        for (project, arm) in &mut self.armed {
            if !counted.contains(project) {
                arm.streak = 0;
                arm.last_tick = None;
            }
        }
        tx.execute("DELETE FROM runtime_state WHERE key GLOB 'armed:*'", [])?;
        for (project, arm) in &self.armed {
            tx.execute("INSERT INTO runtime_state(key,value) VALUES(?1,?2)",[
                format!("armed:{project}"),
                serde_json::json!({"streak":arm.streak,"suppressed_ticks":arm.suppressed_ticks}).to_string(),
            ])?;
        }
        tx.commit()?;
        store.record_signals(timestamp, &allowed)
    }
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static CTRL_C: OnceLock<std::result::Result<(), String>> = OnceLock::new();

pub fn run(data_dir: &Path, config: &Config, once: bool) -> Result<()> {
    config.validate()?;
    fs::create_dir_all(data_dir).with_context(|| format!("create {}", data_dir.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_dir.join("tracker.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("a tracking worker already holds this data directory")?;
    let db_path = config
        .db_path
        .clone()
        .unwrap_or_else(|| data_dir.join("db.sqlite"));
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut store = Store::open(&db_path)?;
    if !once
        && let Err(err) = CTRL_C.get_or_init(|| {
            ctrlc::set_handler(|| SHUTDOWN.store(true, Ordering::SeqCst)).map_err(|e| e.to_string())
        })
    {
        anyhow::bail!("install Ctrl-C handler: {err}");
    }
    SHUTDOWN.store(false, Ordering::SeqCst);
    let started_at = Utc::now().timestamp();
    for (key, value) in [
        ("started_at", started_at.to_string()),
        ("heartbeat", started_at.to_string()),
        ("pid", std::process::id().to_string()),
        (
            "platform",
            if cfg!(windows) {
                "windows"
            } else {
                "unsupported"
            }
            .to_owned(),
        ),
        ("stop_requested", "0".into()),
        ("last_error", "".into()),
        // Publish running last, after stop state is initialized. A CLI that
        // observes running=1 can now safely submit a stop request immediately.
        ("running", "1".into()),
    ] {
        set_state(&store, key, &value)?;
    }
    let result = worker_loop(&mut store, config, once);
    // Release the OS lock even after a failed tick; leave an explicit status
    // and diagnostic instead of making a stale heartbeat look like a live worker.
    let status = set_state(&store, "running", "0");
    if let Err(ref err) = result {
        let _ = set_state(&store, "last_error", &format!("{err:#}"));
    }
    result.and(status)
}

fn worker_loop(store: &mut Store, config: &Config, once: bool) -> Result<()> {
    let mut platform = Platform::default();
    let mut cpu = CpuTracker::default();
    let mut transcripts = TranscriptCollector::default();
    let mut project_gate = ProjectGate::default();
    let interval = config.interval_duration()?.as_secs() as i64;
    let idle_threshold = config.idle_duration()?.as_secs().max(1);
    let mut last_tick = None;
    loop {
        if stop_requested(store)? {
            break;
        }
        let now = Utc::now().timestamp();
        let timestamp = now - now.rem_euclid(interval);
        // Wall-clock alignment avoids accumulating work+sleep drift and keeps
        // all projects in the same deduplicated five-second billing bucket.
        if last_tick != Some(timestamp) {
            let rules = store.rules()?;
            let patterns: Vec<_> = rules
                .iter()
                .filter_map(|r| r.match_cwd_prefix.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let bound_spaces: HashSet<_> = rules
                .iter()
                .filter_map(|r| r.match_space_id.clone())
                .collect();
            let watched = watched_programs(store)?;
            let idle = match platform.idle_seconds() {
                Ok(idle) => idle,
                Err(err) => {
                    eprintln!("idle: {err:#}");
                    None
                }
            };
            // A missing idle API must not assume presence and bill background
            // processes against the lower threshold. Transcripts remain usable.
            let user_present = idle.is_some_and(|seconds| seconds < idle_threshold);
            let mut signals = Vec::new();
            if user_present {
                match platform.foreground() {
                    Ok(Some(signal))
                        if watched
                            .0
                            .iter()
                            .any(|id| identifier_eq(id, &signal.bundle_id)) =>
                    {
                        signals.push(signal)
                    }
                    Ok(_) => {}
                    Err(err) => eprintln!("foreground: {err:#}"),
                }
            }
            let processes = platform.processes();
            signals.extend(cpu.collect(&processes, config, user_present, &watched.1, &patterns));
            signals.extend(transcripts.collect_with_spaces(config, &patterns, &bound_spaces, now));
            project_gate.record_signals(store, timestamp, &signals, user_present)?;
            set_state(store, "heartbeat", &now.to_string())?;
            set_state(
                store,
                "idle_seconds",
                &idle.map(|v| v.to_string()).unwrap_or_default(),
            )?;
            set_state(store, "idle", if user_present { "0" } else { "1" })?;
            if let Some(previous) = last_tick
                && timestamp > previous + interval
            {
                eprintln!(
                    "tracking: collection missed {} seconds; no activity is inferred for unobserved time",
                    timestamp - previous - interval
                );
            }
            last_tick = Some(timestamp);
            if once {
                break;
            }
        }
        // Short waits keep SQLite stop requests responsive without drifting the
        // next sampling deadline. Ctrl-C does not need a network/socket endpoint.
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn watched_programs(store: &Store) -> Result<(HashSet<String>, HashSet<String>)> {
    let mut statement = store
        .conn
        .prepare("SELECT kind,identifier FROM watched_programs")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut bundles = HashSet::new();
    let mut binaries = HashSet::new();
    for row in rows {
        let (kind, identifier) = row?;
        match kind.as_str() {
            "bundle" => {
                bundles.insert(identifier);
            }
            "binary" => {
                binaries.insert(identifier);
            }
            _ => {}
        }
    }
    Ok((bundles, binaries))
}

fn stop_requested(store: &Store) -> Result<bool> {
    if SHUTDOWN.load(Ordering::SeqCst) {
        return Ok(true);
    }
    let value: Option<String> = store
        .conn
        .query_row(
            "SELECT value FROM runtime_state WHERE key='stop_requested'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.as_deref().is_some_and(|v| matches!(v, "1" | "true")))
}

fn set_state(store: &Store, key: &str, value: &str) -> Result<()> {
    store.conn.execute("INSERT INTO runtime_state(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value])?;
    Ok(())
}
