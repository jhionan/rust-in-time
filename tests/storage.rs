use rusqlite::params;
use rust_in_time::domain::{
    Rule, Signal, match_rules, matches_cwd, propose_cwd, validate_cwd_pattern,
};
use rust_in_time::store::Store;

fn project(store: &Store, name: &str) -> i64 {
    store
        .conn
        .execute("INSERT INTO projects(name,created_at) VALUES (?,0)", [name])
        .unwrap();
    store.conn.last_insert_rowid()
}

fn agent(cwd: &str) -> Signal {
    Signal {
        source: "agent".into(),
        binary_name: "claude".into(),
        cwd: cwd.into(),
        ..Signal::default()
    }
}

fn cwd_rule(project_id: i64, cwd: &str) -> Rule {
    Rule {
        project_id,
        priority: 100,
        match_cwd_prefix: Some(cwd.into()),
        ..Rule::default()
    }
}

fn count(store: &Store, query: &str) -> i64 {
    store.conn.query_row(query, [], |r| r.get(0)).unwrap()
}

#[test]
fn windows_paths_ignore_case_and_slash_direction_but_respect_boundaries() {
    for cwd in [r"C:\Work\App", "c:/work/APP/src", r"C:\WORK\app\"] {
        assert!(matches_cwd(r"c:\work\app\", cwd), "{cwd}");
    }
    assert!(!matches_cwd(r"C:\work\app", r"C:\work\apple"));
    assert!(matches_cwd(
        r"\\Server\Share\Repo",
        "//server/share/REPO/src"
    ));
    assert!(!matches_cwd("/work/App", "/work/app/src"));
    assert!(matches_cwd("/work/App/", "/work/App/src"));
}

#[test]
fn wildcard_matches_final_segment_and_nested_work_directories() {
    assert!(matches_cwd(
        r"C:\Work\wt\md-*",
        "c:/work/wt/MD-engine/build/tmp"
    ));
    assert!(matches_cwd("/wt/md-*", "/wt/md-engine/build"));
    assert!(!matches_cwd("/wt/md-*", "/wt/other/md-engine"));
    assert!(!matches_cwd("/wt/md-*", "/wt"));
    assert!(!matches_cwd("/wt/*-md", "/wt/a/b-md"));
    assert!(!matches_cwd("", "/anything"));
}

#[test]
fn validates_only_terminal_wildcards_in_a_path() {
    for pattern in [
        "/wt/md-*",
        "/wt/md-*/",
        "/wt/repo",
        "/*",
        r"C:\wt\md-*",
        r"\\server\share\repo",
    ] {
        assert!(validate_cwd_pattern(pattern).is_ok(), "{pattern}");
    }
    for pattern in [
        "",
        "/",
        "*",
        "md-*",
        "/wt/*/repo",
        "/wt/*-md",
        "/wt/md-*x",
        "/wt/**",
        "/wt/repo?",
        "/wt/[ab]",
    ] {
        assert!(validate_cwd_pattern(pattern).is_err(), "{pattern}");
    }
}

#[test]
fn matching_is_conjunctive_and_uses_priority_then_id() {
    let mut signal = agent("/repo/app/src");
    signal.bundle_id = "Code.exe".into();
    signal.window_title = "app - Code".into();
    signal.space_id = "work".into();
    let specific = Rule {
        id: 9,
        project_id: 1,
        priority: 10,
        match_bundle_id: Some("Code.exe".into()),
        match_binary_name: Some("claude".into()),
        match_title_substr: Some("app".into()),
        match_cwd_prefix: Some("/repo/app".into()),
        match_space_id: Some("work".into()),
    };
    let broad = Rule {
        id: 4,
        project_id: 2,
        priority: 20,
        match_binary_name: Some("claude".into()),
        ..Rule::default()
    };
    assert_eq!(
        match_rules(&signal, &[broad.clone(), specific.clone()]),
        Some(1)
    );
    let tie = Rule {
        id: 2,
        project_id: 3,
        priority: 10,
        ..broad.clone()
    };
    assert_eq!(match_rules(&signal, &[specific.clone(), tie]), Some(3));
    for field in ["bundle", "title", "binary", "cwd", "space"] {
        let mut wrong = signal.clone();
        match field {
            "bundle" => wrong.bundle_id = "other.exe".into(),
            "title" => wrong.window_title = "App".into(),
            "binary" => wrong.binary_name = "codex".into(),
            "cwd" => wrong.cwd = "/repo/apple".into(),
            _ => wrong.space_id.clear(),
        }
        assert_eq!(
            match_rules(&wrong, std::slice::from_ref(&specific)),
            None,
            "{field}"
        );
    }
    let empty_space = Rule {
        match_space_id: Some(String::new()),
        ..specific
    };
    signal.space_id.clear();
    assert_eq!(match_rules(&signal, &[empty_space]), None);
}

#[test]
fn proposed_cwd_uses_last_project_directory_or_parent() {
    assert_eq!(propose_cwd("/work/app/src", "App"), "/work/app/");
    assert_eq!(
        propose_cwd("/app/packages/app/src", "app"),
        "/app/packages/app/"
    );
    assert_eq!(propose_cwd("/scratch/experiment", "app"), "/scratch/");
    assert_eq!(propose_cwd(r"C:\Work\App\src", "app"), "C:/Work/App/");
}

#[test]
fn proposed_cwd_cleans_dot_segments_and_preserves_roots() {
    assert_eq!(propose_cwd("/work/app/../other/src", "app"), "/work/other/");
    assert_eq!(propose_cwd("/work//app/./src", "app"), "/work/app/");
    assert_eq!(propose_cwd("/", "app"), "/");
    assert_eq!(propose_cwd(r"C:\", "app"), "C:/");
}

#[test]
fn opens_full_schema_with_foreign_keys_and_persistent_wal() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested/db.sqlite");
    let store = Store::open(&path).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO runtime_state(key,value) VALUES ('heartbeat','100')",
            [],
        )
        .unwrap();
    assert_eq!(count(&store, "PRAGMA foreign_keys"), 1);
    let journal: String = store
        .conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    for table in [
        "companies",
        "projects",
        "watched_programs",
        "rules",
        "observations",
        "ignored_observations",
        "ticks",
        "invoices",
        "sender_state",
    ] {
        store
            .conn
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap();
    }
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        count(
            &reopened,
            "SELECT CAST(value AS INTEGER) FROM runtime_state WHERE key='heartbeat'"
        ),
        100
    );
}

#[test]
fn duplicate_signals_and_five_second_buckets_do_not_add_time() {
    let mut store = Store::memory().unwrap();
    let p = project(&store, "app");
    store.add_rule(&cwd_rule(p, "/repo/app"), false).unwrap();
    let sig = agent("/repo/app/src");
    assert_eq!(
        store
            .record_signals(1001, &[sig.clone(), sig.clone()])
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .record_signals(1004, std::slice::from_ref(&sig))
            .unwrap(),
        0
    );
    assert_eq!(store.record_signals(1005, &[sig]).unwrap(), 1);
    assert_eq!(count(&store, "SELECT COUNT(*) FROM observations"), 1);
    assert_eq!(count(&store, "SELECT MIN(ts) FROM ticks"), 1000);
    assert_eq!(count(&store, "SELECT COUNT(DISTINCT ts)*5 FROM ticks"), 10);
}

#[test]
fn parallel_signals_dedup_project_and_company_totals() {
    let mut store = Store::memory().unwrap();
    store
        .conn
        .execute(
            "INSERT INTO companies(name,created_at) VALUES ('client',0)",
            [],
        )
        .unwrap();
    let a = project(&store, "app");
    let b = project(&store, "api");
    store
        .conn
        .execute("UPDATE projects SET company_id=1", [])
        .unwrap();
    store.add_rule(&cwd_rule(a, "/repo/app"), false).unwrap();
    store.add_rule(&cwd_rule(b, "/repo/api"), false).unwrap();
    store
        .record_signals(
            1000,
            &[
                agent("/repo/app"),
                Signal {
                    binary_name: "codex".into(),
                    ..agent("/repo/app")
                },
                agent("/repo/api"),
            ],
        )
        .unwrap();
    store.record_signals(1005, &[agent("/repo/app")]).unwrap();
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(DISTINCT ts)*5 FROM ticks WHERE project_id=1"
        ),
        10
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(DISTINCT t.ts)*5 FROM ticks t JOIN projects p ON p.id=t.project_id WHERE p.company_id=1"
        ),
        10
    );
}

