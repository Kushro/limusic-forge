
CREATE TABLE accounts (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    email_hint      TEXT,
    added_at        TEXT NOT NULL,
    last_sync_at    TEXT
);

CREATE TABLE playlists (
    id                  TEXT PRIMARY KEY,
    account_id          TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    privacy             TEXT NOT NULL DEFAULT 'private'
                            CHECK (privacy IN ('public','unlisted','private')),
    item_count          INTEGER NOT NULL DEFAULT 0,
    thumb_url           TEXT,
    first_seen          TEXT NOT NULL,
    last_seen           TEXT NOT NULL,
    deleted_remotely    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_playlists_account_id ON playlists(account_id);

CREATE TABLE videos (
    id                  TEXT PRIMARY KEY,
    best_title          TEXT,
    best_channel_id     TEXT,
    best_channel_title  TEXT,
    duration_s          INTEGER,
    published_at        TEXT,
    thumb_path          TEXT,
    status              TEXT NOT NULL DEFAULT 'unknown'
                            CHECK (status IN ('available','private','deleted','region_blocked','age_restricted','unknown')),
    status_changed_at   TEXT,
    first_seen          TEXT NOT NULL,
    last_seen           TEXT NOT NULL
);

CREATE TABLE playlist_snapshots (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id     TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    taken_at        TEXT NOT NULL,
    content_hash    TEXT NOT NULL,
    item_count      INTEGER NOT NULL,
    is_current      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_playlist_snapshots_playlist_taken ON playlist_snapshots(playlist_id, taken_at);
CREATE UNIQUE INDEX idx_playlist_snapshots_current ON playlist_snapshots(playlist_id) WHERE is_current = 1;

CREATE TABLE snapshot_items (
    snapshot_id         INTEGER NOT NULL REFERENCES playlist_snapshots(id) ON DELETE CASCADE,
    position            INTEGER NOT NULL,
    playlist_item_id    TEXT NOT NULL,
    video_id            TEXT NOT NULL REFERENCES videos(id) ON DELETE RESTRICT,
    title_at_time       TEXT,
    channel_at_time     TEXT,
    added_at            TEXT,
    PRIMARY KEY (snapshot_id, position)
);
CREATE INDEX idx_snapshot_items_video_id ON snapshot_items(video_id);

-- F4 (snapshots/diff/alerts): DDL only in F2, no reader/writer code yet.
CREATE TABLE alerts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    playlist_id     TEXT REFERENCES playlists(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL,
    video_id        TEXT REFERENCES videos(id) ON DELETE SET NULL,
    payload_json    TEXT NOT NULL DEFAULT '{}',
    created_at      TEXT NOT NULL,
    seen            INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_alerts_account_created ON alerts(account_id, created_at);

-- F6 (write job queue): DDL only in F2, no reader/writer code yet.
CREATE TABLE jobs (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id          TEXT REFERENCES accounts(id) ON DELETE CASCADE,
    kind                TEXT NOT NULL,
    params_json         TEXT NOT NULL DEFAULT '{}',
    status              TEXT NOT NULL DEFAULT 'queued',
    priority            INTEGER NOT NULL DEFAULT 2,
    phase               INTEGER NOT NULL DEFAULT 1,
    total_phases        INTEGER NOT NULL DEFAULT 1,
    resume_at           TEXT,
    created_at          TEXT NOT NULL,
    started_at          TEXT,
    finished_at         TEXT,
    est_units_total     INTEGER NOT NULL DEFAULT 0,
    spent_units         INTEGER NOT NULL DEFAULT 0,
    total_items         INTEGER NOT NULL DEFAULT 0,
    done_items          INTEGER NOT NULL DEFAULT 0,
    failed_items        INTEGER NOT NULL DEFAULT 0,
    skipped_items       INTEGER NOT NULL DEFAULT 0,
    last_error          TEXT
);
CREATE INDEX idx_jobs_status_priority_created ON jobs(status, priority, created_at);

CREATE TABLE job_items (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id              INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    seq                 INTEGER NOT NULL,
    phase               INTEGER NOT NULL DEFAULT 1,
    action              TEXT NOT NULL,
    params_json         TEXT NOT NULL DEFAULT '{}',
    status              TEXT NOT NULL DEFAULT 'pending',
    api_result_json     TEXT,
    inverse_json        TEXT,
    attempts            INTEGER NOT NULL DEFAULT 0,
    last_error          TEXT,
    updated_at          TEXT NOT NULL
);
CREATE INDEX idx_job_items_job_phase_seq ON job_items(job_id, phase, seq);

-- F2: quota ledger — global to the GCP project (doc 08 §1.1), `account_id`
-- is informational only.
CREATE TABLE quota_ledger (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts              TEXT NOT NULL,
    endpoint        TEXT NOT NULL,
    units           INTEGER NOT NULL,
    account_id      TEXT REFERENCES accounts(id) ON DELETE SET NULL,
    job_id          INTEGER REFERENCES jobs(id) ON DELETE SET NULL
);
CREATE INDEX idx_quota_ledger_ts ON quota_ledger(ts);

-- F6: single-row lock (UI runner vs headless monitor runner, doc 08 §2.5).
CREATE TABLE runner_lock (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    pid             INTEGER NOT NULL,
    heartbeat_at    TEXT NOT NULL
);

CREATE TABLE settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);
