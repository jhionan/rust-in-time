use std::{collections::HashSet, fs, io::Write, path::Path};

use chrono::{DateTime, Utc};
use rust_in_time::{
    config::Config,
    platform::ProcessSample,
    tracker::CpuTracker,
    transcripts::{TranscriptCollector, decode_file_url},
};

fn sample(cpu: u64) -> ProcessSample {
    ProcessSample {
        pid: 42,
        start_time: 1,
        binary_name: "agent.exe".into(),
        cpu_centiseconds: cpu,
        cwd: Some("C:\\work\\project".into()),
    }
}

#[test]
fn tracking_cpu_requires_sustained_work_and_preserves_brief_dips() {
    let config = Config::default();
    let mut tracker = CpuTracker::default();
    let watched = HashSet::new();
    let patterns = vec!["C:\\work\\project".into()];
    let mut counts = Vec::new();
    for cpu in [0, 15, 30, 30, 30, 30, 45, 60] {
        counts.push(
            tracker
                .collect(&[sample(cpu)], &config, true, &watched, &patterns)
                .len(),
        );
    }
    assert_eq!(counts, [0, 0, 1, 1, 1, 0, 0, 1]);
}

#[test]
fn tracking_idle_threshold_and_pid_reuse_cannot_inherit_busy_state() {
    let config = Config::default();
    let mut tracker = CpuTracker::default();
    let watched = HashSet::from(["agent.exe".into()]);
    let mut counts = Vec::new();
    for cpu in [0, 99, 198, 298, 398] {
        counts.push(
            tracker
                .collect(&[sample(cpu)], &config, false, &watched, &[])
                .len(),
        );
    }
    assert_eq!(counts, [0, 0, 0, 0, 1]);
    let mut reused = sample(500);
    reused.start_time = 2;
    assert!(
        tracker
            .collect(&[reused], &config, false, &watched, &[])
            .is_empty()
    );
    assert!(
        tracker
            .collect(&[sample(1)], &config, false, &watched, &[])
            .is_empty()
    );
}

#[test]
fn tracking_unreadable_cwd_is_never_inferred_and_unwatched_cwds_are_skipped() {
    let config = Config::default();
    let mut tracker = CpuTracker::default();
    let watched = HashSet::from(["agent.exe".into()]);
    let mut process = sample(0);
    process.cwd = None;
    for cpu in [0, 100, 200] {
        process.cpu_centiseconds = cpu;
        assert!(
            tracker
                .collect(&[process.clone()], &config, true, &watched, &[])
                .is_empty()
        );
    }
    process.cwd = Some("C:\\untracked".into());
    process.cpu_centiseconds = 300;
    assert!(
        tracker
            .collect(
                &[process],
                &config,
                true,
                &HashSet::new(),
                &["C:\\work".into()]
            )
            .is_empty()
    );
}

fn config_for(root: &Path) -> Config {
    Config {
        transcript_root: root.to_owned(),
        codex_root: "off".into(),
        opencode_db: "off".into(),
        ..Config::default()
    }
}

fn stamp(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0).unwrap().to_rfc3339()
}

fn claude_line(ts: i64, cwd: &str) -> String {
    serde_json::json!({"timestamp": stamp(ts), "cwd": cwd}).to_string() + "\n"
}

#[test]
fn tracking_claude_retains_partial_records_and_resets_truncated_session_state() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("lossy-project-dir");
    fs::create_dir(&project).unwrap();
    let file = project.join("session.jsonl");
    let now = Utc::now().timestamp();
    let line = claude_line(now - 1, "C:\\work\\project");
    let midpoint = line.len() / 2;
    fs::write(&file, &line[..midpoint]).unwrap();
    let mut collector = TranscriptCollector::default();
    let config = config_for(dir.path());
    let patterns = vec!["C:\\work\\project".into()];
    assert!(collector.collect(&config, &patterns, now).is_empty());
    fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(&line.as_bytes()[midpoint..])
        .unwrap();
    let signals = collector.collect(&config, &patterns, now);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].cwd, "C:\\work\\project");
    assert_eq!(signals[0].binary_name, "");
    // Truncation must forget the former recent timestamp and cwd.
    fs::write(&file, "{}\n").unwrap();
    assert!(collector.collect(&config, &patterns, now).is_empty());
}

