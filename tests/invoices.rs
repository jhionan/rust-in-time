use rusqlite::{Connection, params};
use rust_in_time::invoice::{InvoiceArgs, execute};
use serde_json::Value;
use std::path::PathBuf;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    conn: Connection,
    config: PathBuf,
}

impl Fixture {
    fn new(mode: &str, rate: i64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE companies (id INTEGER PRIMARY KEY,name TEXT UNIQUE,created_at INTEGER,billing_mode TEXT,currency TEXT,rate_cents INTEGER,billed_from TEXT);
             CREATE TABLE projects (id INTEGER PRIMARY KEY,name TEXT,company_id INTEGER,paused INTEGER DEFAULT 0,created_at INTEGER);
             CREATE TABLE ticks (ts INTEGER,observation_id INTEGER,project_id INTEGER,PRIMARY KEY(ts,observation_id));
             CREATE TABLE invoices (id INTEGER PRIMARY KEY,company_id INTEGER,sent_at INTEGER,note TEXT DEFAULT '',created_at INTEGER,number TEXT,pdf_path TEXT,total_cents INTEGER,currency TEXT,sender_key TEXT,kind TEXT DEFAULT 'hourly',credit_applied_cents INTEGER DEFAULT 0,discount_cents INTEGER DEFAULT 0);
             CREATE TABLE sender_state (sender_key TEXT PRIMARY KEY,next_invoice_number INTEGER);"
        ).unwrap();
        conn.execute(
            "INSERT INTO companies VALUES(1,'Acme',1704067200,?1,'EUR',?2,'primary')",
            params![mode, rate],
        )
        .unwrap();
        conn.execute_batch("INSERT INTO projects VALUES(1,'a',1,0,1704067200),(2,'b',1,0,1704067200),(3,'paused',1,1,1704067200);").unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(
            &config,
            format!(
                r#"
interval: 5s
senders:
  primary:
    legal_name: "José Sender"
    tax_id: "TAX-42"
    tax_id_label: "VAT"
    address_lines: ["Calle Mayor 1", "Madrid"]
    invoice: {{ number_prefix: "INV-", number_pad: 3, next_number: 41 }}
    bank_accounts:
      USD:
        title: "International transfer"
        subtitle: "Use the invoice reference"
        also_accepts: [EUR]
        fields:
          - {{ label: "IBAN", value: "ES00 0000 0000 0000" }}
clients:
  Acme:
    legal_name: "Société Acme"
    tax_id: "CLIENT-21"
    tax_id_label: "VAT"
    email: "billing@example.invalid"
    address_lines: ["Long client street 45", "Paris"]
invoice:
  output_dir: '{}'
  line_item_label: "Software development"
  due_days: 14
"#,
                dir.path().join("pdfs").display()
            ),
        )
        .unwrap();
        Self { dir, conn, config }
    }
    fn args(&self) -> InvoiceArgs {
        InvoiceArgs {
            company: Some("Acme".into()),
            issue_date: Some("2024-01-03".into()),
            ..Default::default()
        }
    }
    fn ticks(&self, count: i64) {
        for n in 0..count {
            self.conn
                .execute(
                    "INSERT INTO ticks VALUES(?1,1,1),(?1,2,2)",
                    [1704153600 + n * 5],
                )
                .unwrap();
        }
        self.conn
            .execute("INSERT INTO ticks VALUES(1704150000,3,3)", [])
            .unwrap();
    }
    fn run(&mut self, command: &str, args: &InvoiceArgs) -> Value {
        execute(&mut self.conn, &self.config, command, args).unwrap()
    }
    fn count(&self, table: &str) -> i64 {
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
}

#[test]
fn hourly_company_deduplicates_projects_and_uses_displayed_quantity() {
    let mut f = Fixture::new("hourly", 12345);
    f.ticks(720);
    let result = f.run("generate", &f.args());
    assert_eq!(result["ticks"], 720);
    assert_eq!(result["quantity_hours_times100"], 100);
    assert_eq!(result["total_cents"], 12345);
    assert_eq!(result["number"], "INV-041");
    assert_eq!(f.count("invoices"), 1);
    let bytes = std::fs::read(result["pdf_path"].as_str().unwrap()).unwrap();
    assert!(bytes.starts_with(b"%PDF-"));
    assert!(bytes.trim_ascii_end().ends_with(b"%%EOF"));
    if let Some(path) = std::env::var_os("RIT_INVOICE_FIXTURE_PDF") {
        std::fs::write(path, &bytes).unwrap();
    }
}

