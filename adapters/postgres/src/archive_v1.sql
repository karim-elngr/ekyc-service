-- Take a lossless image of the actual installed v1 schema, including optional
-- columns from later v1 migrations. A namespace collision stops the upgrade.
CREATE SCHEMA ekyc_v1_archive;
CREATE TABLE ekyc_v1_archive.case_view AS TABLE case_view;
CREATE TABLE ekyc_v1_archive.case_revision AS TABLE case_revision;
ALTER TABLE ekyc_v1_archive.case_view ADD PRIMARY KEY (case_id);
ALTER TABLE ekyc_v1_archive.case_revision ADD PRIMARY KEY (case_id,version);
DO $$
BEGIN
    IF EXISTS (TABLE case_view EXCEPT ALL TABLE ekyc_v1_archive.case_view)
       OR EXISTS (TABLE ekyc_v1_archive.case_view EXCEPT ALL TABLE case_view)
       OR EXISTS (TABLE case_revision EXCEPT ALL TABLE ekyc_v1_archive.case_revision)
       OR EXISTS (TABLE ekyc_v1_archive.case_revision EXCEPT ALL TABLE case_revision)
    THEN
        RAISE EXCEPTION 'v1 audit archive does not match its source';
    END IF;
END $$;