#[test]
fn tracking_future_timestamps_do_not_poison_later_valid_activity() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir(&project).unwrap();
    let path = project.join("session.jsonl");
    let now = Utc::now().timestamp();
    fs::write(&path, claude_line(now + 3600, "/work/project")).unwrap();
    let mut collector = TranscriptCollector::default();
    let config = config_for(dir.path());
    let patterns = vec!["/work/project".into()];
    assert!(collector.collect(&config, &patterns, now).is_empty());
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(claude_line(now - 1, "/work/project").as_bytes())
        .unwrap();
    assert_eq!(collector.collect(&config, &patterns, now).len(), 1);
    assert!(collector.collect(&config, &patterns, now + 600).is_empty());
}

#[test]
fn tracking_windows_file_urls_keep_drive_and_unc_directories() {
    assert_eq!(
        decode_file_url("file:///C:/Work/my%20repo"),
        "C:\\Work\\my repo"
    );
    assert_eq!(
        decode_file_url("file://server/share/my%20repo"),
        "\\\\server\\share\\my repo"
    );
    assert_eq!(decode_file_url("file:///work/my%20repo"), "/work/my repo");
}

#[test]
fn tracking_codex_command_directory_survives_workspace_commands_and_turns() {
    let dir = tempfile::tempdir().unwrap();
    let day = dir.path().join("2025/01/01");
    fs::create_dir_all(&day).unwrap();
    let file = day.join("rollout-session.jsonl");
    let now = Utc::now().timestamp();
    let event = |typ: &str, payload: serde_json::Value| {
        serde_json::json!({"timestamp":stamp(now - 1),"type":typ,"payload":payload}).to_string()
            + "\n"
    };
    fs::write(&file, event("session_meta", serde_json::json!({"cwd":"C:\\work"})) +
        &event("event_msg", serde_json::json!({"type":"item_completed","item":{"cwd":"file:///C:/work/project"}}))).unwrap();
    let config = Config {
        transcript_root: "off".into(),
        codex_root: dir.path().to_owned(),
        opencode_db: "off".into(),
        ..Config::default()
    };
    let patterns = vec!["C:\\work\\project".into()];
    let mut collector = TranscriptCollector::default();
    assert_eq!(
        collector.collect(&config, &patterns, now)[0].cwd,
        "C:\\work\\project"
    );
    let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(event("turn_context", serde_json::json!({"cwd":"C:\\work"})).as_bytes())
        .unwrap();
    f.write_all(
        event(
            "event_msg",
            serde_json::json!({"item":{"cwd":"file:///C:/work"}}),
        )
        .as_bytes(),
    )
    .unwrap();
    let signals = collector.collect(&config, &patterns, now);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].cwd, "C:\\work\\project");
}

#[test]
fn tracking_config_accepts_invoice_sections_and_rejects_invalid_runtime_values() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.yaml");
    fs::write(&file, "interval: 30s\ntranscript_tracking: false\nsenders:\n  acme:\n    name: Acme\nclients: {}\ninvoice: {}\n").unwrap();
    let config = Config::load(&file).unwrap();
    assert!(!config.transcript_tracking);
    assert_eq!(config.interval_duration().unwrap().as_secs(), 5);
    fs::write(&file, "idle_threshold: 0s\n").unwrap();
    assert!(Config::load(&file).is_err());
    fs::write(&file, "agent_busy_rise_ticks: 0\n").unwrap();
    assert!(Config::load(&file).is_err());
    assert!(Config::load(&dir.path().join("missing.yaml")).is_ok());
}

#[test]
fn tracking_relative_config_paths_follow_the_config_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.yaml");
    fs::write(&file, "db_path: data/db.sqlite\ntranscript_root: transcripts\ncodex_root: codex\nopencode_db: off\n").unwrap();
    let config = Config::load(&file).unwrap();
    assert_eq!(config.db_path.unwrap(), dir.path().join("data/db.sqlite"));
    assert_eq!(config.transcript_root, dir.path().join("transcripts"));
    assert_eq!(config.codex_root, dir.path().join("codex"));
    assert_eq!(config.opencode_db, Path::new("off"));
}