#[test]
fn advance_preserves_anchor_and_goodwill_precedes_credit() {
    let mut f = Fixture::new("hourly", 12345);
    f.ticks(720);
    let mut args = f.args();
    args.at = Some("2024-01-01".into());
    f.run("send", &args);
    args.issue_date = Some("2024-01-03".into());
    args.amount = Some("50".into());
    let advance = f.run("advance", &args);
    assert_eq!(advance["credit_remaining_cents"], 5000);
    args.issue_date = Some("2024-01-04".into());
    args.discount = Some("10.00".into());
    let invoice = f.run("generate", &args);
    assert_eq!(invoice["from"], "2024-01-01");
    assert_eq!(invoice["ticks"], 720);
    assert_eq!(invoice["credit_applied_cents"], 5000);
    assert_eq!(invoice["total_cents"], 6345);
    assert_eq!(invoice["credit_remaining_cents"], 0);
    assert_eq!(invoice["credit_applied_ref"], "INV-041");
}

#[test]
fn dry_run_creates_preview_without_allocating_or_drawing_credit() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    let mut args = f.args();
    args.dry_run = true;
    let preview = f.run("generate", &args);
    assert_eq!(preview["number"], "INV-041");
    assert_eq!(f.count("invoices"), 0);
    assert_eq!(f.count("sender_state"), 0);
    assert!(!f.dir.path().join("pdfs").exists());
    assert!(std::path::Path::new(preview["pdf_path"].as_str().unwrap()).exists());
    std::fs::remove_file(preview["pdf_path"].as_str().unwrap()).unwrap();
    args.dry_run = false;
    assert_eq!(f.run("generate", &args)["number"], "INV-041");
}

#[test]
fn failed_render_rolls_back_number_and_invoice() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    std::fs::write(f.dir.path().join("pdfs"), "file blocks directory").unwrap();
    let args = f.args();
    assert!(execute(&mut f.conn, &f.config, "generate", &args).is_err());
    assert_eq!(f.count("invoices"), 0);
    assert_eq!(f.count("sender_state"), 0);
    std::fs::remove_file(f.dir.path().join("pdfs")).unwrap();
    assert_eq!(f.run("generate", &f.args())["number"], "INV-041");
}

#[test]
fn credit_bearing_delete_requires_force_and_retains_pdf() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    let mut args = f.args();
    args.amount = Some("25".into());
    let advance = f.run("advance", &args);
    args.id = advance["invoice_id"].as_i64();
    assert!(execute(&mut f.conn, &f.config, "delete", &args).is_err());
    assert_eq!(f.count("invoices"), 1);
    args.force = true;
    f.run("delete", &args);
    assert_eq!(f.count("invoices"), 0);
    assert!(std::path::Path::new(advance["pdf_path"].as_str().unwrap()).exists());
}

#[test]
fn bank_fallback_client_details_and_fixed_calendar_period_are_resolved() {
    let mut f = Fixture::new("monthly_fixed", 9999);
    let result = f.run("generate", &f.args());
    assert_eq!(result["total_cents"], 9999);
    assert_eq!(result["quantity_hours_times100"], 100);
    assert_eq!(result["bank"]["title"], "International transfer");
    assert_eq!(result["client"]["legal_name"], "Société Acme");
    assert_eq!(result["from"], "2024-01-01");
    assert_eq!(result["to"], "2024-02-01");
}

#[test]
fn bankers_rounding_applies_to_quantity_then_cents() {
    for (ticks, qty, total) in [(18, 2, 2), (54, 8, 10)] {
        let mut f = Fixture::new("hourly", 125);
        f.ticks(ticks);
        let result = f.run("generate", &f.args());
        assert_eq!(result["quantity_hours_times100"], qty);
        assert_eq!(result["total_cents"], total);
    }
}

#[test]
fn invalid_money_discount_and_empty_period_never_allocate() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    let mut args = f.args();
    for amount in ["-1", "0.001", "NaN", "inf", "9223372036854775808"] {
        args.amount = Some(amount.into());
        assert!(
            execute(&mut f.conn, &f.config, "advance", &args).is_err(),
            "{amount}"
        );
    }
    args.discount = Some("100.01".into());
    assert!(execute(&mut f.conn, &f.config, "generate", &args).is_err());
    assert_eq!(f.count("sender_state"), 0);
    let mut hourly = Fixture::new("hourly", 10000);
    let args = hourly.args();
    assert!(execute(&mut hourly.conn, &hourly.config, "generate", &args).is_err());
}

