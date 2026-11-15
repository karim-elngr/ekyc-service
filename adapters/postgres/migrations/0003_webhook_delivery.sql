-- The monotonically increasing claim_count is the fencing token. A worker
-- whose lease was reclaimed cannot acknowledge or reschedule the new claim.
ALTER TABLE decision_outbox
    ADD COLUMN claim_count bigint NOT NULL DEFAULT 0 CHECK (claim_count >= 0),
    ADD COLUMN claim_until timestamptz,
    ADD COLUMN next_attempt_at timestamptz NOT NULL DEFAULT now(),
    ADD COLUMN delivered_at timestamptz;

CREATE INDEX decision_outbox_due ON decision_outbox (next_attempt_at, created_at)
    WHERE delivered_at IS NULL;
