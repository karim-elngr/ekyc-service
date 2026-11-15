-- Only IDs accepted by the single-case API have an attributable caller.
ALTER TABLE case_view
    ADD COLUMN caller_id text COLLATE "C" GENERATED ALWAYS AS
        (substring(case_id COLLATE "C" FROM '^([A-Za-z0-9-]{1,48})~[0-9a-f]{64}$')) STORED,
    ADD COLUMN created_at timestamptz;

UPDATE case_view AS c
SET created_at = (
    SELECT r.recorded_at FROM case_revision AS r
    WHERE r.case_id = c.case_id ORDER BY r.version LIMIT 1
);

-- Existing rows without a revision keep their unknown timestamp. New rows
-- use their first projection insert time, which is preserved on upsert.
ALTER TABLE case_view ALTER COLUMN created_at SET DEFAULT clock_timestamp();
CREATE INDEX case_view_caller_created ON case_view (caller_id, created_at DESC NULLS LAST, case_id);
