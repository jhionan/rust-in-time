-- A company groups projects.
CREATE TABLE IF NOT EXISTS companies (
    id            INTEGER PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,
    created_at    INTEGER NOT NULL,
    billing_mode  TEXT NOT NULL DEFAULT 'none'
                  CHECK (billing_mode IN ('none','hourly','monthly_fixed')),
    currency      TEXT,
    rate_cents    INTEGER,
    billed_from   TEXT
) STRICT;

-- A project = a billable bucket.
CREATE TABLE IF NOT EXISTS projects (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    company_id  INTEGER REFERENCES companies(id) ON DELETE SET NULL,
    paused      INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
) STRICT;

-- The allowlist: what the daemon may observe.
CREATE TABLE IF NOT EXISTS watched_programs (
    id          INTEGER PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('bundle', 'binary')),
    identifier  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    UNIQUE (kind, identifier)
) STRICT;

-- Mapping rules: observation → project. At least one match column required.
CREATE TABLE IF NOT EXISTS rules (
    id                  INTEGER PRIMARY KEY,
    project_id          INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    priority            INTEGER NOT NULL DEFAULT 100,
    match_bundle_id     TEXT,
    match_title_substr  TEXT,
    match_binary_name   TEXT,
    match_cwd_prefix    TEXT,
    match_space_id      TEXT,
    created_at          INTEGER NOT NULL,
    CHECK (
        match_bundle_id IS NOT NULL OR
        match_title_substr IS NOT NULL OR
        match_binary_name IS NOT NULL OR
        match_cwd_prefix IS NOT NULL OR
        match_space_id IS NOT NULL
    )
) STRICT;
CREATE INDEX IF NOT EXISTS idx_rules_priority ON rules(priority);

-- Distinct signal signatures. NOT NULL DEFAULT '' so UNIQUE dedup works
-- (SQLite UNIQUE treats NULL != NULL otherwise).
CREATE TABLE IF NOT EXISTS observations (
    id              INTEGER PRIMARY KEY,
    source          TEXT NOT NULL CHECK (source IN ('focus', 'agent', 'transcript')),
    bundle_id       TEXT NOT NULL DEFAULT '',
    window_title    TEXT NOT NULL DEFAULT '',
    binary_name     TEXT NOT NULL DEFAULT '',
    cwd             TEXT NOT NULL DEFAULT '',
    space_id        TEXT NOT NULL DEFAULT '',
    first_seen      INTEGER NOT NULL,
    UNIQUE (source, bundle_id, window_title, binary_name, cwd, space_id)
) STRICT;

CREATE TABLE IF NOT EXISTS ignored_observations (
    observation_id  INTEGER PRIMARY KEY REFERENCES observations(id) ON DELETE CASCADE,
    ignored_at      INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS ticks (
    ts              INTEGER NOT NULL,
    observation_id  INTEGER NOT NULL REFERENCES observations(id),
    project_id      INTEGER REFERENCES projects(id),
    PRIMARY KEY (ts, observation_id)
) STRICT;
CREATE INDEX IF NOT EXISTS idx_ticks_project_ts ON ticks(project_id, ts);
CREATE INDEX IF NOT EXISTS idx_ticks_unassigned ON ticks(observation_id) WHERE project_id IS NULL;

CREATE TABLE IF NOT EXISTS invoices (
    id           INTEGER PRIMARY KEY,
    company_id   INTEGER NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    sent_at      INTEGER NOT NULL,
    note         TEXT NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    number       TEXT,
    pdf_path     TEXT,
    total_cents  INTEGER,
    currency     TEXT,
    sender_key   TEXT,
    kind                  TEXT NOT NULL DEFAULT 'hourly'
                            CHECK (kind IN ('hourly','advance')),
    credit_applied_cents  INTEGER NOT NULL DEFAULT 0,
    discount_cents        INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX IF NOT EXISTS idx_invoices_company_sent ON invoices(company_id, sent_at DESC);

-- Per-sender invoice number cursor. Seed value comes from config the first
-- time a sender_key is used; subsequent allocations increment the row in this
-- table. Tax authorities require gapless numbering per legal entity, so we
-- never reset the counter or pull from MAX(invoices.number).
CREATE TABLE IF NOT EXISTS sender_state (
    sender_key            TEXT PRIMARY KEY,
    next_invoice_number   INTEGER NOT NULL
) STRICT;

-- Worker heartbeat and status metadata are independent from portable data.
CREATE TABLE IF NOT EXISTS runtime_state (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
) STRICT;
