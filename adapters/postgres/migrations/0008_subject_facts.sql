-- Subject-scoped serialization of relevant fact publications and reused approvals.
CREATE TABLE subject_fact_generation (
    institution text NOT NULL, subject_ref text NOT NULL,
    generation bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (institution, subject_ref)
);
ALTER TABLE product_case_revision ADD COLUMN fact_generation bigint;
ALTER TABLE product_fact ADD COLUMN observed_at timestamptz;
UPDATE product_fact SET observed_at = (fact->>'observed_at')::timestamptz;
ALTER TABLE product_fact ALTER COLUMN observed_at SET NOT NULL;

CREATE FUNCTION ekyc_fact_time() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.observed_at := (NEW.fact->>'observed_at')::timestamptz;
    RETURN NEW;
END $$;
CREATE TRIGGER fact_time BEFORE INSERT ON product_fact
    FOR EACH ROW EXECUTE FUNCTION ekyc_fact_time();

CREATE INDEX product_fact_reuse ON product_fact
    (institution,subject_ref,(fact#>>'{evidence,kind}'),(fact#>>'{evidence,document_revision}'),observed_at DESC,operation_id DESC);

INSERT INTO subject_fact_generation(institution,subject_ref,generation)
    SELECT institution,subject_ref,count(*) FROM product_fact
    WHERE fact#>>'{evidence,kind}' IN ('document','face') GROUP BY institution,subject_ref;

CREATE FUNCTION ekyc_advance_fact_generation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO subject_fact_generation(institution,subject_ref,generation)
    VALUES(NEW.institution,NEW.subject_ref,1)
    ON CONFLICT(institution,subject_ref) DO UPDATE SET generation=subject_fact_generation.generation+1;
    RETURN NULL;
END $$;
CREATE TRIGGER advance_subject_generation AFTER INSERT ON product_fact
    FOR EACH ROW WHEN (NEW.fact#>>'{evidence,kind}' IN ('document','face'))
    EXECUTE FUNCTION ekyc_advance_fact_generation();
