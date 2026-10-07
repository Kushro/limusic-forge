-- Fictitious PlaylistForge data for the importer's tests (pf_import). Runs after 0001-0005.
-- Dates are written the way PlaylistForge writes them (`to_rfc3339()`: `+00:00`, sometimes with
-- fractional seconds). Nothing here is a real account, playlist, video or token.

INSERT INTO accounts (id, title, email_hint, added_at, last_sync_at) VALUES
    ('UCfakeAccountOne00000001', 'Cuenta Uno', 'u***@example.com', '2026-09-01T10:00:00+00:00', '2026-10-05T10:00:00.250+00:00'),
    ('UCfakeAccountTwo00000002', 'Cuenta Dos', NULL, '2026-09-02T11:30:00+00:00', NULL);

INSERT INTO videos (id, best_title, best_channel_id, best_channel_title, duration_s, published_at, thumb_path, status, status_changed_at, first_seen, last_seen) VALUES
    ('vidAAAAAAA1', 'Fake Song One', 'UCchanA', 'Fake Artist A', 213, '2020-01-01T00:00:00+00:00', NULL, 'available', NULL, '2026-09-01T10:00:00+00:00', '2026-10-05T10:00:00+00:00'),
    ('vidBBBBBBB2', 'Fake Song Two', 'UCchanB', 'Fake Artist B', 187, NULL, NULL, 'deleted', '2026-10-04T09:00:00+00:00', '2026-09-01T10:00:00+00:00', '2026-10-05T10:00:00+00:00'),
    ('vidCCCCCCC3', 'Fake Song Three', NULL, 'Fake Artist C', NULL, NULL, NULL, 'private', '2026-10-04T09:00:00+00:00', '2026-09-01T10:00:00+00:00', '2026-10-05T10:00:00+00:00'),
    ('vidDDDDDDD4', NULL, NULL, NULL, NULL, NULL, NULL, 'unknown', NULL, '2026-10-05T10:00:00+00:00', '2026-10-05T10:00:00+00:00');

INSERT INTO playlists (id, account_id, title, description, privacy, item_count, thumb_url, first_seen, last_seen, deleted_remotely) VALUES
    ('PLfakeOne', 'UCfakeAccountOne00000001', 'Fake Mix', 'first', 'public', 3, 'https://example.invalid/one.jpg', '2026-09-01T10:00:00+00:00', '2026-10-05T10:00:00+00:00', 0),
    ('PLfakeTwo', 'UCfakeAccountOne00000001', 'Gone Mix', '', 'unlisted', 1, NULL, '2026-09-01T10:00:00+00:00', '2026-10-01T10:00:00+00:00', 1),
    ('PLfakeThree', 'UCfakeAccountTwo00000002', 'Empty Mix', '', 'private', 0, NULL, '2026-09-02T11:30:00+00:00', '2026-10-05T10:00:00+00:00', 0);

-- PLfakeOne: an older snapshot (2 items) and the current one (3 items). PLfakeTwo: current only.
INSERT INTO playlist_snapshots (id, playlist_id, taken_at, content_hash, item_count, is_current) VALUES
    (1, 'PLfakeOne', '2026-10-01T10:00:00+00:00', 'hash-one-old', 2, 0),
    (2, 'PLfakeOne', '2026-10-05T10:00:00.250+00:00', 'hash-one-new', 3, 1),
    (3, 'PLfakeTwo', '2026-10-01T10:00:00+00:00', 'hash-two', 1, 1);

INSERT INTO snapshot_items (snapshot_id, position, playlist_item_id, video_id, title_at_time, channel_at_time, added_at) VALUES
    (1, 0, 'PIone0', 'vidAAAAAAA1', 'Fake Song One', 'Fake Artist A', '2026-09-01T10:00:00+00:00'),
    (1, 1, 'PIone1', 'vidBBBBBBB2', 'Fake Song Two', 'Fake Artist B', '2026-09-02T10:00:00+00:00'),
    (2, 0, 'PIone1', 'vidBBBBBBB2', 'Deleted video', NULL, '2026-09-02T10:00:00+00:00'),
    (2, 1, 'PIone0', 'vidAAAAAAA1', 'Fake Song One', 'Fake Artist A', '2026-09-01T10:00:00+00:00'),
    (2, 2, 'PIone2', 'vidCCCCCCC3', 'Private video', NULL, NULL),
    (3, 0, 'PItwo0', 'vidDDDDDDD4', 'Fake Song Four', 'Fake Artist D', '2026-09-03T10:00:00+00:00');