#[test]
fn tracking_opencode_is_read_only_and_excludes_future_messages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    let config = Config {
        transcript_root: "off".into(),
        codex_root: "off".into(),
        opencode_db: path.clone(),
        ..Config::default()
    };
    let now = Utc::now().timestamp();
    let mut collector = TranscriptCollector::default();
    let patterns = vec!["/work/project".into()];
    assert!(collector.collect(&config, &patterns, now).is_empty());
    assert!(!path.exists());
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT); CREATE TABLE session_message(session_id TEXT,time_created INTEGER); INSERT INTO session_v2 VALUES('active','/work/project'),('future','/work/project');").unwrap();
    db.execute(
        "INSERT INTO session_message VALUES('active',?1),('future',?2)",
        [(now - 1) * 1000, (now + 1) * 1000],
    )
    .unwrap();
    let signals = collector.collect(&config, &patterns, now);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].window_title, "active");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM session_message", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn tracking_space_only_rule_accepts_known_session_and_rejects_absent_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir(&project).unwrap();
    let now = Utc::now().timestamp();
    fs::write(
        project.join("session.jsonl"),
        claude_line(now - 1, "/untracked"),
    )
    .unwrap();
    let metadata = dir.path().join("space.json");
    fs::write(&metadata,r#"{"workspaces":[{"id":"workspace","tabs":[{"panes":{"pane":{"agent_session":{"value":"session"}}}}]}]}"#).unwrap();
    let mut collector = TranscriptCollector::new(metadata.clone());
    let config = config_for(dir.path());
    let bound = HashSet::from(["workspace".into()]);
    let signals = collector.collect_with_spaces(&config, &[], &bound, now);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].space_id, "workspace");
    fs::remove_file(metadata).unwrap();
    assert!(
        collector
            .collect_with_spaces(&config, &[], &bound, now)
            .is_empty()
    );
}

