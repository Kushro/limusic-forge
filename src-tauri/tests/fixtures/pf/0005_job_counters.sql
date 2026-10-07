
ALTER TABLE jobs ADD COLUMN planned_items INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN retried_items INTEGER NOT NULL DEFAULT 0;
UPDATE jobs SET planned_items = (
    SELECT COUNT(*) FROM job_items WHERE job_items.job_id = jobs.id AND job_items.phase = 1
);
UPDATE jobs SET retried_items = (
    SELECT COUNT(*) FROM job_items
    WHERE job_items.job_id = jobs.id AND job_items.attempts > 1 AND job_items.action != 'verify_destination'
);
UPDATE jobs SET done_items = (
    SELECT COUNT(*) FROM job_items WHERE job_items.job_id = jobs.id AND job_items.status = 'done'
);
