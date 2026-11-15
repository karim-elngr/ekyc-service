-- One obligation per original screening. The protected receipt contains its immutable
-- evidence binding, the linked replacement and any attributable reviewer disposition.
CREATE TABLE strict_screen_obligation (
    case_id text NOT NULL REFERENCES product_case(case_id) DEFERRABLE INITIALLY DEFERRED,
    original_operation text PRIMARY KEY,
    replacement_operation text UNIQUE,
    status text NOT NULL CHECK(status IN ('outstanding','clear','hit','not_accepted','inconclusive')),
    assigned_to text,
    assign_at timestamptz NOT NULL,
    receipt jsonb NOT NULL
);
CREATE INDEX strict_screen_case ON strict_screen_obligation(case_id);
CREATE INDEX strict_screen_outstanding ON strict_screen_obligation(assigned_to, assign_at)
    WHERE status='outstanding';

ALTER VIEW staff_current_work RENAME TO staff_legacy_work;
CREATE VIEW staff_current_work AS
SELECT w.* FROM staff_legacy_work w WHERE NOT EXISTS (
    SELECT FROM strict_screen_obligation h WHERE h.case_id=w.case_id AND h.original_operation=w.operation_id
)
UNION ALL
SELECT 'review:' || h.original_operation, c.case_id, c.institution, c.product, c.state,
       'screening_review', h.original_operation, COALESCE((h.receipt->>'received_at')::timestamptz,h.assign_at)
FROM strict_screen_obligation h JOIN product_case c USING(case_id)
WHERE h.assigned_to IS NOT NULL AND h.status IN ('outstanding','hit','inconclusive')
  AND h.receipt->>'review' IS NULL;

-- Defend the projection and outbox even if an approval writer bypasses domain evaluation.
CREATE FUNCTION ekyc_guard_screening_approval() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state='APPROVED' AND EXISTS (
        SELECT FROM strict_screen_obligation h WHERE h.case_id=NEW.case_id AND
        (h.status='outstanding' OR (h.status IN ('hit','inconclusive') AND
         (h.receipt #>> '{review,action}') IS DISTINCT FROM 'clear'))
    ) THEN
        RAISE EXCEPTION 'unresolved strict screening obligation' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER product_screening_approval BEFORE INSERT OR UPDATE ON product_case
    FOR EACH ROW EXECUTE FUNCTION ekyc_guard_screening_approval();