#[test]
fn tracking_worker_once_records_all_projects_without_timestamp_multiplication() {
    use rust_in_time::{domain::Rule, store::Store, tracker};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("transcripts");
    fs::create_dir_all(root.join("projects")).unwrap();
    let now = Utc::now().timestamp();
    fs::write(
        root.join("projects/one.jsonl"),
        claude_line(now - 1, "/work/one"),
    )
    .unwrap();
    fs::write(
        root.join("projects/two.jsonl"),
        claude_line(now - 1, "/work/two"),
    )
    .unwrap();
    fs::write(
        root.join("projects/duplicate.jsonl"),
        claude_line(now - 1, "/work/one"),
    )
    .unwrap();
    let path = dir.path().join("db.sqlite");
    let mut store = Store::open(&path).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO projects(id,name,created_at) VALUES(1,'one',0),(2,'two',0)",
            [],
        )
        .unwrap();
    for (project_id, cwd) in [(1, "/work/one"), (2, "/work/two")] {
        store
            .add_rule(
                &Rule {
                    project_id,
                    match_cwd_prefix: Some(cwd.into()),
                    ..Rule::default()
                },
                false,
            )
            .unwrap();
    }
    tracker::run(dir.path(), &config_for(&root), true).unwrap();
    let totals: (i64, i64) = store
        .conn
        .query_row(
            "SELECT COUNT(*),COUNT(DISTINCT project_id) FROM ticks WHERE project_id IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(totals, (2, 2));
    let runtime: String = store
        .conn
        .query_row(
            "SELECT value FROM runtime_state WHERE key='running'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(runtime, "0");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("tracker.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    assert!(tracker::run(dir.path(), &config_for(&root), true).is_err());
}

#[test]
fn tracking_worker_honors_sqlite_stop_request_and_releases_its_lock() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("config.yaml"),
        "transcript_tracking: false\n",
    )
    .unwrap();
    let store = rust_in_time::store::Store::open(&dir.path().join("db.sqlite")).unwrap();
    let mut worker = Worker(
        Command::new(env!("CARGO_BIN_EXE_rit"))
            .arg("--data-dir")
            .arg(dir.path())
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let started: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_state WHERE key='running' AND value='1')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if started {
            break;
        }
        assert!(
            worker.0.try_wait().unwrap().is_none(),
            "worker exited before starting"
        );
        assert!(Instant::now() < deadline, "worker did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    store
        .conn
        .execute(
            "UPDATE runtime_state SET value='1' WHERE key='stop_requested'",
            [],
        )
        .unwrap();
    loop {
        if let Some(status) = worker.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "worker ignored stop request");
        std::thread::sleep(Duration::from_millis(20));
    }
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("tracker.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let running: String = store
        .conn
        .query_row(
            "SELECT value FROM runtime_state WHERE key='running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(running, "0");
}

fn gate_store() -> rust_in_time::store::Store {
    use rust_in_time::{domain::Rule, store::Store};
    let mut store = Store::memory().unwrap();
    store
        .conn
        .execute(
            "INSERT INTO projects(id,name,created_at) VALUES(1,'one',0)",
            [],
        )
        .unwrap();
    store
        .add_rule(
            &Rule {
                project_id: 1,
                match_cwd_prefix: Some("/work/one".into()),
                ..Rule::default()
            },
            false,
        )
        .unwrap();
    store
}

fn project_signal(source: &str) -> rust_in_time::domain::Signal {
    rust_in_time::domain::Signal {
        source: source.into(),
        cwd: "/work/one".into(),
        ..Default::default()
    }
}

#[test]
fn tracking_armed_project_suppresses_idle_cpu_and_disarms_on_focus_or_sustained_presence() {
    use rust_in_time::tracker::ProjectGate;
    let mut store = gate_store();
    store
        .conn
        .execute(
            "INSERT INTO runtime_state VALUES('arm_all','generation')",
            [],
        )
        .unwrap();
    let mut gate = ProjectGate::default();
    let agents = [
        project_signal("agent"),
        rust_in_time::domain::Signal {
            binary_name: "other".into(),
            ..project_signal("agent")
        },
    ];
    for tick in 0..20 {
        assert_eq!(
            gate.record_signals(&mut store, tick * 5, &agents, false)
                .unwrap(),
            0
        );
    }
    // Multiple PIDs for one project advance the 60s gate only once per tick.
    for tick in 20..31 {
        assert_eq!(
            gate.record_signals(&mut store, tick * 5, &agents, true)
                .unwrap(),
            0
        );
    }
    assert_eq!(
        gate.record_signals(&mut store, 155, &agents, true).unwrap(),
        2
    );
    store
        .conn
        .execute(
            "INSERT INTO runtime_state VALUES('arm_project:1','next')",
            [],
        )
        .unwrap();
    assert_eq!(
        gate.record_signals(&mut store, 160, &[project_signal("agent")], false)
            .unwrap(),
        0
    );
    assert_eq!(
        gate.record_signals(&mut store, 165, &[project_signal("focus")], true)
            .unwrap(),
        1
    );
    assert_eq!(
        gate.record_signals(&mut store, 170, &[project_signal("agent")], false)
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM runtime_state WHERE key IN ('arm_all','arm_project:1')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn tracking_paused_cpu_resumes_only_when_present_and_transcript_counts_immediately() {
    use rust_in_time::tracker::ProjectGate;
    let mut store = gate_store();
    store
        .conn
        .execute("UPDATE projects SET paused=1", [])
        .unwrap();
    let mut gate = ProjectGate::default();
    assert_eq!(
        gate.record_signals(&mut store, 0, &[project_signal("focus")], true)
            .unwrap(),
        0
    );
    assert_eq!(
        gate.record_signals(&mut store, 5, &[project_signal("agent")], false)
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT paused FROM projects WHERE id=1", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        gate.record_signals(&mut store, 10, &[project_signal("agent")], true)
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT paused FROM projects WHERE id=1", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        gate.record_signals(&mut store, 15, &[project_signal("agent")], false)
            .unwrap(),
        0
    );
    assert_eq!(
        gate.record_signals(&mut store, 20, &[project_signal("transcript")], false)
            .unwrap(),
        1
    );
    assert_eq!(
        gate.record_signals(&mut store, 25, &[project_signal("agent")], false)
            .unwrap(),
        1
    );
    store
        .conn
        .execute("UPDATE projects SET paused=1", [])
        .unwrap();
    assert_eq!(
        gate.record_signals(&mut store, 30, &[project_signal("transcript")], false)
            .unwrap(),
        1
    );
}

#[test]
fn tracking_ignored_transcript_cannot_resume_or_disarm_a_project() {
    use rust_in_time::tracker::ProjectGate;
    let mut store = gate_store();
    store.conn.execute_batch("UPDATE projects SET paused=1; INSERT INTO observations(source,cwd,first_seen) VALUES('transcript','/work/one',0); INSERT INTO ignored_observations VALUES(1,0); INSERT INTO runtime_state VALUES('arm_project:1','generation');").unwrap();
    let mut gate = ProjectGate::default();
    assert_eq!(
        gate.record_signals(&mut store, 0, &[project_signal("transcript")], false)
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT paused FROM projects WHERE id=1", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM runtime_state WHERE key='armed:1'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn tracking_armed_presence_streak_requires_consecutive_ticks() {
    use rust_in_time::tracker::ProjectGate;
    let mut store = gate_store();
    store
        .conn
        .execute(
            "INSERT INTO runtime_state VALUES('arm_all','generation')",
            [],
        )
        .unwrap();
    let mut gate = ProjectGate::default();
    for tick in 0..11 {
        assert_eq!(
            gate.record_signals(&mut store, tick * 5, &[project_signal("agent")], true)
                .unwrap(),
            0
        );
    }
    gate.record_signals(&mut store, 55, &[], true).unwrap();
    assert_eq!(
        gate.record_signals(&mut store, 60, &[project_signal("agent")], true)
            .unwrap(),
        0
    );
    let status: String = store
        .conn
        .query_row(
            "SELECT value FROM runtime_state WHERE key='armed:1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&status).unwrap()["streak"],
        1
    );
}