#[test]
fn paused_projects_drop_focus_and_cpu_then_transcripts_resume() {
    let mut store = Store::memory().unwrap();
    let p = project(&store, "app");
    store.add_rule(&cwd_rule(p, "/repo/app"), false).unwrap();
    store
        .conn
        .execute("UPDATE projects SET paused=1", [])
        .unwrap();
    let sig = agent("/repo/app");
    assert_eq!(
        store
            .record_signals(
                1000,
                &[
                    sig.clone(),
                    Signal {
                        source: "focus".into(),
                        ..sig.clone()
                    }
                ]
            )
            .unwrap(),
        0
    );
    assert_eq!(count(&store, "SELECT paused FROM projects"), 1);
    assert_eq!(count(&store, "SELECT COUNT(*) FROM observations"), 2);
    assert_eq!(
        store
            .record_signals(
                1005,
                &[Signal {
                    source: "transcript".into(),
                    ..sig.clone()
                }]
            )
            .unwrap(),
        1
    );
    assert_eq!(count(&store, "SELECT paused FROM projects"), 0);
    assert_eq!(store.record_signals(1010, &[sig]).unwrap(), 1);
}

#[test]
fn ignored_observations_never_tick_or_resume_a_project() {
    let mut store = Store::memory().unwrap();
    let p = project(&store, "app");
    let sig = Signal {
        source: "transcript".into(),
        ..agent("/repo/app")
    };
    store
        .record_signals(1000, std::slice::from_ref(&sig))
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO ignored_observations SELECT id,1001 FROM observations",
            [],
        )
        .unwrap();
    store.add_rule(&cwd_rule(p, "/repo/app"), false).unwrap();
    store
        .conn
        .execute("UPDATE projects SET paused=1", [])
        .unwrap();
    assert_eq!(store.record_signals(1005, &[sig]).unwrap(), 0);
    assert_eq!(count(&store, "SELECT paused FROM projects"), 1);
    assert_eq!(count(&store, "SELECT COUNT(*) FROM ticks"), 1);
}

