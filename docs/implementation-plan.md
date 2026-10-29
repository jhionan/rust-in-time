# Windows Rust port implementation plan

**Goal:** A usable Windows Rust port of Antitimely in rust-in-time.
**Spec:** [design.md](design.md)
**Source:** ../antitimely, commit 0a47e0b.

## Tasks

- [x] Port domain matching and SQLite persistence, with accounting and retroactive rule tests (`src/domain.rs`, `src/store.rs`, `schema.sql`).
- [x] Port configuration, transcript collectors, CPU hysteresis and polling; add native Windows focus/idle integration and worker lock (`src/config.rs`, `src/transcripts.rs`, `src/platform.rs`, `src/tracker.rs`).
- [x] Port invoice generation and PDF rendering, advance credit and numbering with transaction tests (`src/invoice.rs`).
- [x] Build CLI, interactive review/menu, status/report/summary, configuration and Windows startup management (`src/cli.rs`, `src/main.rs`).
- [x] Add Windows CI/release artifacts, documentation and CLI end-to-end tests; verify native Windows compilation, lint/test/release; review and resolve material findings; commit and push feature branch with explicit upstream and open PR.

Independent modules have exclusive owners and share the contracts in design.md. Root integrates and verifies the complete application. Preserve source and live tracker state. Never reset, rebase, force push, or push main directly.

## Verification record

Local verification on 2026-10-29: 62 tests pass; rustfmt and clippy with warnings denied pass; optimized Windows x64 GNU build succeeds. Invoice fixtures were rendered with Poppler and visually checked, including Unicode, logos, pagination and credit rows. Independent review findings about project arming, historical directory transitions, Claude observation parity and menu workflows were addressed; the no-company rollup was also restored. Windows CI runs the suite, builds the native executable and exercises Task Scheduler installation, worker heartbeat, stop and removal. Interactive desktop focus/input capture remains a Windows smoke-test limitation.
