use crate::{
    cli::{Range, bounds},
    config::Config,
    domain::{Signal, match_rules},
    store::Store,
};
use anyhow::{Context, Result};
use chrono::DateTime;
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

pub(crate) fn run(store: &mut Store, config: &Config, range: &Range) -> Result<Value> {
    let (from, to) = if range.from.is_none() && range.to.is_none() {
        (0, chrono::Local::now().timestamp() + 1)
    } else {
        bounds(range)?
    };
    let root = &config.transcript_root;
    if !root.exists() {
        return Ok(json!({"inserted":0}));
    }
    let grace = humantime::parse_duration(&config.transcript_grace)?.as_secs() as i64;
    let rules = store.rules()?;
    let mut spaces = crate::spaces::Resolver::new(crate::spaces::Resolver::default_path());
    let mut inserted = 0;
    for project in std::fs::read_dir(root)? {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(project.path())? {
            let file = file?;
            let path = file.path();
            if !file.file_type()?.is_file() || path.extension().is_none_or(|s| s != "jsonl") {
                continue;
            }
            let sessions = entries(&path, from, to)?;
            let title = path.file_stem().unwrap().to_string_lossy().into_owned();
            let space = spaces.space_for_session(&title);
            for (cwd, mut stamps) in sessions {
                stamps.sort_unstable();
                stamps.dedup();
                let signal = Signal {
                    source: "transcript".into(),
                    cwd,
                    window_title: title.clone(),
                    space_id: space.clone(),
                    ..Signal::default()
                };
                let Some(pid) = match_rules(&signal, &rules) else {
                    continue;
                };
                if let Some(name) = &range.project {
                    let p: String = store.conn.query_row(
                        "SELECT name FROM projects WHERE id=?",
                        [pid],
                        |r| r.get(0),
                    )?;
                    if &p != name {
                        continue;
                    }
                }
                if let Some(company) = &range.company {
                    let p:Option<String>=store.conn.query_row("SELECT c.name FROM projects p LEFT JOIN companies c ON c.id=p.company_id WHERE p.id=?",[pid],|r|r.get(0))?;
                    if p.as_ref() != Some(company) {
                        continue;
                    }
                }
                let tx = store.conn.transaction()?;
                tx.execute("INSERT OR IGNORE INTO observations(source,bundle_id,window_title,binary_name,cwd,space_id,first_seen) VALUES('transcript','',?,'',?,?,?)",params![title,signal.cwd,space,stamps[0]])?;
                let obs:i64=tx.query_row("SELECT id FROM observations WHERE source='transcript' AND bundle_id='' AND window_title=? AND binary_name='' AND cwd=? AND space_id=?",params![title,signal.cwd,space],|r|r.get(0))?;
                let ignored: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM ignored_observations WHERE observation_id=?)",
                    [obs],
                    |r| r.get(0),
                )?;
                if ignored {
                    continue;
                }
                let mut start = stamps[0];
                let mut last = start;
                let mut blocks = Vec::new();
                for ts in stamps.into_iter().skip(1) {
                    if ts - last > grace {
                        blocks.push((start, last));
                        start = ts;
                    }
                    last = ts;
                }
                blocks.push((start, last));
                for (start, end) in blocks {
                    for ts in (start.div_euclid(5) * 5..=end.div_euclid(5) * 5).step_by(5) {
                        if ts < from || ts >= to {
                            continue;
                        }
                        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ticks WHERE project_id=? AND ts BETWEEN ? AND ?)",params![pid,ts-4,ts+4],|r|r.get(0))?;
                        if !exists {
                            inserted+=tx.execute("INSERT OR IGNORE INTO ticks(ts,observation_id,project_id) VALUES(?,?,?)",params![ts,obs,pid])?;
                        }
                    }
                }
                tx.commit()?;
            }
        }
    }
    Ok(json!({"inserted":inserted}))
}

fn entries(path: &Path, from: i64, to: i64) -> Result<Vec<(String, Vec<i64>)>> {
    let mut events = Vec::new();
    let mut cwd = String::new();
    let mut reader =
        BufReader::new(File::open(path).with_context(|| format!("read {}", path.display()))?);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if !line.ends_with('\n') {
            break;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(value) = entry["cwd"].as_str().filter(|s| !s.is_empty()) {
            cwd = value.to_owned();
        }
        let Some(stamp) = entry["timestamp"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp())
        else {
            continue;
        };
        if !cwd.is_empty() && stamp >= from && stamp < to {
            events.push((stamp, cwd.clone()));
        }
    }
    events.sort_by_key(|(stamp, _)| *stamp);
    let mut blocks = Vec::<(String, Vec<i64>)>::new();
    for (stamp, cwd) in events {
        if let Some((previous, stamps)) = blocks.last_mut()
            && *previous == cwd
        {
            stamps.push(stamp);
        } else {
            blocks.push((cwd, vec![stamp]));
        }
    }
    Ok(blocks)
}
