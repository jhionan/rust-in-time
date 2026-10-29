//! Invoice accounting and standalone PDF generation. No external transmissions occur.
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Days, Local, NaiveDate, NaiveDateTime, TimeZone};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "invoice/pdf.rs"]
mod pdf;

#[derive(Debug, Clone, Default)]
pub struct InvoiceArgs {
    pub company: Option<String>,
    pub id: Option<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub issue_date: Option<String>,
    pub at: Option<String>,
    pub note: String,
    pub amount: Option<String>,
    pub discount: Option<String>,
    pub no_credit: bool,
    pub dry_run: bool,
    pub allow_empty: bool,
    pub force: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Sender {
    legal_name: String,
    tax_id: String,
    tax_id_label: String,
    address_lines: Vec<String>,
    logo_path: String,
    invoice: InvoiceSeed,
    bank_accounts: BTreeMap<String, Bank>,
    output_dir: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct InvoiceSeed {
    number_prefix: String,
    number_pad: usize,
    next_number: i64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Bank {
    title: String,
    subtitle: String,
    also_accepts: Vec<String>,
    fields: Vec<BankField>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct BankField {
    label: String,
    value: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Client {
    legal_name: String,
    tax_id: String,
    tax_id_label: String,
    email: String,
    address_lines: Vec<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct InvoiceConfig {
    output_dir: String,
    line_item_label: String,
    due_days: u64,
}
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct SendersConfig {
    senders: BTreeMap<String, Sender>,
    clients: BTreeMap<String, Client>,
    invoice: InvoiceConfig,
}
impl SendersConfig {
    fn load(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("read invoice config {}", path.display()))?;
        let config: Self = serde_yaml::from_slice(&bytes).context("parse invoice config")?;
        ensure!(
            !config.invoice.output_dir.is_empty(),
            "invoice.output_dir is empty"
        );
        ensure!(
            !config.invoice.line_item_label.is_empty(),
            "invoice.line_item_label is empty"
        );
        for (key, s) in &config.senders {
            ensure!(
                !s.legal_name.is_empty(),
                "senders.{key}.legal_name is empty"
            );
            ensure!(!s.tax_id.is_empty(), "senders.{key}.tax_id is empty");
            ensure!(
                !s.address_lines.is_empty(),
                "senders.{key}.address_lines is empty"
            );
            ensure!(
                !s.invoice.number_prefix.is_empty(),
                "senders.{key}.invoice.number_prefix is empty"
            );
            ensure!(
                s.invoice.next_number >= 1,
                "senders.{key}.invoice.next_number must be >= 1"
            );
            ensure!(
                s.invoice.number_pad <= 100,
                "senders.{key}.invoice.number_pad exceeds 100"
            );
            ensure!(
                !s.bank_accounts.is_empty(),
                "senders.{key}.bank_accounts has no entries"
            );
            for (ccy, b) in &s.bank_accounts {
                ensure!(
                    !b.fields.is_empty(),
                    "senders.{key}.bank_accounts.{ccy}.fields is empty"
                );
            }
        }
        Ok(config)
    }
}
impl Sender {
    fn bank_for(&self, currency: &str) -> Result<Bank> {
        self.bank_accounts.get(currency).or_else(||self.bank_accounts.values().find(|b|b.also_accepts.iter().any(|c|c==currency)))
            .cloned().with_context(||format!("sender has no bank account for currency {currency:?} (and no also_accepts fallback)"))
    }
}

#[derive(Debug)]
struct Company {
    id: i64,
    name: String,
    created_at: i64,
    mode: String,
    currency: String,
    rate: i64,
    sender: String,
}
fn company(conn: &Connection, name: Option<&str>) -> Result<Company> {
    let name = name
        .filter(|s| !s.is_empty())
        .context("company is required")?;
    conn.query_row("SELECT id,name,created_at,billing_mode,COALESCE(currency,''),COALESCE(rate_cents,0),COALESCE(billed_from,'') FROM companies WHERE name=?1",[name],|r|Ok(Company{id:r.get(0)?,name:r.get(1)?,created_at:r.get(2)?,mode:r.get(3)?,currency:r.get(4)?,rate:r.get(5)?,sender:r.get(6)?}))
        .with_context(||format!("company {name:?} not found"))
}

/// All invoice writes and advance drawdowns use an immediate SQLite transaction.
/// Number allocation rolls back if validation, rendering, or insertion fails.
pub fn execute(
    conn: &mut Connection,
    config_path: &Path,
    command: &str,
    args: &InvoiceArgs,
) -> Result<Value> {
    match command {
        "generate" | "advance" => generate(conn, config_path, command == "advance", args),
        "show-senders" => Ok(serde_json::to_value(SendersConfig::load(config_path)?)?),
        "list" => list(conn, args.company.as_deref()),
        "balance" => {
            let co = company(conn, args.company.as_deref())?;
            let (remaining, rows) = credit(conn, &co)?;
            Ok(
                json!({"company":co.name,"currency":co.currency,"remaining_cents":remaining,"rate_cents":co.rate,"rows":rows}),
            )
        }
        "send" => {
            let co = company(conn, args.company.as_deref())?;
            let sent = parse_date(args.at.as_deref())?.unwrap_or_else(Local::now);
            if args.dry_run {
                return Ok(
                    json!({"id":0,"company":co.name,"sent_at":sent.timestamp(),"dry_run":true}),
                );
            }
            conn.execute(
                "INSERT INTO invoices(company_id,sent_at,note,created_at) VALUES(?1,?2,?3,?4)",
                params![co.id, sent.timestamp(), args.note, Local::now().timestamp()],
            )?;
            Ok(
                json!({"id":conn.last_insert_rowid(),"company":co.name,"sent_at":sent.timestamp(),"note":args.note}),
            )
        }
        "delete" => {
            let id = args.id.context("invoice id is required")?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let inv:Option<(String,i64,String,String)>=tx.query_row("SELECT kind,credit_applied_cents,COALESCE(number,''),COALESCE(pdf_path,'') FROM invoices WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            let (kind, applied, number, path) =
                inv.with_context(|| format!("invoice {id} not found"))?;
            ensure!(
                args.force || (kind != "advance" && applied == 0),
                "invoice {number} carries advance credit (kind={kind}, applied={applied} cents); deleting it changes the credit balance while the client still holds the PDF at {path}; pass --force if intended"
            );
            if !args.dry_run {
                tx.execute("DELETE FROM invoices WHERE id=?1", [id])?;
            }
            tx.commit()?;
            Ok(json!({"id":id,"deleted":!args.dry_run,"dry_run":args.dry_run}))
        }
        _ => bail!("unknown invoice command {command:?}"),
    }
}

fn list(conn: &Connection, name: Option<&str>) -> Result<Value> {
    let company_id = match name {
        Some(n) => Some(company(conn, Some(n))?.id),
        None => None,
    };
    let mut stmt=conn.prepare("SELECT i.id,c.name,i.sent_at,i.note,i.number,i.kind,i.total_cents,i.credit_applied_cents,i.currency,i.pdf_path,i.discount_cents,i.sender_key FROM invoices i JOIN companies c ON c.id=i.company_id WHERE (?1 IS NULL OR i.company_id=?1) ORDER BY i.sent_at DESC,i.id DESC")?;
    let rows=stmt.query_map([company_id],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"company_name":r.get::<_,String>(1)?,"sent_at":r.get::<_,i64>(2)?,"note":r.get::<_,String>(3)?,"number":r.get::<_,Option<String>>(4)?,"kind":r.get::<_,String>(5)?,"total_cents":r.get::<_,Option<i64>>(6)?,"credit_applied_cents":r.get::<_,i64>(7)?,"currency":r.get::<_,Option<String>>(8)?,"pdf_path":r.get::<_,Option<String>>(9)?,"discount_cents":r.get::<_,i64>(10)?,"sender_key":r.get::<_,Option<String>>(11)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"items":rows}))
}
#[derive(Debug, Serialize)]
struct CreditRow {
    number: Option<String>,
    kind: String,
    total_cents: i64,
    credit_applied_cents: i64,
}
fn credit(conn: &Connection, co: &Company) -> Result<(i64, Vec<CreditRow>)> {
    let remaining=conn.query_row("SELECT COALESCE(SUM(CASE WHEN kind='advance' THEN total_cents ELSE 0 END),0)-COALESCE(SUM(credit_applied_cents),0) FROM invoices WHERE company_id=?1 AND currency=?2",params![co.id,co.currency],|r|r.get(0))?;
    let mut stmt=conn.prepare("SELECT number,kind,COALESCE(total_cents,0),credit_applied_cents FROM invoices WHERE company_id=?1 AND currency=?2 AND (kind='advance' OR credit_applied_cents>0) ORDER BY sent_at DESC,id DESC")?;
    let rows = stmt
        .query_map(params![co.id, co.currency], |r| {
            Ok(CreditRow {
                number: r.get(0)?,
                kind: r.get(1)?,
                total_cents: r.get(2)?,
                credit_applied_cents: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((remaining, rows))
}
fn oldest_advance(rows: &[CreditRow]) -> String {
    let drawn: i128 = rows.iter().map(|r| r.credit_applied_cents as i128).sum();
    let mut advanced = 0_i128;
    for r in rows.iter().rev().filter(|r| r.kind == "advance") {
        advanced += r.total_cents as i128;
        if let (true, Some(number)) = (advanced > drawn, &r.number) {
            return number.clone();
        }
    }
    String::new()
}
fn parse_date(value: Option<&str>) -> Result<Option<DateTime<Local>>> {
    match value {
        None | Some("") => Ok(None),
        Some(s) => {
            for format in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S"] {
                if let Ok(d) = NaiveDateTime::parse_from_str(s, format) {
                    return Local
                        .from_local_datetime(&d)
                        .single()
                        .context("date is ambiguous or absent in local timezone")
                        .map(Some);
                }
            }
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                return Local
                    .from_local_datetime(&d.and_hms_opt(0, 0, 0).context("invalid date")?)
                    .single()
                    .context("date is ambiguous or absent in local timezone")
                    .map(Some);
            }
            let d = DateTime::parse_from_rfc3339(s)
                .with_context(|| format!("invalid date {s:?}: expected YYYY-MM-DD or RFC3339"))?;
            Ok(Some(d.with_timezone(&Local)))
        }
    }
}
fn timestamp(ts: i64) -> Result<DateTime<Local>> {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .context("date outside supported range")
}
fn month_start(now: DateTime<Local>) -> Result<DateTime<Local>> {
    let d = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).context("invalid month")?;
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).context("invalid month")?)
        .single()
        .context("month boundary absent in local timezone")
}
fn next_month(now: DateTime<Local>) -> Result<DateTime<Local>> {
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    let d = NaiveDate::from_ymd_opt(year, month, 1).context("invalid next month")?;
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).context("invalid month")?)
        .single()
        .context("month boundary absent in local timezone")
}
fn parse_money(value: Option<&str>) -> Result<i64> {
    let s = value.unwrap_or("").trim();
    if s.is_empty() {
        return Ok(0);
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    let mut parts = s.split('.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next().unwrap_or("");
    ensure!(
        parts.next().is_none()
            && fraction.len() <= 2
            && (!whole.is_empty() || !fraction.is_empty())
            && whole
                .bytes()
                .chain(fraction.bytes())
                .all(|c| c.is_ascii_digit()),
        "invalid amount {s:?}; use a nonnegative decimal with at most two places"
    );
    let major = if whole.is_empty() {
        0
    } else {
        whole.parse::<i64>().context("amount is too large")?
    };
    let minor = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<i64>()? * 10,
        _ => fraction.parse::<i64>()?,
    };
    major
        .checked_mul(100)
        .and_then(|v| v.checked_add(minor))
        .context("amount is too large")
}
fn round_even(n: i128, d: i128) -> Result<i64> {
    ensure!(n >= 0 && d > 0, "invalid negative billing quantity");
    let q = n / d;
    let remainder = n % d;
    let rounded = q + i128::from(remainder * 2 > d || (remainder * 2 == d && q % 2 != 0));
    i64::try_from(rounded).context("invoice amount exceeds supported range")
}
#[derive(Debug)]
struct Document {
    number: String,
    issue: DateTime<Local>,
    due: DateTime<Local>,
    from: DateTime<Local>,
    to: DateTime<Local>,
    company: String,
    currency: String,
    client: Client,
    sender: Sender,
    bank: Bank,
    label: String,
    quantity: i64,
    rate: i64,
    subtotal: i64,
    discount: i64,
    applied: i64,
    credit_ref: String,
    advance: bool,
}
impl Document {
    fn total(&self) -> i64 {
        self.subtotal - self.discount - self.applied
    }
}

fn generate(
    conn: &mut Connection,
    config_path: &Path,
    advance: bool,
    args: &InvoiceArgs,
) -> Result<Value> {
    let config = SendersConfig::load(config_path)?;
    let now = parse_date(args.issue_date.as_deref())?.unwrap_or_else(Local::now);
    let discount = if advance {
        0
    } else {
        parse_money(args.discount.as_deref())?
    };
    let amount = if advance {
        parse_money(args.amount.as_deref())?
    } else {
        0
    };
    if advance {
        ensure!(amount > 0, "advance amount must be positive");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let co = company(&tx, args.company.as_deref())?;
    ensure!(
        co.mode == "hourly" || co.mode == "monthly_fixed",
        "company {:?} is not billable (billing_mode={:?})",
        co.name,
        co.mode
    );
    ensure!(
        !co.sender.is_empty(),
        "company {:?} has no billed_from sender",
        co.name
    );
    ensure!(co.rate >= 0, "company rate must not be negative");
    if advance {
        ensure!(
            co.rate > 0,
            "company {:?} has no rate; cannot express an advance in hours",
            co.name
        );
    }
    let sender = config
        .senders
        .get(&co.sender)
        .cloned()
        .with_context(|| format!("sender {:?} not in config", co.sender))?;
    let bank = sender.bank_for(&co.currency)?;
    let (from, to) = if advance {
        (now, next_month(now)?)
    } else {
        let defaults = if co.mode == "monthly_fixed" {
            (month_start(now)?, next_month(now)?)
        } else {
            let last: Option<i64> = tx.query_row(
                "SELECT MAX(sent_at) FROM invoices WHERE company_id=?1 AND kind<>'advance'",
                [co.id],
                |r| r.get(0),
            )?;
            (
                timestamp(last.filter(|v| *v > 0).unwrap_or(co.created_at))?,
                now,
            )
        };
        (
            parse_date(args.from.as_deref())?.unwrap_or(defaults.0),
            parse_date(args.to.as_deref())?.unwrap_or(defaults.1),
        )
    };
    ensure!(to > from, "invoice period end must be after its start");
    let ticks: i64 = if !advance && co.mode == "hourly" {
        tx.query_row("SELECT COUNT(DISTINCT t.ts) FROM ticks t JOIN projects p ON p.id=t.project_id WHERE p.company_id=?1 AND p.paused=0 AND t.ts>=?2 AND t.ts<?3",params![co.id,from.timestamp(),to.timestamp()],|r|r.get(0))?
    } else {
        0
    };
    if !advance && co.mode == "hourly" {
        ensure!(
            ticks > 0 || args.allow_empty,
            "no time tracked for {:?} in {}..{}; pass --allow-empty to override",
            co.name,
            from.format("%Y-%m-%d"),
            to.format("%Y-%m-%d")
        );
    }
    let quantity = if advance {
        i64::try_from(amount as i128 * 100 / co.rate as i128)
            .context("advance quantity too large")?
    } else if co.mode == "monthly_fixed" {
        100
    } else {
        round_even(ticks as i128 * 5 * 100, 3600)?
    };
    let subtotal = if advance {
        amount
    } else if co.mode == "monthly_fixed" {
        co.rate
    } else {
        round_even(quantity as i128 * co.rate as i128, 100)?
    };
    ensure!(
        discount <= subtotal,
        "discount {discount} exceeds line-item total {subtotal} cents"
    );
    let (before, rows) = credit(&tx, &co)?;
    let applied = if advance || args.no_credit {
        0
    } else {
        before.max(0).min(subtotal - discount)
    };
    let remaining = if advance {
        before
            .checked_add(amount)
            .context("credit balance overflow")?
    } else {
        before
            .checked_sub(applied)
            .context("credit balance overflow")?
    };
    let credit_ref = if applied > 0 {
        oldest_advance(&rows)
    } else {
        String::new()
    };
    let configured = sender.invoice.next_number;
    let allocated = if args.dry_run {
        tx.query_row(
            "SELECT next_invoice_number FROM sender_state WHERE sender_key=?1",
            [&co.sender],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(configured)
    } else {
        tx.execute(
            "INSERT OR IGNORE INTO sender_state(sender_key,next_invoice_number) VALUES(?1,?2)",
            params![co.sender, configured],
        )?;
        tx.query_row("UPDATE sender_state SET next_invoice_number=next_invoice_number+1 WHERE sender_key=?1 RETURNING next_invoice_number-1",[&co.sender],|r|r.get(0))?
    };
    ensure!(allocated > 0, "sender next invoice number must be positive");
    let number = format!(
        "{}{:0width$}",
        sender.invoice.number_prefix,
        allocated,
        width = sender.invoice.number_pad
    );
    ensure!(
        !number
            .chars()
            .any(|c| c == '/' || c == '\\' || c == ':' || c.is_control())
            && number != "."
            && number != "..",
        "invoice number contains an invalid filename character"
    );
    let due = now
        .checked_add_days(Days::new(config.invoice.due_days))
        .context("due date outside supported range")?;
    let doc = Document {
        number: number.clone(),
        issue: now,
        due,
        from,
        to,
        company: co.name.clone(),
        currency: co.currency.clone(),
        client: config.clients.get(&co.name).cloned().unwrap_or_default(),
        sender: sender.clone(),
        bank,
        label: config.invoice.line_item_label,
        quantity,
        rate: co.rate,
        subtotal,
        discount,
        applied,
        credit_ref,
        advance,
    };
    let bytes = pdf::render(&doc)?;
    let (path, mut staged) = if args.dry_run {
        let staged = StagedPdf::create(&std::env::temp_dir(), "rit-invoice-preview", &bytes)?;
        (staged.path.clone(), staged)
    } else {
        let dir = if sender.output_dir.is_empty() {
            expand_home(&config.invoice.output_dir)?.join(&co.sender)
        } else {
            expand_home(&sender.output_dir)?
        };
        fs::create_dir_all(&dir)
            .with_context(|| format!("create invoice output directory {}", dir.display()))?;
        let path = dir.join(format!("{number}.pdf"));
        ensure!(
            !path.exists(),
            "invoice PDF {} already exists; refusing to overwrite",
            path.display()
        );
        let staged = StagedPdf::create(&dir, &format!(".{number}"), &bytes)?;
        (path, staged)
    };
    let id = if args.dry_run {
        0
    } else {
        tx.execute("INSERT INTO invoices(company_id,sent_at,note,created_at,number,pdf_path,total_cents,currency,sender_key,kind,credit_applied_cents,discount_cents) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![co.id,now.timestamp(),args.note,Local::now().timestamp(),number,path.to_string_lossy(),doc.total(),co.currency,co.sender,if advance {"advance"} else {"hourly"},applied,discount])?;
        tx.last_insert_rowid()
    };
    tx.commit().context("commit invoice")?;
    // The row is durable before the numbered client-facing file is published.
    // Preserve the staged file on rename failure: retrying would bill twice.
    staged.keep = true;
    if !args.dry_run {
        fs::rename(&staged.path,&path).with_context(||format!("invoice {number} committed but PDF could not be moved from {} to {}; move the file manually rather than retrying",staged.path.display(),path.display()))?;
    }
    Ok(
        json!({"invoice_id":id,"id":id,"number":number,"pdf_path":path,"total_cents":doc.total(),"currency":doc.currency,"sender_key":co.sender,"company":co.name,"issue_date_unix":now.timestamp(),"due_date_unix":due.timestamp(),"from_unix":from.timestamp(),"to_unix":to.timestamp(),"from":from.format("%Y-%m-%d").to_string(),"to":to.format("%Y-%m-%d").to_string(),"ticks":ticks,"billing_mode":co.mode,"kind":if advance {"advance"} else {"hourly"},"quantity_hours_times100":quantity,"rate_cents":co.rate,"subtotal_cents":subtotal,"discount_cents":discount,"credit_applied_cents":applied,"credit_applied_ref":doc.credit_ref,"credit_remaining_cents":remaining,"bank":doc.bank,"client":doc.client,"dry_run":args.dry_run}),
    )
}
fn expand_home(value: &str) -> Result<PathBuf> {
    if value == "~" {
        return dirs::home_dir().context("home directory unavailable");
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return Ok(dirs::home_dir()
            .context("home directory unavailable")?
            .join(rest));
    }
    Ok(PathBuf::from(value))
}
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
struct StagedPdf {
    path: PathBuf,
    keep: bool,
}
impl StagedPdf {
    fn create(dir: &Path, prefix: &str, bytes: &[u8]) -> Result<Self> {
        for _ in 0..100 {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let path = dir.join(format!("{prefix}-{}-{stamp}-{n}.pdf", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file: File = match options.open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("create staged PDF {}", path.display()));
                }
            };
            let staged = Self { path, keep: false };
            file.write_all(bytes).context("write invoice PDF")?;
            file.sync_all().context("flush invoice PDF")?;
            return Ok(staged);
        }
        bail!("could not allocate temporary invoice filename")
    }
}
impl Drop for StagedPdf {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}
