use super::*;

#[derive(Args, Debug, Default)]
pub(super) struct ReviewArgs {
    #[arg(long)]
    list: bool,
    #[arg(long)]
    id: Option<i64>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    ignore: bool,
    /// Tag only this signature without creating a rule.
    #[arg(long)]
    no_rule: bool,
}

fn pending(c: &Connection) -> Result<Value> {
    query(
        c,
        "SELECT o.*,COUNT(*) AS ticks FROM observations o JOIN ticks t ON t.observation_id=o.id LEFT JOIN ignored_observations i ON i.observation_id=o.id WHERE t.project_id IS NULL AND i.observation_id IS NULL GROUP BY o.id ORDER BY ticks DESC,o.id LIMIT 100",
        [],
    )
}

pub(super) fn run(store: &mut Store, args: &ReviewArgs, json_output: bool) -> Result<()> {
    if args.list || (args.id.is_none() && (json_output || !io::stdin().is_terminal())) {
        return emit(&pending(&store.conn)?, json_output);
    }
    if let Some(id) = args.id {
        return emit(
            &apply(
                store,
                id,
                args.project.as_deref(),
                args.ignore,
                args.no_rule,
            )?,
            json_output,
        );
    }
    loop {
        let items = pending(&store.conn)?;
        if items.as_array().unwrap().is_empty() {
            println!("Nothing to review.");
            return Ok(());
        }
        emit(&items, false)?;
        let choice = prompt("Observation ID (q quits): ")?;
        if choice == "q" || choice.is_empty() {
            return Ok(());
        }
        let Ok(id) = choice.parse::<i64>() else {
            continue;
        };
        let project = prompt("Project name (i ignores, s skips): ")?;
        if project == "s" {
            continue;
        }
        if project != "i" && named_id(&store.conn, "projects", &project).is_err() {
            if prompt("Create this project? [y/N]: ")? != "y" {
                continue;
            }
            super::project(
                &mut store.conn,
                &Project::Add {
                    name: project.clone(),
                    company: None,
                },
            )?;
        }
        let value = if project == "i" {
            apply(store, id, None, true, false)
        } else {
            let obs = query(&store.conn, "SELECT * FROM observations WHERE id=?", [id])?;
            let row = obs
                .as_array()
                .unwrap()
                .first()
                .context("observation not found")?;
            println!(
                "Proposed rule: {}",
                serde_json::to_string_pretty(&propose(
                    row,
                    named_id(&store.conn, "projects", &project)?,
                    &project
                ))?
            );
            let action =
                prompt("Apply rule and retag matching history? [y / n=signature only / s=skip]: ")?;
            if action == "s" {
                continue;
            }
            if !matches!(action.as_str(), "y" | "n") {
                continue;
            }
            apply(store, id, Some(&project), false, action == "n")
        };
        match value {
            Ok(v) => emit(&v, false)?,
            Err(e) => eprintln!("{e:#}"),
        }
    }
}

fn propose(row: &Value, pid: i64, name: &str) -> Rule {
    let get = |field: &str| row[field].as_str().unwrap_or("").to_string();
    let mut rule = Rule {
        project_id: pid,
        priority: 100,
        ..Rule::default()
    };
    if get("source") == "focus" {
        rule.match_bundle_id = Some(get("bundle_id"));
        let title = get("window_title");
        rule.match_title_substr = Some(if title.contains(name) {
            name.to_owned()
        } else {
            title
        });
    } else {
        rule.match_cwd_prefix = Some(crate::domain::propose_cwd(&get("cwd"), name));
        let binary = get("binary_name");
        if !binary.is_empty() {
            rule.match_binary_name = Some(binary);
        }
    }
    let space = get("space_id");
    if !space.is_empty() {
        rule.match_space_id = Some(space);
    }
    rule
}

fn apply(
    store: &mut Store,
    id: i64,
    project: Option<&str>,
    ignore: bool,
    no_rule: bool,
) -> Result<Value> {
    let rows = query(&store.conn, "SELECT * FROM observations WHERE id=?", [id])?;
    let row = rows
        .as_array()
        .unwrap()
        .first()
        .context("observation not found")?;
    if ignore {
        ensure!(
            project.is_none(),
            "--ignore cannot be combined with --project"
        );
        store.conn.execute(
            "INSERT OR IGNORE INTO ignored_observations(observation_id,ignored_at) VALUES(?,?)",
            params![id, now()],
        )?;
        return Ok(json!({"ignored":id}));
    }
    let project = project.context("--project is required to tag an observation")?;
    let pid = named_id(&store.conn, "projects", project)?;
    if no_rule {
        let count = store.conn.execute(
            "UPDATE ticks SET project_id=? WHERE observation_id=? AND project_id IS NULL",
            params![pid, id],
        )?;
        return Ok(json!({"observation":id,"project":project,"retagged":count}));
    }
    let (rule_id, count) = store.add_rule_for_observation(&propose(row, pid, project), id)?;
    Ok(json!({"observation":id,"project":project,"rule_id":rule_id,"retagged":count}))
}
