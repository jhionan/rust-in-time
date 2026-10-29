# Rust in Time

A Rust port of [Antitimely](https://github.com/jhionan/antitimely) for Windows. Tracks work across projects, including agents working in parallel, and generates reports and invoice PDFs. One executable, one SQLite database, no Go runtime.

Windows tracking combines foreground window titles, sustained process CPU activity and Claude Code / Codex / opencode transcripts. Company billing deduplicates time across that company's projects. Work for different companies can count in parallel.

## Tech stack

| Area | Technology |
| --- | --- |
| Language and build | Rust 2024 edition (Rust 1.88+), Cargo, Windows MSVC toolchain |
| CLI and terminal UI | `clap` for commands and arguments; `crossterm` for terminal interaction |
| Storage | SQLite via `rusqlite`, with bundled SQLite, WAL mode and foreign keys |
| Windows tracking | Win32 APIs via the `windows` crate for foreground windows and input idle time; `sysinfo` for process CPU activity and working directories |
| Background startup | Windows Task Scheduler (`schtasks.exe`), running per user at logon |
| Configuration and data formats | `serde`, `serde_yaml` and `serde_json` for YAML configuration and JSON output/transcripts |
| Time handling | `chrono` for dates and local time; `humantime` for duration settings |
| Invoice PDFs | `pdf-writer`, `ttf-parser` and `image`, with an embedded Ubuntu font and PNG/JPEG logo support |
| Worker lifecycle | `fs2` file locking for one worker per data directory; `ctrlc` for graceful shutdown |
| Quality and CI | `rustfmt`, Clippy, Rust unit/integration tests and `tempfile`; GitHub Actions checks on Windows and Linux and builds a Windows x64 executable |

Building on Windows requires Rust and the Visual Studio C++ build tools. Running the compiled `rit.exe` does not require a Rust SDK or Cargo installation.

## Build and run

Install Rust 1.88 or later and the Visual Studio C++ build tools, then:

```powershell
git clone git@github.com:jhionan/rust-in-time.git
cd rust-in-time
cargo build --release
.\target\release\rit.exe --help
```

For CI builds, download **rust-in-time-windows-x64** from the repository's Actions run. Put `rit.exe` in a permanent directory on your PATH before installing startup.

```powershell
rit company add Acme
rit project add api --company Acme
rit rules add --project api --cwd 'C:\Work\api'
rit watch add binary claude.exe
rit watch add binary codex.exe
rit watch add app Code.exe
rit rules add --project api --bundle code.exe --title api

# Run in this terminal; Ctrl-C stops gracefully.
rit daemon

# Or install and immediately start a per-user task at Windows logon.
rit install-startup
rit status
```

The task runs in your interactive Windows session, with standard user privileges. Keep the executable at the path used for installation. `rit stop` requests a graceful stop; `rit restart` starts the installed task again; `rit uninstall-startup` removes it. These startup commands are Windows-specific. No macOS daemon or launchd integration is included.

## Commands

Run `rit` in a terminal for an interactive menu, including rules, watched programs, reports, config and invoice preview/confirmation, or use commands directly. Flags can appear before or after positional arguments. `--json`, `--data-dir`, and `--config` apply to every command.

| Command | Purpose |
| --- | --- |
| `status [--once]` | Today's project/company totals, unbilled hours and worker heartbeat; live in terminals |
| `company add/list/delete` | Manage companies; deleting one with invoices requires `--force` |
| `company set-billing NAME --mode hourly --currency EUR --rate 50 --billed-from personal` | Configure invoice pricing and sender |
| `project add NAME [--company NAME]` | Create a project |
| `project list/delete/set-company/pause/resume` | Manage project ownership and activity |
| `end-day` / `start-day` | Pause / resume all projects |
| `watch add app/binary NAME`, `watch list/remove` | Manage observed executable names |
| `rules add --project NAME [--cwd PATH --bundle EXE --binary EXE --title TEXT --space ID --priority 100]` | Create a conjunctive rule; lowest priority number, then oldest ID, wins |
| `rules list/delete`, `rules add … --retroactive` | Inspect/delete rules or fill matching unassigned history |
| `review` | Interactively attribute or ignore unassigned observations |
| `review --list`, `review --id ID --project NAME [--no-rule]`, `review --id ID --ignore` | Scriptable review |
| `report --from YYYY-MM-DD --to YYYY-MM-DD` | Project hours and deduplicated company totals |
| `summary [--from … --to … --company … --project … --all-authors --txt]` | Hours and Git commits; defaults to your configured Git email |
| `import-transcripts [--from … --to …]` | Fill gaps from historical Claude transcripts; never lays a second grid over tracked work |
| `invoice generate NAME [--from … --to … --issue-date … --discount 50 --no-credit --dry-run --allow-empty]` | Generate numbered invoice PDF and close the period |
| `invoice advance NAME --amount 500 [--dry-run]` | Record an advance invoice without moving the billing anchor |
| `invoice balance/send/list/delete/show-senders` | Credit balances, cutoff anchors, invoice history and sender validation |
| `invoice setup` | Interactively set company pricing, currency and sender |
| `config init/show/path` | Create, view or locate shared YAML config |
| `reset ticks/all --yes` | Delete tracking data; `all` also deletes projects, rules and invoices |
| `spaces`, `debug frontmost/windows/idle` | Inspect optional Herdr metadata or Windows signals |
| `daemon [--once]` | Foreground tracker; `--once` collects a single poll |

Dates in reports, summaries and imports use local time; `--to` is **exclusive**. Use tomorrow's date to include all of today. Names are exact and case-sensitive. Reset preserves invoice numbering cursors to prevent reusing issued numbers.

## Accounting and rules

Each distinct observation references a timestamp on a fixed **five-second grid**. A project's time is `COUNT(DISTINCT timestamp) × 5 seconds`; company totals deduplicate across all its projects. Multiple sources on a project cannot multiply its hours. Signals on different companies can accrue simultaneously.

Windows executable allowlists are normalized to lowercase. Focus observations carry the executable name (for example `code.exe`) in the retained `bundle_id` field; macOS bundle identifiers should be replaced when importing rules. Focus has a window title and no working directory. CPU observations use a process's actual working directory; inaccessible directories are skipped.

Windows paths accept either slash direction, compare case-insensitively, and respect directory boundaries. `C:\Work\api` matches its children, never `C:\Work\api-other`. A single wildcard as the final character supports worktrees, such as `C:\Work\trees\api-*`. Unix paths remain case-sensitive. Specified rule clauses must all match. Space rules require actual matching metadata; missing metadata never satisfies a space clause.

Review can create a rule and fill unassigned historical ticks in one transaction. Assigned history is preserved. A space-bound observation's retrospective assignment stays within its space.

## Configuration

Default state is `%LOCALAPPDATA%\rust-in-time`: `db.sqlite`, `config.yaml`, `tracker.lock`, and worker metadata in SQLite. Use `--data-dir PATH` or `RIT_DATA_DIR` for portable/isolated data. `--config PATH` selects a different YAML file. SQLite runs in WAL mode with foreign keys and a busy timeout; commands work independently of the worker. One worker is allowed per data directory.

```yaml
interval: 5s
idle_threshold: 2m
agent_cpu_threshold: 15
agent_cpu_threshold_idle: 100
agent_busy_rise_ticks: 2
agent_busy_fall_ticks: 3
transcript_tracking: true
transcript_grace: 10m
transcript_root: ~/.claude/projects
codex_root: ~/.codex/sessions
opencode_db: ~/.local/share/opencode/opencode.db
# db_path: C:/Data/rust-in-time.sqlite
```

CPU thresholds are centiseconds per poll: 15 is approximately 3% of one core over five seconds. Idle users have a stricter CPU threshold; foreground tracking stops after the idle threshold. Busy detection requires sustained activity, with rise/fall hysteresis. Active matched transcripts resume paused projects, matching Antitimely's behavior.

New projects and `start-day`/`resume-all` arm the CPU signal to prevent stale background processes from billing immediately. Matching focus/transcript activity disarms the project; twelve sustained CPU polls while the user is present also disarm it. Paused projects can auto-resume on present-user CPU activity, entering that same armed state. Idle CPU never resumes paused projects. Status exposes armed projects and suppressed ticks. Active arming clears on worker restart; commands issued while it is stopped are queued for its next poll.

The interval is fixed at five seconds to preserve SQLite accounting, including imported data. Old custom `interval` values are normalized to `5s`. Transcript paths can be `off` or empty to disable a source. Environment defaults honor `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `XDG_DATA_HOME`. Relative paths in tracking configuration resolve beside that config file, so startup and interactive commands use the same database. Collectors read complete records, retain partial writes for the next poll, and ignore stale or future activity. Source failures produce diagnostics rather than attributing guessed work. If Windows idle detection fails, the stricter idle CPU threshold applies; transcripts continue counting.

## Invoices

`rit config init` appends a commented sender/client example. [config.invoice.example.yaml](config.invoice.example.yaml) documents the shared format. Replace all placeholder details, configure a company using `company set-billing`, and preview with `--dry-run` before generating an invoice. `monthly_fixed` uses a quantity of one and a calendar-month period; hourly pricing uses deduplicated company hours with half-even rounding. Money is stored as integer cents.

PDFs embed a Unicode font and support sender/client addresses, bank details, an optional PNG/JPEG logo, discount and advance-credit rows. No messages or invoices are transmitted. `invoice send` records a cutoff only; it does not create a PDF. Advance credit is derived from advance totals minus credit already applied, and draws down automatically unless `--no-credit` is given. Deleting advances or credit-bearing invoices requires `--force`.

## Existing Antitimely data

The Rust port retains Antitimely's current SQLite schema. To experiment with a copy, create a **consistent SQLite backup** of the Go database, place the copy at your Rust data directory's `db.sqlite`, and copy/edit the YAML separately. Do not copy only the database file while its writer is active: committed changes can still be in the WAL. Avoid opening the same live database with both programs. The Rust port never discovers or modifies `~/.antitimely` automatically. Very old schemas missing current Go columns require migration in Antitimely first.

## Development

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
```

The shared logic and transcript tests also run on Linux/macOS. Native foreground-window and input-idle capture is Windows-only; other hosts run process/transcript tracking without a macOS adapter. GitHub Actions tests on Windows and Linux and builds a downloadable Windows executable. Live window capture and logon startup still need a smoke test in an interactive Windows desktop session.

The port's [design](docs/design.md) records the behavioral contract. The MIT license is retained. The embedded Ubuntu font includes its own license in `assets`.
