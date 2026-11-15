-- Preserve every queued payload and its lease/delivery metadata while retiring
-- the two v1 projections. The relay drains existing rows from the same outbox.
ALTER TABLE decision_outbox DROP COLUMN case_id;
ALTER TABLE decision_outbox ADD CONSTRAINT outbox_case_or_historical_event
    CHECK (product_case_id IS NOT NULL OR payload->>'event_type' IS NOT DISTINCT FROM 'ekyc.case.decided.v1');
DROP TABLE case_revision;
DROP TABLE case_view;
DROP VIEW product_check_totals;

ALTER TABLE product_case
    ADD COLUMN owner_id text COLLATE "C" GENERATED ALWAYS AS
        (substring(case_id COLLATE "C" FROM '^v2~([A-Za-z0-9-]{1,48})~[0-9a-f]{64}$')) STORED,
    ADD COLUMN created_at timestamptz;
UPDATE product_case SET created_at = (public_view->>'created_at')::timestamptz;
ALTER TABLE product_case ALTER COLUMN created_at SET NOT NULL;

CREATE INDEX case_institution_created ON product_case (institution, created_at, case_id);
CREATE INDEX case_product_created ON product_case (institution, product, created_at, case_id);
CREATE INDEX case_owner_created ON product_case (institution, product, owner_id, created_at, case_id);
CREATE INDEX case_alerts ON decision_outbox (product_case_id, created_at, event_id)
    WHERE payload->>'event_type' = 'ekyc.screening.alert.v2';

-- New revisions pin their public representation. Earlier revisions retain
-- their complete original assessment and are rendered with the same public projection.
ALTER TABLE product_case_revision ADD COLUMN public_view jsonb;
CREATE INDEX case_revision_recorded ON product_case_revision (case_id, recorded_at, version);
