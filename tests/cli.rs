use serde_json::{Value, json};
use std::process::{Command, Output};
use tempfile::TempDir;

fn run(dir: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rit"))
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--json")
        .args(args)
        .output()
        .unwrap()
}
fn ok(dir: &TempDir, args: &[&str]) -> Value {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("JSON output")
}

#[test]
fn companies_projects_rules_and_pause_work_without_daemon() {
    let d = TempDir::new().unwrap();
    ok(&d, &["company", "add", "Acme"]);
    ok(&d, &["project", "add", "api", "--company", "Acme"]);
    let projects = ok(&d, &["project", "list"]);
    assert_eq!(projects[0]["name"], "api");
    assert_eq!(projects[0]["company"], "Acme");
    ok(
        &d,
        &["rules", "add", "--project", "api", "--cwd", r"C:\Work\api"],
    );
    let rules = ok(&d, &["rules", "list"]);
    assert_eq!(rules[0]["match_cwd_prefix"], r"C:\Work\api");
    ok(&d, &["end-day"]);
    assert_eq!(ok(&d, &["project", "list"])[0]["paused"], true);
    ok(&d, &["start-day"]);
    assert_eq!(ok(&d, &["project", "list"])[0]["paused"], false);
    assert!(!run(&d, &["company", "add", "Acme"]).status.success());
}

#[test]
fn report_deduplicates_parallel_projects_per_company() {
    let d = TempDir::new().unwrap();
    ok(&d, &["company", "add", "Acme"]);
    for project in ["one", "two"] {
        ok(&d, &["project", "add", project, "--company", "Acme"]);
    }
    let c = rusqlite::Connection::open(d.path().join("db.sqlite")).unwrap();
    c.execute(
        "INSERT INTO observations(source,cwd,first_seen) VALUES('agent','C:/one',0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO observations(source,cwd,first_seen) VALUES('agent','C:/two',0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO ticks VALUES(1780272000,1,1),(1780272000,2,2),(1780272005,1,1)",
        [],
    )
    .unwrap();
    let report = ok(
        &d,
        &["report", "--from", "2026-06-01", "--to", "2026-06-03"],
    );
    assert_eq!(report["companies"][0]["seconds"], 10);
    assert_eq!(report["projects"][0]["seconds"], 10);
    assert_eq!(report["projects"][1]["seconds"], 5);
}

#[test]
fn report_includes_deduplicated_projects_without_a_company() {
    let d = TempDir::new().unwrap();
    ok(&d, &["project", "add", "one"]);
    ok(&d, &["project", "add", "two"]);
    let c = rusqlite::Connection::open(d.path().join("db.sqlite")).unwrap();
    c.execute("INSERT INTO observations(source,cwd,first_seen) VALUES('agent','C:/one',0),('agent','C:/two',0)",[]).unwrap();
    c.execute(
        "INSERT INTO ticks VALUES(1780272000,1,1),(1780272000,2,2)",
        [],
    )
    .unwrap();
    let report = ok(
        &d,
        &["report", "--from", "2026-06-01", "--to", "2026-06-03"],
    );
    assert_eq!(report["companies"][0]["name"], "(no company)");
    assert_eq!(report["companies"][0]["seconds"], 5);
}

#[test]
fn review_tags_history_and_never_overwrites_assigned_work() {
    let d = TempDir::new().unwrap();
    ok(&d, &["project", "add", "api"]);
    let c = rusqlite::Connection::open(d.path().join("db.sqlite")).unwrap();
    c.execute(
        "INSERT INTO observations(source,cwd,first_seen) VALUES('agent','C:/Work/api',0)",
        [],
    )
    .unwrap();
    c.execute("INSERT INTO ticks VALUES(100,1,NULL)", [])
        .unwrap();
    assert_eq!(ok(&d, &["review", "--list"])[0]["ticks"], 1);
    let tagged = ok(&d, &["review", "--id", "1", "--project", "api"]);
    assert_eq!(tagged["retagged"], 1);
    assert_eq!(ok(&d, &["review", "--list"]), json!([]));
}

#[test]
fn invalid_rules_and_reset_require_explicit_action() {
    let d = TempDir::new().unwrap();
    ok(&d, &["project", "add", "api"]);
    assert!(
        !run(
            &d,
            &["rules", "add", "--project", "api", "--cwd", r"C:\*\api"]
        )
        .status
        .success()
    );
    assert!(!run(&d, &["reset", "all"]).status.success());
    assert_eq!(ok(&d, &["project", "list"]).as_array().unwrap().len(), 1);
    ok(&d, &["reset", "all", "--yes"]);
    assert_eq!(ok(&d, &["project", "list"]), json!([]));
}

#[test]
fn config_init_is_non_destructive_and_watch_normalizes_executables() {
    let d = TempDir::new().unwrap();
    ok(&d, &["config", "init"]);
    assert!(!run(&d, &["config", "init"]).status.success());
    ok(&d, &["watch", "add", "app", "Code.exe"]);
    assert_eq!(ok(&d, &["watch", "list"])[0]["identifier"], "code.exe");
}

