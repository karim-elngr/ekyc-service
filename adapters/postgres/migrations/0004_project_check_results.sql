-- Backfill v1 read projections from the recorded reasons. Historical revision
-- snapshots remain byte-for-byte unchanged. New writers supply checks directly.
UPDATE case_view AS cv
SET snapshot = jsonb_set(cv.snapshot, '{checks}', (
    SELECT jsonb_agg(jsonb_build_object('kind', part.kind, 'result',
        CASE
            WHEN NOT ((cv.snapshot->'observations') ? part.kind) THEN
                CASE WHEN cv.terminal THEN 'SKIPPED_AFTER_DECISION' ELSE 'PENDING' END
            WHEN NOT cv.terminal THEN 'OBSERVED'
            WHEN EXISTS (
                SELECT 1 FROM jsonb_array_elements(cv.snapshot#>'{decision,reasons}') AS reason
                WHERE reason->>'check' = part.kind AND
                    (part.kind = 'sanctions' OR reason->>'code' = 'DOCUMENT_EXPIRED')
            ) THEN 'FAIL'
            WHEN EXISTS (
                SELECT 1 FROM jsonb_array_elements(cv.snapshot#>'{decision,reasons}') AS reason
                WHERE reason->>'check' = part.kind
            ) THEN 'REVIEW'
            ELSE 'PASS'
        END) ORDER BY part.seq)
    FROM jsonb_array_elements_text(cv.snapshot->'scope') WITH ORDINALITY AS part(kind, seq)
))
WHERE NOT (cv.snapshot ? 'checks') AND cv.snapshot->>'policy_version' = 'v1';