INSERT INTO alerts (id, account_id, playlist_id, kind, video_id, payload_json, created_at, seen, dedupe_key) VALUES
    (1, 'UCfakeAccountOne00000001', 'PLfakeOne', 'video_deleted', 'vidBBBBBBB2', '{"video_id":"vidBBBBBBB2","title":"Fake Song Two","channel":"Fake Artist B","url":"https://youtu.be/vidBBBBBBB2"}', '2026-10-05T10:00:01+00:00', 1, 'PLfakeOne|2|video_deleted|vidBBBBBBB2'),
    (2, 'UCfakeAccountOne00000001', 'PLfakeOne', 'video_private', 'vidCCCCCCC3', '{"video_id":"vidCCCCCCC3","title":"Fake Song Three","channel":"Fake Artist C"}', '2026-10-05T10:00:01+00:00', 0, 'PLfakeOne|2|video_private|vidCCCCCCC3'),
    (3, 'UCfakeAccountOne00000001', 'PLfakeOne', 'video_restored', 'vidAAAAAAA1', '{"video_id":"vidAAAAAAA1","title":"Fake Song One","channel":"Fake Artist A"}', '2026-10-05T10:00:01+00:00', 0, 'PLfakeOne|2|video_restored|vidAAAAAAA1'),
    (4, 'UCfakeAccountOne00000001', 'PLfakeOne', 'item_added', 'vidCCCCCCC3', '{"video_id":"vidCCCCCCC3","title":"Fake Song Three","position":2}', '2026-10-05T10:00:01+00:00', 1, 'PLfakeOne|2|item_added|PIone2'),
    (5, 'UCfakeAccountOne00000001', 'PLfakeTwo', 'item_removed', 'vidAAAAAAA1', '{"video_id":"vidAAAAAAA1","title":"Fake Song One","channel":"Fake Artist A","position":4}', '2026-10-01T10:00:01+00:00', 0, 'PLfakeTwo|3|item_removed|PItwoX'),
    (6, 'UCfakeAccountOne00000001', 'PLfakeOne', 'item_moved', 'vidAAAAAAA1', '{"video_id":"vidAAAAAAA1","from":0,"to":1}', '2026-10-05T10:00:01+00:00', 0, 'PLfakeOne|2|item_moved|PIone0'),
    (7, 'UCfakeAccountOne00000001', 'PLfakeOne', 'mystery_kind', 'vidAAAAAAA1', '{}', '2026-10-05T10:00:01+00:00', 0, 'PLfakeOne|2|mystery_kind|x'),
    (8, 'UCfakeAccountOne00000001', NULL, 'item_added', NULL, '{}', '2026-10-05T10:00:01+00:00', 0, 'none|2|item_added|y');

-- Job 1: queued, two items. Job 2: completed (history). Job 3: waiting for quota, no items yet.
INSERT INTO jobs (id, account_id, kind, params_json, status, priority, phase, total_phases, resume_at, created_at, started_at, finished_at, est_units_total, spent_units, total_items, done_items, failed_items, skipped_items, last_error, planned_items, retried_items) VALUES
    (1, 'UCfakeAccountOne00000001', 'add_items', '{"playlist_id":"PLfakeOne"}', 'queued', 1, 1, 1, NULL, '2026-10-05T09:00:00+00:00', NULL, NULL, 100, 0, 2, 0, 0, 0, NULL, 2, 0),
    (2, 'UCfakeAccountOne00000001', 'remove_items', '{}', 'completed', 2, 1, 1, NULL, '2026-10-04T09:00:00+00:00', '2026-10-04T09:00:01+00:00', '2026-10-04T09:00:05+00:00', 50, 50, 1, 1, 0, 0, NULL, 1, 0),
    (3, 'UCfakeAccountTwo00000002', 'create_playlist', '{}', 'waiting_quota', 2, 1, 1, '2026-10-06T07:00:00+00:00', '2026-10-05T12:00:00+00:00', NULL, NULL, 50, 0, 0, 0, 0, 0, 'quotaExceeded', 0, 0);

