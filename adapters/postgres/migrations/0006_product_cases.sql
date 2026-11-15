CREATE TABLE product_case (
    case_id text PRIMARY KEY,
    institution text NOT NULL,
    product text NOT NULL,
    subject_ref text NOT NULL,
    version bigint NOT NULL CHECK (version > 0),
    state text NOT NULL,
    public_view jsonb NOT NULL
);
CREATE TABLE product_case_revision (
    case_id text NOT NULL REFERENCES product_case,
    version bigint NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    assessment jsonb NOT NULL,
    PRIMARY KEY (case_id,version)
);
CREATE TABLE product_fact (
    operation_id text PRIMARY KEY,
    origin_case_id text NOT NULL REFERENCES product_case,
    institution text NOT NULL,
    subject_ref text NOT NULL,
    fact jsonb NOT NULL
);
CREATE INDEX product_fact_subject ON product_fact (institution,subject_ref);
-- v1 events retain both existing foreign keys and their one-row-per-case constraint.
ALTER TABLE decision_outbox ALTER COLUMN case_id DROP NOT NULL;
ALTER TABLE decision_outbox ADD COLUMN product_case_id text,
    ADD CHECK ((case_id IS NULL) <> (product_case_id IS NULL)),
    ADD FOREIGN KEY (product_case_id, version) REFERENCES product_case_revision(case_id, version);
CREATE UNIQUE INDEX product_one_final_event ON decision_outbox(product_case_id)
    WHERE payload->>'event_type' = 'ekyc.case.decided.v2';
CREATE VIEW product_check_totals AS
    SELECT p.institution, p.product, c->>'kind' AS kind, c->>'origin' AS origin,
           c->>'status' AS status, count(*) AS checks
    FROM product_case p CROSS JOIN LATERAL jsonb_array_elements(p.public_view->'checks') c
    GROUP BY 1,2,3,4,5;