#[test]
fn advance_credit_overflow_is_rejected_before_committing() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    f.conn.execute("INSERT INTO invoices(company_id,sent_at,created_at,number,total_cents,currency,kind) VALUES(1,1704067200,1704067200,'OLD',?1,'EUR','advance')",[i64::MAX-10]).unwrap();
    let mut args = f.args();
    args.amount = Some("0.11".into());
    assert!(execute(&mut f.conn, &f.config, "advance", &args).is_err());
    assert_eq!(f.count("invoices"), 1);
    assert_eq!(f.count("sender_state"), 0);
    assert!(!f.dir.path().join("pdfs/primary/INV-041.pdf").exists());
}

#[test]
fn failed_insert_removes_staged_pdf_and_preserves_cursor() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    f.conn.execute_batch("CREATE TRIGGER fail_invoice BEFORE INSERT ON invoices BEGIN SELECT RAISE(ABORT,'fixture insertion error'); END;").unwrap();
    let args = f.args();
    assert!(execute(&mut f.conn, &f.config, "generate", &args).is_err());
    assert_eq!(f.count("sender_state"), 0);
    assert_eq!(
        std::fs::read_dir(f.dir.path().join("pdfs/primary"))
            .unwrap()
            .count(),
        0
    );
    f.conn.execute_batch("DROP TRIGGER fail_invoice;").unwrap();
    assert_eq!(f.run("generate", &args)["number"], "INV-041");
}

#[test]
fn credit_is_currency_scoped_and_no_credit_still_reports_balance() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    f.conn.execute_batch("INSERT INTO invoices(company_id,sent_at,created_at,number,total_cents,currency,kind) VALUES(1,1704067200,1704067200,'USD-OLD',99999,'USD','advance'),(1,1704067200,1704067200,'EUR-OLD',7000,'EUR','advance');").unwrap();
    let mut args = f.args();
    args.no_credit = true;
    let invoice = f.run("generate", &args);
    assert_eq!(invoice["total_cents"], 10000);
    assert_eq!(invoice["credit_applied_cents"], 0);
    assert_eq!(invoice["credit_remaining_cents"], 7000);
    let balance = f.run("balance", &args);
    assert_eq!(balance["remaining_cents"], 7000);
    assert_eq!(balance["rows"].as_array().unwrap().len(), 1);
}

#[test]
fn fifo_reference_moves_past_exhausted_advance_and_negative_balance_is_clamped() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    f.conn.execute_batch("INSERT INTO invoices(company_id,sent_at,created_at,number,total_cents,currency,kind,credit_applied_cents) VALUES(1,1704067200,1704067200,'A1',1000,'EUR','advance',0),(1,1704153600,1704153600,'A2',5000,'EUR','advance',0),(1,1704240000,1704240000,'D1',0,'EUR','hourly',2000);").unwrap();
    let invoice = f.run("generate", &f.args());
    assert_eq!(invoice["credit_applied_cents"], 4000);
    assert_eq!(invoice["credit_applied_ref"], "A2");
    f.conn
        .execute("DELETE FROM invoices WHERE kind='advance'", [])
        .unwrap();
    let invoice = f.run("generate", &f.args());
    assert_eq!(invoice["total_cents"], 10000);
    assert_eq!(invoice["credit_applied_cents"], 0);
}

#[test]
fn numbering_is_per_sender_and_sender_output_directory_has_no_subfolder() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_slice(&std::fs::read(&f.config).unwrap()).unwrap();
    yaml["senders"]["secondary"] = yaml["senders"]["primary"].clone();
    yaml["senders"]["secondary"]["invoice"]["number_prefix"] = "OTHER-".into();
    yaml["senders"]["secondary"]["invoice"]["next_number"] = 8.into();
    yaml["senders"]["secondary"]["output_dir"] =
        f.dir.path().join("custom").to_str().unwrap().into();
    std::fs::write(&f.config, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    assert_eq!(f.run("generate", &f.args())["number"], "INV-041");
    f.conn
        .execute("UPDATE companies SET billed_from='secondary'", [])
        .unwrap();
    let second = f.run("generate", &f.args());
    assert_eq!(second["number"], "OTHER-008");
    assert_eq!(
        PathBuf::from(second["pdf_path"].as_str().unwrap()),
        f.dir.path().join("custom/OTHER-008.pdf")
    );
    f.conn
        .execute("UPDATE companies SET billed_from='primary'", [])
        .unwrap();
    assert_eq!(f.run("generate", &f.args())["number"], "INV-042");
}

#[test]
fn send_accepts_go_local_datetime_formats_and_list_keeps_notes() {
    let mut f = Fixture::new("hourly", 10000);
    for at in ["2024-01-01 12:34", "2024-01-01T12:34:56"] {
        let mut args = f.args();
        args.at = Some(at.into());
        args.note = "Manually sent to client".into();
        let sent = f.run("send", &args);
        assert!(sent["id"].as_i64().unwrap() > 0);
    }
    let list = f.run("list", &f.args());
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    assert_eq!(list["items"][0]["note"], "Manually sent to client");
}