#[test]
fn retroactive_rule_only_fills_matching_unassigned_nonignored_ticks() {
    let mut store = Store::memory().unwrap();
    let p = project(&store, "app");
    let other = project(&store, "other");
    let base = Signal {
        window_title: "Task App".into(),
        space_id: "wN".into(),
        ..agent("C:/Work/App/src")
    };
    let rows = [
        base.clone(),
        Signal {
            window_title: "Task app".into(),
            ..base.clone()
        },
        Signal {
            space_id: "wM".into(),
            ..base.clone()
        },
        Signal {
            cwd: "C:/Work/Apple".into(),
            ..base.clone()
        },
        Signal {
            binary_name: "codex".into(),
            ..base.clone()
        },
        Signal {
            cwd: "c:/WORK/APP/ignored".into(),
            ..base.clone()
        },
        Signal {
            cwd: "c:/WORK/APP/already-assigned".into(),
            ..base.clone()
        },
    ];
    store.record_signals(1000, &rows).unwrap();
    store.conn.execute("INSERT INTO ignored_observations SELECT id,0 FROM observations WHERE cwd LIKE '%ignored'", []).unwrap();
    store.conn.execute("UPDATE ticks SET project_id=? WHERE observation_id IN (SELECT id FROM observations WHERE cwd LIKE '%already-assigned')", [other]).unwrap();
    let rule = Rule {
        match_title_substr: Some("Task App".into()),
        match_binary_name: Some("claude".into()),
        match_space_id: Some("wN".into()),
        ..cwd_rule(p, r"c:\work\app")
    };
    let (id, affected) = store.add_rule(&rule, true).unwrap();
    assert!(id > 0);
    assert_eq!(affected, 1);
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM ticks WHERE project_id=1"),
        1
    );
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM ticks WHERE project_id=2"),
        1
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM ticks WHERE project_id IS NULL"
        ),
        5
    );
    assert_eq!(
        store.rules().unwrap()[0].match_space_id.as_deref(),
        Some("wN")
    );
}

