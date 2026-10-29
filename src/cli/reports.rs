use super::*;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    process::Command as Process,
    time::Duration,
};

pub(super) fn report(c: &Connection, range: &Range) -> Result<Value> {
    let (from, to) = bounds(range)?;
    if let Some(p) = &range.project {
        named_id(c, "projects", p)?;
    }
    if let Some(p) = &range.company {
        named_id(c, "companies", p)?;
    }
    let projects = query(
        c,
        "SELECT p.id,p.name,COALESCE(c.name,'') AS company,p.paused,COUNT(DISTINCT t.ts)*5 AS seconds FROM projects p LEFT JOIN companies c ON c.id=p.company_id LEFT JOIN ticks t ON t.project_id=p.id AND t.ts>=?1 AND t.ts<?2 WHERE (?3 IS NULL OR p.name=?3) AND (?4 IS NULL OR c.name=?4) GROUP BY p.id ORDER BY company,p.name",
        params![from, to, range.project, range.company],
    )?;
    let companies = query(
        c,
        "SELECT c.id,COALESCE(c.name,'(no company)') AS name,COUNT(DISTINCT t.ts)*5 AS seconds FROM projects p LEFT JOIN companies c ON c.id=p.company_id LEFT JOIN ticks t ON t.project_id=p.id AND t.ts>=?1 AND t.ts<?2 WHERE (?3 IS NULL OR p.name=?3) AND (?4 IS NULL OR c.name=?4) GROUP BY c.id ORDER BY c.name",
        params![from, to, range.project, range.company],
    )?;
    let unassigned: i64 = c.query_row(
        "SELECT COUNT(DISTINCT ts)*5 FROM ticks WHERE project_id IS NULL AND ts>=? AND ts<?",
        [from, to],
        |r| r.get(0),
    )?;
    let mut v = json!({"from":from,"to_exclusive":to,"projects":projects,"companies":companies,"unassigned_seconds":unassigned});
    for section in ["projects", "companies"] {
        for row in v[section].as_array_mut().unwrap() {
            row["hours"] = json!(row["seconds"].as_i64().unwrap_or(0) as f64 / 3600.0);
        }
    }
    Ok(v)
}

fn snapshot(c: &Connection) -> Result<Value> {
    let mut result = report(c, &Range::default())?;
    let state = query(c, "SELECT key,value FROM runtime_state", [])?;
    let mut worker = BTreeMap::new();
    for item in state.as_array().unwrap() {
        worker.insert(
            item["key"].as_str().unwrap().to_string(),
            item["value"].as_str().unwrap().to_string(),
        );
    }
    let heartbeat = worker
        .get("heartbeat")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let alive =
        worker.get("running").is_some_and(|s| s == "1") && (0..=15).contains(&(now() - heartbeat));
    result["tracking"] = json!(alive);
    result["worker"] = json!(worker);
    result["unbilled"] = query(
        c,
        "SELECT c.id,c.name,c.billing_mode,c.currency,COUNT(DISTINCT t.ts)*5 AS seconds FROM companies c LEFT JOIN projects p ON p.company_id=c.id LEFT JOIN ticks t ON t.project_id=p.id AND t.ts>=COALESCE((SELECT MAX(sent_at) FROM invoices WHERE company_id=c.id AND kind!='advance'),c.created_at) GROUP BY c.id ORDER BY c.name",
        [],
    )?;
    Ok(result)
}

pub(super) fn status(c: &Connection, once: bool, json_output: bool) -> Result<()> {
    if once {
        return emit(&snapshot(c)?, json_output);
    }
    struct Terminal;
    impl Drop for Terminal {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
        }
    }
    terminal::enable_raw_mode()?;
    let _guard = Terminal;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    loop {
        let snap = snapshot(c)?;
        execute!(
            io::stdout(),
            cursor::MoveTo(0, 0),
            terminal::Clear(ClearType::All)
        )?;
        // Raw mode requires CRLF; format into a buffer first.
        let mut bytes = Vec::new();
        human(&mut bytes, &snap, 0)?;
        print!(
            "Rust in Time — {} — Esc / Ctrl-C to exit\r\n{}",
            if snap["tracking"] == true {
                "tracking"
            } else {
                "worker stopped / stale"
            },
            String::from_utf8_lossy(&bytes).replace('\n', "\r\n")
        );
        io::stdout().flush()?;
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < until {
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(k) = event::read()?
                && (k.code == KeyCode::Esc
                    || k.code == KeyCode::Char('q')
                    || (k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL)))
            {
                return Ok(());
            }
        }
    }
}

pub(super) fn summary(c: &Connection, args: &SummaryArgs, json_output: bool) -> Result<()> {
    let mut result = report(c, &args.range)?;
    let (from, to) = bounds(&args.range)?;
    let email = if args.all_authors {
        None
    } else {
        let out = Process::new("git")
            .args(["config", "--get", "user.email"])
            .output()
            .context("git is required for summary")?;
        let email = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        ensure!(
            out.status.success() && !email.is_empty(),
            "set git user.email or use --all-authors explicitly"
        );
        Some(email)
    };
    for p in result["projects"].as_array_mut().unwrap() {
        let id = p["id"].as_i64().unwrap();
        let paths = query(
            c,
            "SELECT match_cwd_prefix AS cwd FROM rules WHERE project_id=? AND match_cwd_prefix IS NOT NULL UNION SELECT DISTINCT o.cwd FROM observations o JOIN ticks t ON t.observation_id=o.id WHERE t.project_id=? AND o.cwd!=''",
            params![id, id],
        )?;
        let mut roots = BTreeSet::new();
        for path in paths.as_array().unwrap() {
            let path = path["cwd"].as_str().unwrap();
            let dirs = if path.contains('*') {
                glob::glob(path)?.filter_map(Result::ok).collect::<Vec<_>>()
            } else {
                vec![PathBuf::from(path)]
            };
            for dir in dirs {
                let root = Process::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["rev-parse", "--show-toplevel"])
                    .output();
                if let Ok(root) = root
                    && root.status.success()
                {
                    roots.insert(String::from_utf8_lossy(&root.stdout).trim().to_string());
                }
            }
        }
        let mut commits = BTreeSet::new();
        for dir in &roots {
            let mut cmd = Process::new("git");
            cmd.arg("-C")
                .arg(dir)
                .args([
                    "log",
                    "--no-merges",
                    "--date=short",
                    "--pretty=format:%ad %h %s",
                ])
                .arg(format!("--since=@{from}"))
                .arg(format!("--until=@{}", to - 1));
            if let Some(email) = &email {
                cmd.arg("--fixed-strings").arg(format!("--author={email}"));
            }
            let out = cmd.output()?;
            ensure!(
                out.status.success(),
                "git log failed in {dir}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            commits.extend(
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            );
        }
        p["directories"] = json!(roots);
        p["commits"] = json!(commits);
    }
    if json_output {
        return emit(&result, true);
    }
    let mut out = io::stdout().lock();
    writeln!(out, "{}Work summary\n", if args.txt { "" } else { "# " })?;
    for p in result["projects"].as_array().unwrap() {
        writeln!(
            out,
            "{}{} / {} — {:.2} hours",
            if args.txt { "" } else { "## " },
            p["company"].as_str().unwrap(),
            p["name"].as_str().unwrap(),
            p["hours"].as_f64().unwrap()
        )?;
        for commit in p["commits"].as_array().unwrap() {
            writeln!(out, "- {}", commit.as_str().unwrap())?;
        }
        writeln!(out)?;
    }
    Ok(())
}