#[test]
fn concurrent_generation_serializes_number_allocation_and_credit_drawdown() {
    let f = Fixture::new("monthly_fixed", 10000);
    f.conn.execute_batch("INSERT INTO invoices(company_id,sent_at,created_at,number,total_cents,currency,kind) VALUES(1,1704067200,1704067200,'PREPAID',15000,'EUR','advance');").unwrap();
    let path = f.dir.path().join("concurrent.sqlite");
    f.conn
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let config = f.config.clone();
            let args = f.args();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut conn = Connection::open(path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                barrier.wait();
                execute(&mut conn, &config, "generate", &args).unwrap()
            })
        })
        .collect();
    let mut results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    results.sort_by_key(|v| v["number"].as_str().unwrap().to_owned());
    assert_eq!(results[0]["number"], "INV-041");
    assert_eq!(results[1]["number"], "INV-042");
    assert_eq!(
        results
            .iter()
            .map(|r| r["credit_applied_cents"].as_i64().unwrap())
            .sum::<i64>(),
        15000
    );
    assert_eq!(
        results
            .iter()
            .map(|r| r["total_cents"].as_i64().unwrap())
            .sum::<i64>(),
        5000
    );
    let conn = Connection::open(path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT next_invoice_number FROM sender_state WHERE sender_key='primary'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        43
    );
}

#[test]
fn allow_empty_explicitly_generates_zero_hour_invoice() {
    let mut f = Fixture::new("hourly", 10000);
    let mut args = f.args();
    args.allow_empty = true;
    let invoice = f.run("generate", &args);
    assert_eq!(invoice["ticks"], 0);
    assert_eq!(invoice["total_cents"], 0);
    assert_eq!(f.count("invoices"), 1);
}

#[test]
fn pdf_supports_logo_multiline_fields_pagination_and_credit_breakdown() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    let logo = f.dir.path().join("logo.png");
    image::RgbaImage::from_pixel(20, 20, image::Rgba([0, 130, 150, 190]))
        .save(&logo)
        .unwrap();
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_slice(&std::fs::read(&f.config).unwrap()).unwrap();
    yaml["senders"]["primary"]["logo_path"] = logo.to_str().unwrap().into();
    let fields: Vec<_> = (0..10).map(|i| {
        serde_yaml::to_value(serde_json::json!({
            "label": format!("Bank details {}", i + 1),
            "value": "A long international bank address that should wrap clearly within its column without clipping any words\nAccount holder: José Sender\nMadrid, España"
        })).unwrap()
    }).collect();
    yaml["senders"]["primary"]["bank_accounts"]["USD"]["fields"] = fields.into();
    std::fs::write(&f.config, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    f.conn.execute_batch("INSERT INTO invoices(company_id,sent_at,created_at,number,total_cents,currency,kind) VALUES(1,1704067200,1704067200,'PREPAID-1',5000,'EUR','advance');").unwrap();
    let mut args = f.args();
    args.discount = Some("5.00".into());
    let invoice = f.run("generate", &args);
    assert_eq!(invoice["total_cents"], 4500);
    let bytes = std::fs::read(invoice["pdf_path"].as_str().unwrap()).unwrap();
    assert!(
        bytes
            .windows(b"/Subtype /Image".len())
            .any(|v| v == b"/Subtype /Image")
    );
    // PDF page dictionaries must declare resources once: a second dictionary
    // for the logo hides the font dictionary in real PDF readers.
    let structure = String::from_utf8_lossy(&bytes);
    assert_eq!(
        structure.matches("/Resources <<").count(),
        structure.matches("/Type /Page\n").count()
    );
    if let Some(path) = std::env::var_os("RIT_INVOICE_LONG_FIXTURE_PDF") {
        std::fs::write(path, &bytes).unwrap();
    }
}

#[test]
fn existing_pdf_is_never_overwritten_and_does_not_consume_a_number() {
    let mut f = Fixture::new("monthly_fixed", 10000);
    let output = f.dir.path().join("pdfs/primary");
    std::fs::create_dir_all(&output).unwrap();
    let path = output.join("INV-041.pdf");
    std::fs::write(&path, "existing invoice artifact").unwrap();
    let args = f.args();
    assert!(execute(&mut f.conn, &f.config, "generate", &args).is_err());
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "existing invoice artifact"
    );
    assert_eq!(f.count("sender_state"), 0);
    assert_eq!(f.count("invoices"), 0);
}
