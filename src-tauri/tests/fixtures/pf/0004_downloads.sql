
CREATE TABLE downloads (
    video_id            TEXT NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
    format              TEXT NOT NULL
                            CHECK (format IN ('audio','video')),
    status              TEXT NOT NULL
                            CHECK (status IN ('queued','running','available','error','missing')),
    requested_quality   TEXT NOT NULL,
    thumbnail_mode      TEXT NOT NULL DEFAULT 'embed',
    dest_dir            TEXT NOT NULL,
    file_path           TEXT,
    file_size_bytes     INTEGER,
    container           TEXT,
    error               TEXT,
    attempts            INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL,
    completed_at        TEXT,
    last_verified_at    TEXT,
    PRIMARY KEY (video_id, format)
);
CREATE INDEX idx_downloads_status ON downloads(status);
