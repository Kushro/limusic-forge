
CREATE TABLE monitor_runs (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at          TEXT NOT NULL,
    finished_at         TEXT NOT NULL,
    trigger             TEXT NOT NULL
                            CHECK (trigger IN ('manual_ui','scheduler','headless')),
    outcome             TEXT NOT NULL
                            CHECK (outcome IN ('ok','partial','failed','lock_busy','cancelled')),
    accounts_ok         INTEGER NOT NULL DEFAULT 0,
    accounts_failed     INTEGER NOT NULL DEFAULT 0,
    accounts_skipped    INTEGER NOT NULL DEFAULT 0,
    alerts_new          INTEGER NOT NULL DEFAULT 0,
    units_spent         INTEGER NOT NULL DEFAULT 0,
    detail_json         TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX idx_monitor_runs_started_at ON monitor_runs(started_at DESC);
