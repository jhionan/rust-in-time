use super::*;

pub(super) fn run(store: &mut Store, config: &Path, data: &Path) -> Result<()> {
    loop {
        println!(
            "\nRust in Time\n  1 Status   2 Review   3 Projects   4 Companies\n  5 Add project   6 Add company   7 Pause all   8 Resume all\n  9 Install Windows startup   10 Watch programs   11 Rules\n  12 Report   13 Weekly summary   14 Invoices   15 Config\n  16 Reset data   17 Restart worker   18 Stop worker   q Quit"
        );
        let result = match prompt("Choice: ")?.as_str() {
            "q" | "" => break,
            "1" => reports::status(&store.conn, true, false),
            "2" => review::run(store, &review::ReviewArgs::default(), false),
            "3" => project(&mut store.conn, &Project::List).and_then(|v| emit(&v, false)),
            "4" => company(&mut store.conn, &Company::List).and_then(|v| emit(&v, false)),
            "5" => {
                let name = prompt("Project name: ")?;
                let c = prompt("Company (blank for none): ")?;
                project(
                    &mut store.conn,
                    &Project::Add {
                        name,
                        company: if c.is_empty() { None } else { Some(c) },
                    },
                )
                .and_then(|v| emit(&v, false))
            }
            "6" => {
                let name = prompt("Company name: ")?;
                company(&mut store.conn, &Company::Add { name }).and_then(|v| emit(&v, false))
            }
            "7" => pause_all(&store.conn, true).and_then(|v| emit(&v, false)),
            "8" => pause_all(&store.conn, false).and_then(|v| emit(&v, false)),
            "9" => crate::startup::install(data, config).and_then(|v| emit(&v, false)),
            "10" => watches(&store.conn),
            "11" => rule_menu(store),
            "12" => reports::report(&store.conn, &range_prompt()?).and_then(|v| emit(&v, false)),
            "13" => {
                let today = Local::now().date_naive();
                reports::summary(
                    &store.conn,
                    &SummaryArgs {
                        range: Range {
                            from: Some(
                                today
                                    .checked_sub_days(chrono::Days::new(6))
                                    .context("date out of range")?
                                    .to_string(),
                            ),
                            ..Range::default()
                        },
                        all_authors: false,
                        txt: false,
                    },
                    false,
                )
            }
            "14" => invoices(&mut store.conn, config),
            "15" => config_menu(config),
            "16" => {
                let scope = prompt("Reset ticks or all (blank cancels): ")?;
                if !matches!(scope.as_str(), "ticks" | "all") {
                    continue;
                }
                let confirmed = prompt("Type DELETE to confirm: ")? == "DELETE";
                if !confirmed {
                    continue;
                }
                reset(&mut store.conn, &scope, true).and_then(|v| emit(&v, false))
            }
            "17" => {
                crate::startup::restart(data, config, &store.conn).and_then(|v| emit(&v, false))
            }
            "18" => crate::startup::stop(&store.conn).and_then(|v| emit(&v, false)),
            _ => {
                println!("Choose a listed option.");
                Ok(())
            }
        };
        if let Err(e) = result {
            eprintln!("{e:#}");
        }
    }
    Ok(())
}

fn optional(label: &str) -> Result<Option<String>> {
    let value = prompt(label)?;
    Ok(if value.is_empty() { None } else { Some(value) })
}

fn range_prompt() -> Result<Range> {
    Ok(Range {
        from: optional("From YYYY-MM-DD (blank today): ")?,
        to: optional("To YYYY-MM-DD, exclusive (blank tomorrow): ")?,
        ..Range::default()
    })
}

fn watches(c: &Connection) -> Result<()> {
    println!("1 List   2 Add   3 Remove");
    let command = match prompt("Choice: ")?.as_str() {
        "1" => Watch::List,
        "2" => {
            let kind = prompt("Kind (app or binary): ")?;
            ensure!(
                matches!(kind.as_str(), "app" | "binary"),
                "choose app or binary"
            );
            Watch::Add {
                kind,
                identifier: prompt("Executable name (for example code.exe): ")?,
            }
        }
        "3" => Watch::Remove {
            identifier: prompt("Executable name: ")?,
        },
        _ => return Ok(()),
    };
    emit(&watch(c, &command)?, false)
}

