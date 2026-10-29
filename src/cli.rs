use crate::{
    config::Config,
    domain::Rule,
    invoice::{self, InvoiceArgs},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{Local, NaiveDate, TimeZone};
use clap::{Args, Parser, Subcommand};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

mod menu;
mod reports;
mod review;

#[derive(Parser, Debug)]
#[command(version, about = "Project-aware parallel time tracking for Windows")]
pub struct Cli {
    /// State directory; defaults to %LOCALAPPDATA%/rust-in-time on Windows.
    #[arg(long, global = true, env = "RIT_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Track in the foreground. Use install-startup for background operation.
    Daemon {
        #[arg(long)]
        once: bool,
    },
    /// Today's project totals and worker heartbeat; refreshes in a terminal.
    Status {
        #[arg(long)]
        once: bool,
    },
    Company {
        #[command(subcommand)]
        command: Company,
    },
    Project {
        #[command(subcommand)]
        command: Project,
    },
    Watch {
        #[command(subcommand)]
        command: Watch,
    },
    Rules {
        #[command(subcommand)]
        command: Rules,
    },
    Review(review::ReviewArgs),
    Report(Range),
    Summary(SummaryArgs),
    Invoice {
        #[command(subcommand)]
        command: InvoiceCommand,
    },
    /// Pause every project. Active transcripts can resume projects.
    EndDay,
    StartDay,
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Reset {
        #[arg(value_parser = ["all", "ticks"])]
        scope: String,
        #[arg(long)]
        yes: bool,
    },
    /// Register and start a Windows Task Scheduler task at logon.
    InstallStartup,
    UninstallStartup,
    Restart,
    Stop,
    Spaces,
    Debug {
        #[arg(value_parser = ["frontmost", "windows", "idle"])]
        signal: String,
    },
    /// Recover Claude transcript work between dates without duplicating existing ticks.
    ImportTranscripts(Range),
}

#[derive(Subcommand, Debug)]
enum Company {
    Add {
        name: String,
    },
    List,
    Delete {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Set billing on a company before generating invoices.
    SetBilling {
        name: String,
        #[arg(long, value_parser=["none", "hourly", "monthly_fixed"])]
        mode: String,
        #[arg(long)]
        currency: Option<String>,
        #[arg(long)]
        rate: Option<String>,
        #[arg(long, conflicts_with = "rate")]
        rate_cents: Option<i64>,
        #[arg(long)]
        billed_from: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum Project {
    Add {
        name: String,
        #[arg(long)]
        company: Option<String>,
    },
    List,
    Delete {
        name: String,
    },
    SetCompany {
        project: String,
        company: Option<String>,
    },
    Pause {
        name: String,
    },
    Resume {
        name: String,
    },
    PauseAll,
    ResumeAll,
}

#[derive(Subcommand, Debug)]
enum Watch {
    Add {
        #[arg(value_parser=["app", "bundle", "binary"])]
        kind: String,
        identifier: String,
    },
    List,
    Remove {
        identifier: String,
    },
}

#[derive(Subcommand, Debug)]
enum Rules {
    List,
    Add {
        #[arg(long)]
        project: String,
        #[arg(long, default_value_t = 100)]
        priority: i64,
        #[arg(long, alias = "app")]
        bundle: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        binary: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        space: Option<String>,
        #[arg(long)]
        retroactive: bool,
    },
    Delete {
        id: i64,
    },
}

#[derive(Args, Debug, Default, Clone)]
pub(super) struct Range {
    /// Inclusive local date YYYY-MM-DD (default today).
    #[arg(long)]
    pub(crate) from: Option<String>,
    /// Exclusive local date YYYY-MM-DD (default tomorrow).
    #[arg(long)]
    pub(crate) to: Option<String>,
    #[arg(long)]
    pub(crate) project: Option<String>,
    #[arg(long)]
    pub(crate) company: Option<String>,
}

#[derive(Args, Debug)]
struct SummaryArgs {
    #[command(flatten)]
    range: Range,
    #[arg(long)]
    all_authors: bool,
    #[arg(long)]
    txt: bool,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    Init,
    Show,
    Path,
}

#[derive(Args, Debug, Default)]
struct InvoiceFlags {
    company: String,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    to: Option<String>,
    #[arg(long)]
    issue_date: Option<String>,
    #[arg(long, default_value = "")]
    note: String,
    #[arg(long)]
    discount: Option<String>,
    #[arg(long)]
    no_credit: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    allow_empty: bool,
}

#[derive(Subcommand, Debug)]
enum InvoiceCommand {
    /// Interactively configure a company's billing mode, rate and sender.
    Setup,
    Generate(InvoiceFlags),
    Advance {
        company: String,
        #[arg(long)]
        amount: String,
        #[arg(long)]
        issue_date: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
        #[arg(long)]
        dry_run: bool,
    },
    Balance {
        company: String,
    },
    Send {
        company: String,
        #[arg(long)]
        at: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    List {
        company: Option<String>,
    },
    Delete {
        id: i64,
        #[arg(long)]
        force: bool,
    },
    ShowSenders,
}

pub fn default_data_dir() -> Result<PathBuf> {
    Ok(dirs::data_local_dir()
        .context("cannot locate application data; set --data-dir")?
        .join("rust-in-time"))
}

pub fn run(cli: Cli) -> Result<()> {
    let data_dir = match cli.data_dir {
        Some(ref p) => p.clone(),
        None => default_data_dir()?,
    };
    let data_dir = std::path::absolute(data_dir)?;
    fs::create_dir_all(&data_dir)?;
    let config_path = std::path::absolute(
        cli.config
            .clone()
            .unwrap_or_else(|| data_dir.join("config.yaml")),
    )?;
    if let Some(Command::Config { command }) = &cli.command {
        return config_command(command, &config_path, cli.json);
    }
    let config = Config::load(&config_path)?;
    if let Some(Command::Daemon { once }) = cli.command {
        return crate::tracker::run(&data_dir, &config, once);
    }
    let db_path = config
        .db_path
        .clone()
        .unwrap_or_else(|| data_dir.join("db.sqlite"));
    let mut store = Store::open(&db_path)?;
    let value = match &cli.command {
        None => {
            if !io::stdin().is_terminal() {
                bail!("choose a command; run rit --help");
            }
            return menu::run(&mut store, &config_path, &data_dir);
        }
        Some(Command::Status { once }) => {
            return reports::status(
                &store.conn,
                *once || cli.json || !io::stdout().is_terminal(),
                cli.json,
            );
        }
        Some(Command::Company { command }) => company(&mut store.conn, command)?,
        Some(Command::Project { command }) => project(&mut store.conn, command)?,
        Some(Command::Watch { command }) => watch(&store.conn, command)?,
        Some(Command::Rules { command }) => rules(&mut store, command)?,
        Some(Command::Review(args)) => return review::run(&mut store, args, cli.json),
        Some(Command::Report(range)) => reports::report(&store.conn, range)?,
        Some(Command::Summary(args)) => return reports::summary(&store.conn, args, cli.json),
        Some(Command::Invoice { command }) => {
            invoice_command(&mut store.conn, &config_path, command)?
        }
        Some(Command::EndDay) => pause_all(&store.conn, true)?,
        Some(Command::StartDay) => pause_all(&store.conn, false)?,
        Some(Command::Reset { scope, yes }) => reset(&mut store.conn, scope, *yes)?,
        Some(Command::InstallStartup) => crate::startup::install(&data_dir, &config_path)?,
        Some(Command::UninstallStartup) => crate::startup::uninstall(&data_dir, &store.conn)?,
        Some(Command::Restart) => crate::startup::restart(&data_dir, &config_path, &store.conn)?,
        Some(Command::Stop) => crate::startup::stop(&store.conn)?,
        Some(Command::Spaces) => crate::spaces::list()?,
        Some(Command::Debug { signal }) => crate::platform::debug(signal)?,
        Some(Command::ImportTranscripts(range)) => crate::import::run(&mut store, &config, range)?,
        _ => unreachable!(),
    };
    emit(&value, cli.json)
}

fn invoice_command(c: &mut Connection, path: &Path, command: &InvoiceCommand) -> Result<Value> {
    let mut a = InvoiceArgs::default();
    let op = match command {
        InvoiceCommand::Generate(f) => {
            a.company = Some(f.company.clone());
            a.from = f.from.clone();
            a.to = f.to.clone();
            a.issue_date = f.issue_date.clone();
            a.note = f.note.clone();
            a.discount = f.discount.clone();
            a.no_credit = f.no_credit;
            a.dry_run = f.dry_run;
            a.allow_empty = f.allow_empty;
            "generate"
        }
        InvoiceCommand::Advance {
            company,
            amount,
            issue_date,
            note,
            dry_run,
        } => {
            a.company = Some(company.clone());
            a.amount = Some(amount.clone());
            a.issue_date = issue_date.clone();
            a.note = note.clone();
            a.dry_run = *dry_run;
            "advance"
        }
        InvoiceCommand::Balance { company } => {
            a.company = Some(company.clone());
            "balance"
        }
        InvoiceCommand::Send { company, at, note } => {
            a.company = Some(company.clone());
            a.at = at.clone();
            a.note = note.clone();
            "send"
        }
        InvoiceCommand::List { company } => {
            a.company = company.clone();
            "list"
        }
        InvoiceCommand::Delete { id, force } => {
            a.id = Some(*id);
            a.force = *force;
            "delete"
        }
        InvoiceCommand::ShowSenders => "show-senders",
        InvoiceCommand::Setup => return menu::setup(c),
    };
    invoice::execute(c, path, op, &a)
}

fn company(c: &mut Connection, command: &Company) -> Result<Value> {
    match command {
        Company::List => query(
            c,
            "SELECT id,name,billing_mode,currency,rate_cents,billed_from FROM companies ORDER BY name",
            [],
        ),
        Company::Add { name } => {
            ensure!(!name.trim().is_empty(), "name must not be empty");
            c.execute(
                "INSERT INTO companies(name,created_at) VALUES(?1,?2)",
                params![name, now()],
            )?;
            Ok(json!({"id":c.last_insert_rowid(),"name":name}))
        }
        Company::Delete { name, force } => {
            let id = named_id(c, "companies", name)?;
            let count: i64 = c.query_row(
                "SELECT COUNT(*) FROM invoices WHERE company_id=?",
                [id],
                |r| r.get(0),
            )?;
            ensure!(
                *force || count == 0,
                "company has invoices; --force deletes them and alters advance credit"
            );
            c.execute("DELETE FROM companies WHERE id=?", [id])?;
            Ok(json!({"deleted":name}))
        }
        Company::SetBilling {
            name,
            mode,
            currency,
            rate,
            rate_cents,
            billed_from,
        } => {
            let id = named_id(c, "companies", name)?;
            let cents = rate
                .as_deref()
                .map(parse_money)
                .transpose()?
                .or(*rate_cents);
            ensure!(cents.is_none_or(|v| v >= 0), "rate must not be negative");
            if mode != "none" {
                ensure!(
                    cents.is_some_and(|v| v > 0),
                    "a positive --rate or --rate-cents is required"
                );
                ensure!(currency.as_deref().is_some_and(|v|v.len()==3 && v.bytes().all(|b|b.is_ascii_alphabetic())), "--currency must be a three-letter code");
                ensure!(
                    billed_from.as_deref().is_some_and(|v| !v.is_empty()),
                    "--billed-from must name a sender in config.yaml"
                );
            }
            c.execute("UPDATE companies SET billing_mode=?,currency=?,rate_cents=?,billed_from=? WHERE id=?",params![mode,currency.as_ref().map(|v|v.to_uppercase()),cents,billed_from,id])?;
            Ok(json!({"company":name,"billing_mode":mode,"rate_cents":cents}))
        }
    }
}

fn project(c: &mut Connection, command: &Project) -> Result<Value> {
    match command {
        Project::List => query(c,"SELECT p.id,p.name,COALESCE(c.name,'') AS company,p.paused FROM projects p LEFT JOIN companies c ON c.id=p.company_id ORDER BY p.name",[]).map(|mut v| {
            for p in v.as_array_mut().unwrap() { p["paused"]=json!(p["paused"].as_i64()==Some(1)); } v
        }),
        Project::Add { name, company } => {
            ensure!(!name.trim().is_empty(),"name must not be empty");
            let cid=company.as_ref().map(|v|named_id(c,"companies",v)).transpose()?;
            let tx=c.transaction()?;
            tx.execute("INSERT INTO projects(name,company_id,created_at) VALUES(?,?,?)",params![name,cid,now()])?;
            let id=tx.last_insert_rowid();
            request_arm(&tx,&format!("arm_project:{id}"))?;
            tx.commit()?;
            Ok(json!({"id":id,"name":name}))
        }
        Project::Delete { name } => {
            let id=named_id(c,"projects",name)?;
            let tx=c.transaction()?;
            tx.execute("UPDATE ticks SET project_id=NULL WHERE project_id=?",[id])?;
            tx.execute("DELETE FROM projects WHERE id=?",[id])?; tx.commit()?;
            Ok(json!({"deleted":name}))
        }
        Project::SetCompany { project, company } => {
            let id=named_id(c,"projects",project)?;
            let cid=company.as_ref().map(|v|named_id(c,"companies",v)).transpose()?;
            c.execute("UPDATE projects SET company_id=? WHERE id=?",params![cid,id])?;
            Ok(json!({"project":project,"company":company}))
        }
        Project::Pause { name } | Project::Resume { name } => {
            let id=named_id(c,"projects",name)?;
            let paused=matches!(command,Project::Pause{..});
            c.execute("UPDATE projects SET paused=? WHERE id=?",params![paused,id])?;
            Ok(json!({"project":name,"paused":paused}))
        }
        Project::PauseAll => pause_all(c,true), Project::ResumeAll => pause_all(c,false),
    }
}

fn watch(c: &Connection, command: &Watch) -> Result<Value> {
    match command {
        Watch::List => query(
            c,
            "SELECT id,kind,identifier FROM watched_programs ORDER BY kind,identifier",
            [],
        ),
        Watch::Add { kind, identifier } => {
            let kind = if kind == "binary" { "binary" } else { "bundle" };
            let identifier = identifier.to_lowercase();
            ensure!(
                !identifier.trim().is_empty(),
                "identifier must not be empty"
            );
            c.execute(
                "INSERT INTO watched_programs(kind,identifier,created_at) VALUES(?,?,?)",
                params![kind, identifier, now()],
            )?;
            Ok(json!({"kind":kind,"identifier":identifier}))
        }
        Watch::Remove { identifier } => {
            let count = c.execute(
                "DELETE FROM watched_programs WHERE identifier=? COLLATE NOCASE",
                [identifier],
            )?;
            ensure!(count > 0, "program is not watched");
            Ok(json!({"removed":identifier}))
        }
    }
}

fn rules(store: &mut Store, command: &Rules) -> Result<Value> {
    match command {
        Rules::List => query(
            &store.conn,
            "SELECT r.*,p.name AS project FROM rules r JOIN projects p ON p.id=r.project_id ORDER BY priority,r.id",
            [],
        ),
        Rules::Delete { id } => {
            ensure!(
                store.conn.execute("DELETE FROM rules WHERE id=?", [id])? > 0,
                "rule not found"
            );
            Ok(json!({"deleted":id}))
        }
        Rules::Add {
            project,
            priority,
            bundle,
            title,
            binary,
            cwd,
            space,
            retroactive,
        } => {
            let rule = Rule {
                project_id: named_id(&store.conn, "projects", project)?,
                priority: *priority,
                match_bundle_id: bundle.as_ref().map(|s| s.to_lowercase()),
                match_title_substr: title.clone(),
                match_binary_name: binary.as_ref().map(|s| s.to_lowercase()),
                match_cwd_prefix: cwd.clone(),
                match_space_id: space.clone(),
                ..Rule::default()
            };
            let (id, retagged) = store.add_rule(&rule, *retroactive)?;
            Ok(json!({"id":id,"retagged":retagged}))
        }
    }
}

fn pause_all(c: &Connection, paused: bool) -> Result<Value> {
    let tx = c.unchecked_transaction()?;
    let count = tx.execute("UPDATE projects SET paused=?", [paused])?;
    if !paused && count > 0 {
        request_arm(&tx, "arm_all")?;
    }
    tx.commit()?;
    Ok(json!({"paused":paused,"projects":count}))
}

fn request_arm(c: &Connection, key: &str) -> Result<()> {
    let generation = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    c.execute("INSERT INTO runtime_state(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[key,&generation])?;
    Ok(())
}

fn reset(c: &mut Connection, scope: &str, yes: bool) -> Result<Value> {
    ensure!(yes, "reset requires --yes; this deletes stored {}", scope);
    let tx = c.transaction()?;
    tx.execute_batch(
        "DELETE FROM ticks; DELETE FROM ignored_observations; DELETE FROM observations;",
    )?;
    if scope == "all" {
        tx.execute_batch("DELETE FROM rules; DELETE FROM invoices; DELETE FROM projects; DELETE FROM companies; DELETE FROM watched_programs;")?;
    }
    // Invoice number cursors deliberately survive reset: issued numbers are never reused.
    tx.commit()?;
    Ok(json!({"reset":scope}))
}

fn config_command(command: &ConfigCommand, path: &Path, json_output: bool) -> Result<()> {
    match command {
        ConfigCommand::Path => emit(&json!({"path":path}), json_output),
        ConfigCommand::Show => {
            let cfg = Config::load(path)?;
            if json_output {
                emit(&serde_json::to_value(&cfg)?, true)
            } else {
                if path.exists() {
                    print!("{}", fs::read_to_string(path)?);
                } else {
                    print!("{}", serde_yaml::to_string(&cfg)?);
                }
                Ok(())
            }
        }
        ConfigCommand::Init => {
            use std::fs::OpenOptions;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .context("config exists or cannot be created; existing files are preserved")?;
            writeln!(
                f,
                "# Rust in Time. Five-second grid preserves Antitimely accounting.\n{}",
                serde_yaml::to_string(&Config::default())?
            )?;
            f.write_all(include_bytes!("../config.invoice.example.yaml"))?;
            emit(&json!({"created":path}), json_output)
        }
    }
}

pub(super) fn named_id(c: &Connection, table: &str, name: &str) -> Result<i64> {
    ensure!(matches!(table, "companies" | "projects"), "invalid table");
    c.query_row(
        &format!("SELECT id FROM {table} WHERE name=?"),
        [name],
        |r| r.get(0),
    )
    .optional()?
    .with_context(|| format!("{table}: {name:?} not found"))
}

pub(super) fn query<P: rusqlite::Params>(c: &Connection, sql: &str, params: P) -> Result<Value> {
    let mut stmt = c.prepare(sql)?;
    let names = stmt
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let rows = stmt
        .query_map(params, |r| {
            let mut obj = serde_json::Map::new();
            for (i, k) in names.iter().enumerate() {
                use rusqlite::types::ValueRef;
                let v = match r.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => json!(n),
                    ValueRef::Real(n) => json!(n),
                    ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
                    ValueRef::Blob(_) => json!("(binary)"),
                };
                obj.insert(k.clone(), v);
            }
            Ok(Value::Object(obj))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!(rows))
}

pub(super) fn now() -> i64 {
    Local::now().timestamp()
}

pub(super) fn date(value: &str) -> Result<i64> {
    let d = NaiveDate::parse_from_str(value, "%Y-%m-%d").context("expected date YYYY-MM-DD")?;
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map(|v| v.timestamp())
        .context("date does not exist in local timezone")
}

pub(super) fn bounds(range: &Range) -> Result<(i64, i64)> {
    let today = Local::now().date_naive();
    let from = range
        .from
        .as_deref()
        .map(date)
        .transpose()?
        .unwrap_or(date(&today.to_string())?);
    let to = range
        .to
        .as_deref()
        .map(date)
        .transpose()?
        .unwrap_or(date(&today.succ_opt().unwrap().to_string())?);
    ensure!(to > from, "--to is exclusive and must be later than --from");
    Ok((from, to))
}

fn parse_money(s: &str) -> Result<i64> {
    ensure!(
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'.'),
        "amount must be a positive decimal"
    );
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    ensure!(
        !whole.is_empty() && frac.len() <= 2 && !frac.contains('.'),
        "amount must have at most two decimals"
    );
    let whole: i64 = whole.parse()?;
    let frac: i64 = if frac.is_empty() {
        0
    } else {
        frac.parse::<i64>()? * if frac.len() == 1 { 10 } else { 1 }
    };
    whole
        .checked_mul(100)
        .and_then(|v| v.checked_add(frac))
        .context("amount is too large")
}

pub(super) fn emit(v: &Value, json_output: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if json_output {
        writeln!(out, "{}", serde_json::to_string(v)?)?;
    } else {
        human(&mut out, v, 0)?;
    }
    Ok(())
}

fn human(out: &mut impl Write, v: &Value, indent: usize) -> io::Result<()> {
    let pad = " ".repeat(indent);
    match v {
        Value::Array(a) if a.is_empty() => writeln!(out, "{pad}(none)"),
        Value::Array(a) => {
            for item in a {
                human(out, item, indent)?;
            }
            Ok(())
        }
        Value::Object(m) => {
            let fields = m
                .iter()
                .filter(|(_, v)| !v.is_array() && !v.is_object())
                .map(|(k, v)| {
                    format!(
                        "{k}: {}",
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string())
                    )
                })
                .collect::<Vec<_>>();
            if !fields.is_empty() {
                writeln!(out, "{pad}{}", fields.join("  |  "))?;
            }
            for (k, v) in m.iter().filter(|(_, v)| v.is_array() || v.is_object()) {
                writeln!(out, "{pad}{k}:")?;
                human(out, v, indent + 2)?;
            }
            Ok(())
        }
        _ => writeln!(out, "{pad}{v}"),
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut s = String::new();
    if io::stdin().read_line(&mut s)? == 0 {
        return Ok("q".into());
    }
    Ok(s.trim().to_string())
}
