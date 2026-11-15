# Upgrade, restore and decision-time evidence reuse

## Before starting a new deployment

Drain v1 commands and provider invocations. Record the old Restate deployment ID and the last delivered v1 event. Either drain outstanding v1 envelopes or verify the receiver can accept their original payloads and deduplicate their event IDs. Give the new worker a distinct deployment URI. Keep any previous deployment available until its pinned invocations finish. Test the upgrade on a copy made with the deployment’s PostgreSQL backup procedure.

Use the API or worker startup migrator for this cutover. It takes the same deployment lock as SQLx and holds exclusive locks on both legacy tables until the archive and migrations commit. A direct SQLx CLI migration or manual execution of 0007 bypasses the archive step.

When populated v1 tables exist, startup copies all columns and rows into `ekyc_v1_archive.case_view` and `ekyc_v1_archive.case_revision`, preserves the case and version keys, and checks full row equality in both directions before 0007 removes the old tables. A namespace collision, insufficient permission, storage failure or comparison failure aborts the transaction. Budget space and downtime for both copies and the equality check. An empty installation proceeds without an archive. Existing 0007 checksums are unchanged.

The service never writes new decisions to this archive. Restrict its database access and back it up for the institution’s retention period. The archive remains in the same database until an independent copy is taken.

## Rehearse a restore

Use a separate empty database for the rehearsal. The first variable selects the migrated source, and the second selects the disposable restore target. The following commands use PostgreSQL client tools.

```sh
pg_dump --format=custom --schema=ekyc_v1_archive --file=v1-audit.pgdump "$DATABASE_URL"
pg_restore --exit-on-error --no-owner --no-acl --dbname="$RESTORE_DATABASE_URL" v1-audit.pgdump
```

Choose a known case and compare the checksums of its current view and every revision, including recorded times and snapshots, between source and restore. Save the two results with the backup record.

```sh
for db in "$DATABASE_URL" "$RESTORE_DATABASE_URL"; do
  psql "$db" -X --set=ON_ERROR_STOP=1 --set=case_id="$SAMPLE_V1_CASE_ID" <<'SQL'
SELECT case_id, version, md5(to_jsonb(v)::text) AS view_checksum
FROM ekyc_v1_archive.case_view v WHERE case_id = :'case_id';
SELECT version, recorded_at, md5(to_jsonb(r)::text) AS revision_checksum
FROM ekyc_v1_archive.case_revision r WHERE case_id = :'case_id' ORDER BY version;
SQL
done
```

For an installation that already ran 0007, check for an existing archive or pre-cutover backup and perform the same restore rehearsal. Its tables in an older backup may live in `public`. Starting this version cannot reconstruct v1 history removed before the new guard ran. Use the backup to preserve historical decisions separately from v2 reusable evidence.

## Reused approvals

The indexed lookup restricts facts by institution, subject, check kind, current document, permitted source products and methods, and the policy’s freshness window. It reads qualifying rows in pages of 64. Rust applies the same binding and eligibility rules as the case engine to original timestamps, including ties. Opening selects at most one fact per reusable kind. Revalidation returns a pinned original and at most one contradictory observation per kind. Within the active window, a long run of plausible newer passes can require additional pages to establish that no contrary observation exists.

Every insert of a new document or face fact advances the subject’s generation in the same transaction. Before publishing an approval that uses a historical fact, the writer locks the generation row, evaluates the pinned originals against the committed facts and policy at the database clock, then publishes the effective assessment, audit revision and event together. Another publisher for that subject must serialize its fact insertion against that boundary. The assessed generation is recorded in `product_case_revision.fact_generation`. A lost commit response can be replayed using the persisted assessment. Restate schedules work from that effective assessment. A retained historical decision is restored as originally issued.

The issuer snapshot and the PostgreSQL fact generation have separate boundaries. The worker resolves the current issuer binding before applying the case transition. Changes in the external issuer after that lookup require a subsequent check or monitoring workflow. An issuer integration needing atomic revocation must supply its own fencing or revocation protocol.

## Regression checks

Run the database suite on a disposable PostgreSQL database with permission to create temporary test databases. These tests cover populated concurrent startup, full archived revision equality, atomic contention, replay after a lost acknowledgement, and long-history selection with a same-time adverse outcome across multiple pages.

```sh
cargo test --locked -p ekyc-postgres --features postgres-tests
cargo test --locked -p ekyc-restate issuer_method_drift
```

The native harness also runs `scripts/binding_drift_smoke.py` against Scenario lab’s interactive provider. It changes the issuer method during a pending screen, delivers a hit, verifies its original alert, restores the method, and completes the same case through a fresh operation.