INSERT INTO job_items (id, job_id, seq, phase, action, params_json, status, api_result_json, inverse_json, attempts, last_error, updated_at) VALUES
    (1, 1, 0, 1, 'insert_item', '{"video_id":"vidAAAAAAA1"}', 'pending', NULL, NULL, 0, NULL, '2026-10-05T09:00:00+00:00'),
    (2, 1, 1, 1, 'insert_item', '{"video_id":"vidDDDDDDD4"}', 'pending', NULL, NULL, 0, NULL, '2026-10-05T09:00:00+00:00'),
    (3, 2, 0, 1, 'delete_item', '{"playlist_item_id":"PIoneZ"}', 'done', '{}', '{"action":"insert_item"}', 1, NULL, '2026-10-04T09:00:05+00:00');

-- The test's "now" is 2026-10-05T18:00:00Z: the Pacific day runs 2026-10-05T07:00Z..2026-10-06T07:00Z.
INSERT INTO quota_ledger (id, ts, endpoint, units, account_id, job_id) VALUES
    (1, '2026-10-05T06:59:59+00:00', 'playlistItems.list', 1, 'UCfakeAccountOne00000001', NULL),
    (2, '2026-10-05T07:00:00+00:00', 'playlistItems.insert', 50, 'UCfakeAccountOne00000001', 1),
    (3, '2026-10-05T17:30:00.5+00:00', 'videos.list', 1, NULL, NULL),
    (4, '2026-10-06T07:00:00+00:00', 'playlists.list', 1, 'UCfakeAccountTwo00000002', NULL);

INSERT INTO runner_lock (id, pid, heartbeat_at) VALUES (1, 4242, '2026-10-05T10:00:00+00:00');

INSERT INTO monitor_runs (started_at, finished_at, trigger, outcome, accounts_ok, units_spent) VALUES
    ('2026-10-05T10:00:00+00:00', '2026-10-05T10:00:05+00:00', 'headless', 'ok', 1, 3);

INSERT INTO downloads (video_id, format, status, requested_quality, thumbnail_mode, dest_dir, file_path, file_size_bytes, container, error, attempts, created_at, completed_at, last_verified_at) VALUES
    ('vidAAAAAAA1', 'audio', 'available', 'best', 'embed', 'C:\Users\fake\Downloads\PlaylistForge', 'C:\Users\fake\Downloads\PlaylistForge\Fake Song One.m4a', 4096, 'm4a', NULL, 1, '2026-10-02T10:00:00+00:00', '2026-10-02T10:01:00+00:00', NULL),
    ('vidDDDDDDD4', 'video', 'error', '720', 'none', 'C:\Users\fake\Downloads\PlaylistForge', NULL, NULL, NULL, 'HTTP 403', 3, '2026-10-03T10:00:00+00:00', NULL, NULL);

INSERT INTO settings (key, value) VALUES
    ('ui.drop_mode', 'move'),
    ('ui.drop_dup_policy', 'consolidate'),
    ('ui.playlists_view', 'list'),
    ('ui.recent_drop_dests', '["PLfakeOne"]'),
    ('retention_keep_last', '12'),
    ('downloads.dir', 'D:\Music\Fake'),
    ('downloads.default_format', 'audio'),
    ('downloads.audio_quality', 'best'),
    ('downloads.cookies_browser', 'firefox'),
    ('budget.safety_margin_percent', '5'),
    ('budget.backup_reserve_units', '600'),
    ('jobs.default_job_priority', '1'),
    ('jobs.local_echo_max_age_s', '3600'),
    ('monitor.schedule_time', '09:00');
