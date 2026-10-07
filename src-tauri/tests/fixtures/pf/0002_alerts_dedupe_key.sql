
ALTER TABLE alerts ADD COLUMN dedupe_key TEXT NOT NULL DEFAULT '';
CREATE UNIQUE INDEX idx_alerts_dedupe_key ON alerts(dedupe_key);