fn rule_menu(store: &mut Store) -> Result<()> {
    println!("1 List   2 Add   3 Delete");
    let command = match prompt("Choice: ")?.as_str() {
        "1" => Rules::List,
        "2" => Rules::Add {
            project: prompt("Project name: ")?,
            priority: optional("Priority (blank 100): ")?
                .map(|v| v.parse::<i64>())
                .transpose()?
                .unwrap_or(100),
            cwd: optional("Working directory prefix (blank for none): ")?,
            bundle: optional("Foreground executable (blank for none): ")?,
            binary: optional("Agent binary (blank for none): ")?,
            title: optional("Window title substring (blank for none): ")?,
            space: optional("Herdr space ID (blank for none): ")?,
            retroactive: prompt("Fill matching unassigned history? [y/N]: ")? == "y",
        },
        "3" => Rules::Delete {
            id: prompt("Rule ID: ")?.parse()?,
        },
        _ => return Ok(()),
    };
    emit(&rules(store, &command)?, false)
}

fn config_menu(path: &Path) -> Result<()> {
    println!("1 Show   2 Path   3 Initialize (existing files preserved)");
    let command = match prompt("Choice: ")?.as_str() {
        "1" => ConfigCommand::Show,
        "2" => ConfigCommand::Path,
        "3" => ConfigCommand::Init,
        _ => return Ok(()),
    };
    config_command(&command, path, false)
}

pub(super) fn setup(c: &mut Connection) -> Result<Value> {
    let name = prompt("Company name: ")?;
    let mode = prompt("Billing mode (hourly or monthly_fixed): ")?;
    ensure!(
        matches!(mode.as_str(), "hourly" | "monthly_fixed"),
        "choose hourly or monthly_fixed"
    );
    let currency = Some(prompt("Currency code (for example EUR): ")?);
    let rate = Some(prompt("Hourly rate / monthly amount: ")?);
    let billed_from = Some(prompt("Sender key in config.yaml: ")?);
    company(
        c,
        &Company::SetBilling {
            name,
            mode,
            currency,
            rate,
            rate_cents: None,
            billed_from,
        },
    )
}

fn invoices(c: &mut Connection, path: &Path) -> Result<()> {
    println!(
        "1 Generate PDF   2 Advance PDF   3 History   4 Credit balance\n5 Record billing cutoff only   6 Delete record   7 Show senders   8 Set billing"
    );
    let choice = prompt("Choice: ")?;
    if choice == "8" {
        return emit(&setup(c)?, false);
    }
    if choice == "7" {
        return emit(
            &invoice::execute(c, path, "show-senders", &InvoiceArgs::default())?,
            false,
        );
    }
    let mut args = InvoiceArgs::default();
    let op = match choice.as_str() {
        "1" => "generate",
        "2" => "advance",
        "3" => "list",
        "4" => "balance",
        "5" => "send",
        "6" => "delete",
        _ => return Ok(()),
    };
    if op == "delete" {
        args.id = Some(prompt("Invoice ID: ")?.parse()?);
        args.force = prompt("Force deletion of credit-bearing invoice? [y/N]: ")? == "y";
        if prompt("Delete this invoice record? [y/N]: ")? != "y" {
            return Ok(());
        }
    } else if op == "list" {
        args.company = optional("Company (blank for all): ")?;
    } else {
        args.company = Some(prompt("Company name: ")?);
    }
    if op == "send" {
        args.at = optional("Cutoff date YYYY-MM-DD (blank now): ")?;
        args.note = prompt("Note (optional): ")?;
        if prompt("Record cutoff without a PDF? [y/N]: ")? != "y" {
            return Ok(());
        }
    }
    if matches!(op, "generate" | "advance") {
        if op == "advance" {
            args.amount = Some(prompt("Advance amount: ")?);
        } else {
            args.from = optional("Period from YYYY-MM-DD (blank automatic): ")?;
            args.to = optional("Period to YYYY-MM-DD, exclusive (blank automatic): ")?;
            args.discount = optional("Discount amount (blank none): ")?;
        }
        args.issue_date = optional("Issue date YYYY-MM-DD (blank today): ")?;
        args.note = prompt("Internal note (optional): ")?;
        args.dry_run = true;
        let preview = invoice::execute(c, path, op, &args)?;
        emit(&preview, false)?;
        #[cfg(windows)]
        if let Some(pdf) = preview["pdf_path"].as_str() {
            let _ = std::process::Command::new("explorer.exe").arg(pdf).spawn();
        }
        if prompt("Generate final numbered PDF and save billing record? [y/N]: ")? != "y" {
            return Ok(());
        }
        args.dry_run = false;
    }
    emit(&invoice::execute(c, path, op, &args)?, false)
}