#[test]
fn review_retroactive_sweep_is_scoped_to_observation_space() {
    let mut store = Store::memory().unwrap();
    let p = project(&store, "app");
    store
        .record_signals(
            1000,
            &[
                Signal {
                    space_id: "wN".into(),
                    ..agent("/repo/app")
                },
                Signal {
                    space_id: "wM".into(),
                    ..agent("/repo/app")
                },
            ],
        )
        .unwrap();
    let observation_id = count(&store, "SELECT id FROM observations WHERE space_id='wN'");
    assert_eq!(
        store
            .add_rule_for_observation(&cwd_rule(p, "/repo/app"), observation_id)
            .unwrap()
            .1,
        1
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM ticks WHERE project_id IS NULL"
        ),
        1
    );
    assert_eq!(store.rules().unwrap()[0].match_space_id, None);
}

#[test]
fn review_without_space_preserves_the_unscoped_sweep() {
    let mut store = Store::memory().unwrap();
    let p = project(&store, "app");
    store
        .record_signals(
            1000,
            &[
                agent("/repo/app"),
                Signal {
                    space_id: "wM".into(),
                    ..agent("/repo/app")
                },
            ],
        )
        .unwrap();
    let observation_id = count(&store, "SELECT id FROM observations WHERE space_id=''");
    assert_eq!(
        store
            .add_rule_for_observation(&cwd_rule(p, "/repo/app"), observation_id)
            .unwrap()
            .1,
        2
    );
}

#[test]
fn retroactive_failure_rolls_back_rule_and_assignments() {
    let mut store = Store::memory().unwrap();
    let p = project(&store, "app");
    store.record_signals(1000, &[agent("/repo/app")]).unwrap();
    store.conn.execute_batch("CREATE TRIGGER fail_retag BEFORE UPDATE ON ticks BEGIN SELECT RAISE(ABORT,'simulated retag failure'); END;").unwrap();
    assert!(store.add_rule(&cwd_rule(p, "/repo/app"), true).is_err());
    assert_eq!(count(&store, "SELECT COUNT(*) FROM rules"), 0);
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM ticks WHERE project_id IS NULL"
        ),
        1
    );
}

#[test]
fn invalid_rules_and_sources_leave_no_partial_writes() {
    let mut store = Store::memory().unwrap();
    let p = project(&store, "app");
    assert!(
        store
            .add_rule(&cwd_rule(p, "/wt/*/invalid"), false)
            .is_err()
    );
    assert!(
        store
            .add_rule(
                &Rule {
                    project_id: p,
                    ..Rule::default()
                },
                false
            )
            .is_err()
    );
    assert!(store.add_rule(&cwd_rule(999, "/repo/app"), false).is_err());
    assert!(
        store
            .record_signals(
                1000,
                &[
                    agent("/repo/app"),
                    Signal {
                        source: "invalid".into(),
                        ..Signal::default()
                    }
                ]
            )
            .is_err()
    );
    assert_eq!(count(&store, "SELECT COUNT(*) FROM observations"), 0);
    assert_eq!(count(&store, "SELECT COUNT(*) FROM rules"), 0);
    assert_eq!(count(&store, "SELECT COUNT(*) FROM ticks"), 0);
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO ticks(ts,observation_id) VALUES (?,?)",
                params![1000, 999]
            )
            .is_err()
    );
}
