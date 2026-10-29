use crate::domain::{Rule, Signal, match_rules, matches_rule, validate_cwd_pattern};
use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::{collections::HashSet, path::Path, time::Duration};

const SCHEMA: &str = include_str!("../schema.sql");

pub struct Store {
    pub conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        Self::initialize(conn)
    }

    pub fn memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)
            .context("initialize database schema")?;
        Ok(Self { conn })
    }

    pub fn rules(&self) -> Result<Vec<Rule>> {
        read_rules(&self.conn)
    }

    /// Write a complete polling cycle atomically. Paused focus/CPU activity is
    /// observed for review history but earns no ticks; real transcripts resume.
    /// Totals use DISTINCT timestamps, so parallel sources cannot multiply time.
    pub fn record_signals(&mut self, ts: i64, signals: &[Signal]) -> Result<usize> {
        let ts = ts - ts.rem_euclid(5);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rules = read_rules(&tx)?;
        let mut paused: HashSet<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM projects WHERE paused != 0")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut ticked: HashSet<i64> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT project_id FROM ticks WHERE ts=? AND project_id IS NOT NULL",
            )?;
            stmt.query_map([ts], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut inserted = 0;
        for signal in signals {
            // Never rewrite observation PKs: ticks reference them and doing so
            // turns the conflict path into a growing foreign-key table scan.
            let observation_id: i64 = tx.query_row(
                "INSERT INTO observations(source,bundle_id,window_title,binary_name,cwd,space_id,first_seen)
                 VALUES (?,?,?,?,?,?,?)
                 ON CONFLICT(source,bundle_id,window_title,binary_name,cwd,space_id)
                 DO UPDATE SET first_seen=first_seen RETURNING id",
                params![signal.source,signal.bundle_id,signal.window_title,signal.binary_name,signal.cwd,signal.space_id,ts],
                |row| row.get(0),
            )?;
            let ignored: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ignored_observations WHERE observation_id=?)",
                [observation_id],
                |row| row.get(0),
            )?;
            if ignored {
                continue;
            }

            let project_id = match_rules(signal, &rules);
            if let Some(project_id) = project_id {
                if paused.contains(&project_id) {
                    if signal.source != "transcript" {
                        continue;
                    }
                    tx.execute("UPDATE projects SET paused=0 WHERE id=?", [project_id])?;
                    paused.remove(&project_id);
                }
                if signal.source == "transcript" && ticked.contains(&project_id) {
                    continue;
                }
            }
            inserted += tx.execute(
                "INSERT OR IGNORE INTO ticks(ts,observation_id,project_id) VALUES (?,?,?)",
                params![ts, observation_id, project_id],
            )?;
            if let Some(project_id) = project_id {
                ticked.insert(project_id);
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn add_rule(&mut self, rule: &Rule, retroactive: bool) -> Result<(i64, usize)> {
        validate_rule(rule)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = insert_rule(&tx, rule, retroactive, None)?;
        tx.commit()?;
        Ok(result)
    }

    /// Review rules may be broad for future activity, but their historical
    /// sweep stays within the tagged observation's space when one is present.
    pub fn add_rule_for_observation(
        &mut self,
        rule: &Rule,
        observation_id: i64,
    ) -> Result<(i64, usize)> {
        validate_rule(rule)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let space: String = tx
            .query_row(
                "SELECT space_id FROM observations WHERE id=?",
                [observation_id],
                |row| row.get(0),
            )
            .with_context(|| format!("observation {observation_id}"))?;
        let result = insert_rule(
            &tx,
            rule,
            true,
            if space.is_empty() { None } else { Some(&space) },
        )?;
        tx.commit()?;
        Ok(result)
    }
}

fn read_rules(conn: &Connection) -> Result<Vec<Rule>> {
    let mut stmt = conn.prepare(
        "SELECT id,project_id,priority,match_bundle_id,match_title_substr,match_binary_name,match_cwd_prefix,match_space_id
         FROM rules ORDER BY priority,id",
    )?;
    let rules = stmt
        .query_map([], |row| {
            Ok(Rule {
                id: row.get(0)?,
                project_id: row.get(1)?,
                priority: row.get(2)?,
                match_bundle_id: row.get(3)?,
                match_title_substr: row.get(4)?,
                match_binary_name: row.get(5)?,
                match_cwd_prefix: row.get(6)?,
                match_space_id: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rules)
}

fn validate_rule(rule: &Rule) -> Result<()> {
    anyhow::ensure!(
        rule.match_bundle_id.is_some()
            || rule.match_title_substr.is_some()
            || rule.match_binary_name.is_some()
            || rule.match_cwd_prefix.is_some()
            || rule.match_space_id.is_some(),
        "a rule requires at least one match clause"
    );
    if let Some(pattern) = &rule.match_cwd_prefix {
        validate_cwd_pattern(pattern)?;
    }
    Ok(())
}

fn insert_rule(
    tx: &Transaction<'_>,
    rule: &Rule,
    retroactive: bool,
    space_scope: Option<&str>,
) -> Result<(i64, usize)> {
    tx.execute(
        "INSERT INTO rules(project_id,priority,match_bundle_id,match_title_substr,match_binary_name,match_cwd_prefix,match_space_id,created_at)
         VALUES (?,?,?,?,?,?,?,?)",
        params![rule.project_id,rule.priority,rule.match_bundle_id,rule.match_title_substr,rule.match_binary_name,rule.match_cwd_prefix,rule.match_space_id,chrono::Utc::now().timestamp()],
    )?;
    let id = tx.last_insert_rowid();
    let mut updated = 0;
    if retroactive {
        // Use the live matcher for both paths: SQLite LIKE/GLOB differ on case,
        // wildcard characters and Windows separators, corrupting attribution.
        let matching_observations = {
            let mut stmt = tx.prepare(
                "SELECT o.id,o.source,o.bundle_id,o.window_title,o.binary_name,o.cwd,o.space_id FROM observations o
                 WHERE EXISTS(SELECT 1 FROM ticks t WHERE t.observation_id=o.id AND t.project_id IS NULL)
                 AND NOT EXISTS(SELECT 1 FROM ignored_observations i WHERE i.observation_id=o.id)",
            )?;
            let mut rows = stmt.query([])?;
            let mut matching = Vec::new();
            while let Some(row) = rows.next()? {
                let signal = Signal {
                    source: row.get(1)?,
                    bundle_id: row.get(2)?,
                    window_title: row.get(3)?,
                    binary_name: row.get(4)?,
                    cwd: row.get(5)?,
                    space_id: row.get(6)?,
                };
                if space_scope.is_none_or(|space| signal.space_id == space)
                    && matches_rule(&signal, rule)
                {
                    matching.push(row.get::<_, i64>(0)?);
                }
            }
            matching
        };
        let mut update = tx.prepare(
            "UPDATE ticks SET project_id=? WHERE observation_id=? AND project_id IS NULL",
        )?;
        for observation_id in matching_observations {
            updated += update.execute(params![rule.project_id, observation_id])?;
        }
    }
    Ok((id, updated))
}
