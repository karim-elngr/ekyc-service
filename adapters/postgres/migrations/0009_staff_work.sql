-- Preserve the acknowledgement independently of changes to the case projection.
CREATE TABLE staff_work_acknowledgement (
    work_id text PRIMARY KEY,
    case_id text NOT NULL REFERENCES product_case(case_id),
    kind text NOT NULL CHECK (kind IN ('screening_review','late_alert')),
    operation_id text NOT NULL,
    opened_at timestamptz NOT NULL,
    acknowledged_by text NOT NULL,
    acknowledged_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX staff_ack_case ON staff_work_acknowledgement(case_id);
CREATE INDEX staff_review_scope ON product_case(institution,product,case_id)
    WHERE public_view->'checks' @> '[{"kind":"sanctions","status":"REVIEW"}]';
CREATE INDEX staff_alert_case ON decision_outbox(product_case_id,event_id)
    WHERE payload->>'event_type' = 'ekyc.screening.alert.v2';

CREATE VIEW staff_current_work AS
SELECT 'review:' || (check_item->>'operation_id') AS work_id, c.case_id,
       c.institution, c.product, c.state, 'screening_review'::text AS kind,
       check_item->>'operation_id' AS operation_id,
       COALESCE((check_item->>'observed_at')::timestamptz,c.created_at) AS opened_at
FROM product_case c CROSS JOIN LATERAL jsonb_array_elements(c.public_view->'checks') check_item
WHERE c.state NOT IN ('APPROVED','REJECTED')
  AND c.public_view->'checks' @> '[{"kind":"sanctions","status":"REVIEW"}]'
  AND check_item->>'kind'='sanctions' AND check_item->>'status'='REVIEW'
  AND check_item->>'operation_id' IS NOT NULL
UNION ALL
SELECT 'alert:' || e.event_id, c.case_id, c.institution,c.product,c.state,'late_alert',
       e.payload->>'operation_id', e.created_at
FROM decision_outbox e JOIN product_case c ON c.case_id=e.product_case_id
WHERE e.payload->>'event_type'='ekyc.screening.alert.v2';