#[test]
fn transcript_import_is_idempotent_and_respects_existing_shifted_ticks() {
    use chrono::{Local, TimeZone};
    let d = TempDir::new().unwrap();
    ok(&d, &["project", "add", "api"]);
    ok(
        &d,
        &["rules", "add", "--project", "api", "--cwd", "C:/Work/api"],
    );
    let root = d.path().join("transcripts");
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let start = Local
        .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .unwrap()
        .timestamp();
    let line = |ts| {
        json!({"timestamp":chrono::DateTime::from_timestamp(ts,0).unwrap().to_rfc3339(),"cwd":"C:/Work/api"}).to_string()+"\n"
    };
    std::fs::write(project.join("s.jsonl"), line(start) + &line(start + 10)).unwrap();
    std::fs::write(
        d.path().join("config.yaml"),
        format!(
            "transcript_root: '{}'\ncodex_root: off\nopencode_db: off\n",
            root.display()
        ),
    )
    .unwrap();
    let c = rusqlite::Connection::open(d.path().join("db.sqlite")).unwrap();
    c.execute(
        "INSERT INTO observations(source,cwd,first_seen) VALUES('agent','C:/Work/api',0)",
        [],
    )
    .unwrap();
    // Existing live tracking used another grid phase immediately before the
    // first transcript record. It covers that record, but not the next two.
    c.execute("INSERT INTO ticks VALUES(?1,1,1)", [start - 1])
        .unwrap();
    let first = ok(
        &d,
        &[
            "import-transcripts",
            "--from",
            "2026-06-01",
            "--to",
            "2026-06-02",
        ],
    );
    assert_eq!(first["inserted"], 2);
    assert_eq!(
        ok(
            &d,
            &[
                "import-transcripts",
                "--from",
                "2026-06-01",
                "--to",
                "2026-06-02"
            ]
        )["inserted"],
        0
    );
    assert_eq!(
        ok(
            &d,
            &["report", "--from", "2026-06-01", "--to", "2026-06-02"]
        )["projects"][0]["seconds"],
        15
    );
}

#[test]
fn transcript_import_does_not_bridge_time_spent_in_another_project() {
    use chrono::{Local, TimeZone};
    let d = TempDir::new().unwrap();
    for name in ["A", "B"] {
        ok(&d, &["company", "add", name]);
        ok(&d, &["project", "add", name, "--company", name]);
        ok(
            &d,
            &[
                "rules",
                "add",
                "--project",
                name,
                "--cwd",
                &format!("C:/Work/{name}"),
            ],
        );
    }
    let root = d.path().join("transcripts/project");
    std::fs::create_dir_all(&root).unwrap();
    let start = Local
        .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .unwrap()
        .timestamp();
    let mut lines = String::new();
    for (i, name) in ["A", "A", "B", "B", "A", "A"].into_iter().enumerate() {
        lines+=&(json!({"timestamp":chrono::DateTime::from_timestamp(start+i as i64*5,0).unwrap().to_rfc3339(),"cwd":format!("C:/Work/{name}")}).to_string()+"\n");
    }
    std::fs::write(root.join("s.jsonl"), lines).unwrap();
    std::fs::write(
        d.path().join("config.yaml"),
        format!(
            "transcript_root: '{}'\ncodex_root: off\nopencode_db: off\n",
            root.parent().unwrap().display()
        ),
    )
    .unwrap();
    assert_eq!(
        ok(
            &d,
            &[
                "import-transcripts",
                "--from",
                "2026-06-01",
                "--to",
                "2026-06-02"
            ]
        )["inserted"],
        6
    );
    let report = ok(
        &d,
        &["report", "--from", "2026-06-01", "--to", "2026-06-02"],
    );
    assert_eq!(report["companies"][0]["seconds"], 20);
    assert_eq!(report["companies"][1]["seconds"], 10);
}

#[test]
fn live_claude_signatures_share_review_rules_and_ignore_state_with_import() {
    use chrono::{Local, TimeZone};
    use rust_in_time::{config::Config, store::Store, transcripts::TranscriptCollector};
    let d = TempDir::new().unwrap();
    ok(&d, &["project", "add", "api"]);
    let root = d.path().join("transcripts/project");
    std::fs::create_dir_all(&root).unwrap();
    let start = Local
        .with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .unwrap()
        .timestamp();
    let line = |ts| {
        json!({"timestamp":chrono::DateTime::from_timestamp(ts,0).unwrap().to_rfc3339(),"cwd":"C:/Work/api"}).to_string()+"\n"
    };
    std::fs::write(
        root.join("s.jsonl"),
        line(start) + &line(start + 10) + &line(Local::now().timestamp() - 1),
    )
    .unwrap();
    std::fs::write(
        d.path().join("config.yaml"),
        format!(
            "transcript_root: '{}'\ncodex_root: off\nopencode_db: off\n",
            root.parent().unwrap().display()
        ),
    )
    .unwrap();
    let config = Config::load(&d.path().join("config.yaml")).unwrap();
    let signals = TranscriptCollector::default().collect(
        &config,
        &["C:/Work/api".into()],
        Local::now().timestamp(),
    );
    assert_eq!(signals.len(), 1);
    let mut store = Store::open(&d.path().join("db.sqlite")).unwrap();
    store
        .record_signals(Local::now().timestamp(), &signals)
        .unwrap();
    ok(&d, &["review", "--id", "1", "--project", "api"]);
    assert_eq!(
        ok(
            &d,
            &[
                "import-transcripts",
                "--from",
                "2026-06-01",
                "--to",
                "2026-06-02"
            ]
        )["inserted"],
        3
    );
    ok(&d, &["review", "--id", "1", "--ignore"]);
    store
        .conn
        .execute("DELETE FROM ticks WHERE ts<?", [start + 100])
        .unwrap();
    assert_eq!(
        ok(
            &d,
            &[
                "import-transcripts",
                "--from",
                "2026-06-01",
                "--to",
                "2026-06-02"
            ]
        )["inserted"],
        0
    );
}
