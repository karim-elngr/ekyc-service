--
-- PostgreSQL database dump
--



SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: public; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA public;


--
-- Name: ekyc_acquire_provider(text, text, bigint, bigint, bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_acquire_provider(p_provider text, p_token text, p_per_minute bigint, p_max_in_flight bigint, p_lease_ms bigint) RETURNS bigint
    LANGUAGE plpgsql
    AS $$
DECLARE
    gate provider_gate%ROWTYPE;
    prior provider_lease%ROWTYPE;
    now_at timestamptz;
    active_count bigint;
    first_expiry timestamptz;
    active_probe_expiry timestamptz;
    recent_count bigint;
    oldest_dispatch timestamptz;
BEGIN
    INSERT INTO provider_gate (provider) VALUES (p_provider) ON CONFLICT DO NOTHING;
    SELECT * INTO STRICT gate FROM provider_gate WHERE provider = p_provider FOR UPDATE;
    now_at := clock_timestamp();
    SELECT * INTO prior FROM provider_lease WHERE provider = p_provider AND token = p_token;
    IF FOUND THEN
        IF prior.finished_at IS NULL AND prior.expires_at > now_at THEN
            RETURN ekyc_wait_ms(now_at, prior.expires_at);
        END IF;
        RETURN -1;
    END IF;

    IF gate.throttle_until > now_at OR gate.open_until > now_at THEN
        RETURN ekyc_wait_ms(now_at, greatest(gate.throttle_until, gate.open_until));
    END IF;

    SELECT count(*), min(expires_at), min(expires_at) FILTER (WHERE probe)
        INTO active_count, first_expiry, active_probe_expiry
        FROM provider_lease
        WHERE provider = p_provider AND finished_at IS NULL AND expires_at > now_at;
    -- When the cooldown elapses, exactly one probe can be in flight.
    IF gate.open_until IS NOT NULL AND active_probe_expiry IS NOT NULL THEN
        RETURN least(250, ekyc_wait_ms(now_at, active_probe_expiry));
    END IF;
    IF active_count >= p_max_in_flight THEN
        RETURN least(250, ekyc_wait_ms(now_at, first_expiry));
    END IF;

    SELECT count(*), min(started_at) INTO recent_count, oldest_dispatch FROM provider_lease
        WHERE provider = p_provider AND started_at > now_at - interval '1 minute';
    IF recent_count >= p_per_minute THEN
        RETURN ekyc_wait_ms(now_at, oldest_dispatch + interval '1 minute');
    END IF;

    INSERT INTO provider_lease (provider, token, generation, probe, started_at, expires_at)
        VALUES (p_provider, p_token, gate.generation, gate.open_until IS NOT NULL,
                now_at, now_at + p_lease_ms * interval '1 millisecond');
    RETURN 0;
END;
$$;


--
-- Name: ekyc_admit(text, bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_admit(p_caller text, p_quota bigint) RETURNS bigint
    LANGUAGE plpgsql
    AS $$
DECLARE
    now_at timestamptz;
    bucket_start timestamptz;
    current_quota caller_quota%ROWTYPE;
BEGIN
    INSERT INTO caller_quota (caller, bucket, used) VALUES (p_caller, '-infinity', 0)
        ON CONFLICT DO NOTHING;
    SELECT * INTO STRICT current_quota FROM caller_quota WHERE caller = p_caller FOR UPDATE;
    now_at := clock_timestamp();
    bucket_start := date_trunc('minute', now_at);
    IF current_quota.bucket = bucket_start AND current_quota.used >= p_quota THEN
        RETURN ceil(extract(epoch FROM bucket_start + interval '1 minute' - now_at) * 1000)::bigint;
    END IF;
    UPDATE caller_quota SET bucket = bucket_start,
        used = CASE WHEN bucket = bucket_start THEN used + 1 ELSE 1 END
        WHERE caller = p_caller;
    RETURN 0;
END;
$$;


--
-- Name: ekyc_advance_fact_generation(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_advance_fact_generation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO subject_fact_generation(institution,subject_ref,generation)
    VALUES(NEW.institution,NEW.subject_ref,1)
    ON CONFLICT(institution,subject_ref) DO UPDATE SET generation=subject_fact_generation.generation+1;
    RETURN NULL;
END $$;


--
-- Name: ekyc_fact_time(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_fact_time() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.observed_at := (NEW.fact->>'observed_at')::timestamptz;
    RETURN NEW;
END $$;


--
-- Name: ekyc_finish_provider(text, text, boolean, bigint, bigint, bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_finish_provider(p_provider text, p_token text, p_failed boolean, p_threshold bigint, p_cooldown_ms bigint, p_retry_after_ms bigint) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    gate provider_gate%ROWTYPE;
    finished provider_lease%ROWTYPE;
    now_at timestamptz;
BEGIN
    SELECT * INTO gate FROM provider_gate WHERE provider = p_provider FOR UPDATE;
    IF NOT FOUND THEN RETURN; END IF;
    now_at := clock_timestamp();
    UPDATE provider_lease SET finished_at = now_at
        WHERE provider = p_provider AND token = p_token AND finished_at IS NULL
        RETURNING * INTO finished;
    IF NOT FOUND THEN RETURN; END IF;

    -- A late worker or an old in-flight request cannot close a newer circuit.
    IF finished.expires_at <= now_at OR finished.generation <> gate.generation THEN
        RETURN;
    END IF;
    IF p_retry_after_ms > 0 THEN
        UPDATE provider_gate SET throttle_until = greatest(
            throttle_until, now_at + p_retry_after_ms * interval '1 millisecond')
            WHERE provider = p_provider;
    END IF;
    IF p_failed AND (finished.probe OR gate.failures + 1 >= p_threshold) THEN
        UPDATE provider_gate SET failures = 0, generation = generation + 1,
            open_until = now_at + p_cooldown_ms * interval '1 millisecond'
            WHERE provider = p_provider;
    ELSIF p_failed THEN
        UPDATE provider_gate SET failures = failures + 1 WHERE provider = p_provider;
    ELSE
        UPDATE provider_gate SET failures = 0, open_until = NULL WHERE provider = p_provider;
    END IF;
END;
$$;


--
-- Name: ekyc_wait_ms(timestamp with time zone, timestamp with time zone); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_wait_ms(now_at timestamp with time zone, until_at timestamp with time zone) RETURNS bigint
    LANGUAGE sql IMMUTABLE STRICT
    AS $$
    SELECT greatest(1, ceil(extract(epoch FROM until_at - now_at) * 1000)::bigint)
$$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: caller_quota; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.caller_quota (
    caller text NOT NULL,
    bucket timestamp with time zone NOT NULL,
    used bigint NOT NULL
);


--
-- Name: decision_outbox; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.decision_outbox (
    event_id text NOT NULL,
    version bigint NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    claim_count bigint DEFAULT 0 NOT NULL,
    claim_until timestamp with time zone,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL,
    delivered_at timestamp with time zone,
    product_case_id text,
    CONSTRAINT decision_outbox_claim_count_check CHECK ((claim_count >= 0)),
    CONSTRAINT outbox_case_or_historical_event CHECK (((product_case_id IS NOT NULL) OR (NOT ((payload ->> 'event_type'::text) IS DISTINCT FROM 'ekyc.case.decided.v1'::text))))
);


--
-- Name: product_case; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.product_case (
    case_id text NOT NULL,
    institution text NOT NULL,
    product text NOT NULL,
    subject_ref text NOT NULL,
    version bigint NOT NULL,
    state text NOT NULL,
    public_view jsonb NOT NULL,
    owner_id text GENERATED ALWAYS AS ("substring"((case_id COLLATE "C"), '^v2~([A-Za-z0-9-]{1,48})~[0-9a-f]{64}$'::text)) STORED COLLATE pg_catalog."C",
    created_at timestamp with time zone NOT NULL,
    CONSTRAINT product_case_version_check CHECK ((version > 0))
);


--
-- Name: product_case_revision; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.product_case_revision (
    case_id text NOT NULL,
    version bigint NOT NULL,
    recorded_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    assessment jsonb NOT NULL,
    public_view jsonb,
    fact_generation bigint
);


--
-- Name: product_fact; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.product_fact (
    operation_id text NOT NULL,
    origin_case_id text NOT NULL,
    institution text NOT NULL,
    subject_ref text NOT NULL,
    fact jsonb NOT NULL,
    observed_at timestamp with time zone NOT NULL
);


--
-- Name: provider_gate; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_gate (
    provider text NOT NULL,
    generation bigint DEFAULT 0 NOT NULL,
    failures integer DEFAULT 0 NOT NULL,
    open_until timestamp with time zone,
    throttle_until timestamp with time zone
);


--
-- Name: provider_lease; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_lease (
    provider text NOT NULL,
    token text NOT NULL,
    generation bigint NOT NULL,
    probe boolean NOT NULL,
    started_at timestamp with time zone NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    finished_at timestamp with time zone
);


--
-- Name: staff_current_work; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.staff_current_work AS
 SELECT ('review:'::text || (check_item.value ->> 'operation_id'::text)) AS work_id,
    c.case_id,
    c.institution,
    c.product,
    c.state,
    'screening_review'::text AS kind,
    (check_item.value ->> 'operation_id'::text) AS operation_id,
    COALESCE(((check_item.value ->> 'observed_at'::text))::timestamp with time zone, c.created_at) AS opened_at
   FROM (public.product_case c
     CROSS JOIN LATERAL jsonb_array_elements((c.public_view -> 'checks'::text)) check_item(value))
  WHERE ((c.state <> ALL (ARRAY['APPROVED'::text, 'REJECTED'::text])) AND ((c.public_view -> 'checks'::text) @> '[{"kind": "sanctions", "status": "REVIEW"}]'::jsonb) AND ((check_item.value ->> 'kind'::text) = 'sanctions'::text) AND ((check_item.value ->> 'status'::text) = 'REVIEW'::text) AND ((check_item.value ->> 'operation_id'::text) IS NOT NULL))
UNION ALL
 SELECT ('alert:'::text || e.event_id) AS work_id,
    c.case_id,
    c.institution,
    c.product,
    c.state,
    'late_alert'::text AS kind,
    (e.payload ->> 'operation_id'::text) AS operation_id,
    e.created_at AS opened_at
   FROM (public.decision_outbox e
     JOIN public.product_case c ON ((c.case_id = e.product_case_id)))
  WHERE ((e.payload ->> 'event_type'::text) = 'ekyc.screening.alert.v2'::text);


--
-- Name: staff_work_acknowledgement; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.staff_work_acknowledgement (
    work_id text NOT NULL,
    case_id text NOT NULL,
    kind text NOT NULL,
    operation_id text NOT NULL,
    opened_at timestamp with time zone NOT NULL,
    acknowledged_by text NOT NULL,
    acknowledged_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    CONSTRAINT staff_work_acknowledgement_kind_check CHECK ((kind = ANY (ARRAY['screening_review'::text, 'late_alert'::text])))
);


--
-- Name: subject_fact_generation; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.subject_fact_generation (
    institution text NOT NULL,
    subject_ref text NOT NULL,
    generation bigint DEFAULT 0 NOT NULL
);


--
-- Name: caller_quota caller_quota_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.caller_quota
    ADD CONSTRAINT caller_quota_pkey PRIMARY KEY (caller);


--
-- Name: decision_outbox decision_outbox_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.decision_outbox
    ADD CONSTRAINT decision_outbox_pkey PRIMARY KEY (event_id);


--
-- Name: product_case product_case_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.product_case
    ADD CONSTRAINT product_case_pkey PRIMARY KEY (case_id);


--
-- Name: product_case_revision product_case_revision_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.product_case_revision
    ADD CONSTRAINT product_case_revision_pkey PRIMARY KEY (case_id, version);


--
-- Name: product_fact product_fact_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.product_fact
    ADD CONSTRAINT product_fact_pkey PRIMARY KEY (operation_id);


--
-- Name: provider_gate provider_gate_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_gate
    ADD CONSTRAINT provider_gate_pkey PRIMARY KEY (provider);


--
-- Name: provider_lease provider_lease_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_lease
    ADD CONSTRAINT provider_lease_pkey PRIMARY KEY (provider, token);


--
-- Name: staff_work_acknowledgement staff_work_acknowledgement_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_work_acknowledgement
    ADD CONSTRAINT staff_work_acknowledgement_pkey PRIMARY KEY (work_id);


--
-- Name: subject_fact_generation subject_fact_generation_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subject_fact_generation
    ADD CONSTRAINT subject_fact_generation_pkey PRIMARY KEY (institution, subject_ref);


--
-- Name: case_alerts; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX case_alerts ON public.decision_outbox USING btree (product_case_id, created_at, event_id) WHERE ((payload ->> 'event_type'::text) = 'ekyc.screening.alert.v2'::text);


--
-- Name: case_institution_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX case_institution_created ON public.product_case USING btree (institution, created_at, case_id);


--
-- Name: case_owner_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX case_owner_created ON public.product_case USING btree (institution, product, owner_id, created_at, case_id);


--
-- Name: case_product_created; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX case_product_created ON public.product_case USING btree (institution, product, created_at, case_id);


--
-- Name: case_revision_recorded; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX case_revision_recorded ON public.product_case_revision USING btree (case_id, recorded_at, version);


--
-- Name: decision_outbox_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX decision_outbox_due ON public.decision_outbox USING btree (next_attempt_at, created_at) WHERE (delivered_at IS NULL);


--
-- Name: product_fact_reuse; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX product_fact_reuse ON public.product_fact USING btree (institution, subject_ref, ((fact #>> '{evidence,kind}'::text[])), ((fact #>> '{evidence,document_revision}'::text[])), observed_at DESC, operation_id DESC);


--
-- Name: product_fact_subject; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX product_fact_subject ON public.product_fact USING btree (institution, subject_ref);


--
-- Name: product_one_final_event; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX product_one_final_event ON public.decision_outbox USING btree (product_case_id) WHERE ((payload ->> 'event_type'::text) = 'ekyc.case.decided.v2'::text);


--
-- Name: provider_lease_active; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_lease_active ON public.provider_lease USING btree (provider, expires_at) WHERE (finished_at IS NULL);


--
-- Name: provider_lease_rate; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_lease_rate ON public.provider_lease USING btree (provider, started_at);


--
-- Name: staff_ack_case; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_ack_case ON public.staff_work_acknowledgement USING btree (case_id);


--
-- Name: staff_alert_case; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_alert_case ON public.decision_outbox USING btree (product_case_id, event_id) WHERE ((payload ->> 'event_type'::text) = 'ekyc.screening.alert.v2'::text);


--
-- Name: staff_review_scope; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX staff_review_scope ON public.product_case USING btree (institution, product, case_id) WHERE ((public_view -> 'checks'::text) @> '[{"kind": "sanctions", "status": "REVIEW"}]'::jsonb);


--
-- Name: product_fact advance_subject_generation; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER advance_subject_generation AFTER INSERT ON public.product_fact FOR EACH ROW WHEN (((new.fact #>> '{evidence,kind}'::text[]) = ANY (ARRAY['document'::text, 'face'::text]))) EXECUTE FUNCTION public.ekyc_advance_fact_generation();


--
-- Name: product_fact fact_time; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER fact_time BEFORE INSERT ON public.product_fact FOR EACH ROW EXECUTE FUNCTION public.ekyc_fact_time();


--
-- Name: decision_outbox decision_outbox_product_case_id_version_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.decision_outbox
    ADD CONSTRAINT decision_outbox_product_case_id_version_fkey FOREIGN KEY (product_case_id, version) REFERENCES public.product_case_revision(case_id, version);


--
-- Name: product_case_revision product_case_revision_case_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.product_case_revision
    ADD CONSTRAINT product_case_revision_case_id_fkey FOREIGN KEY (case_id) REFERENCES public.product_case(case_id);


--
-- Name: product_fact product_fact_origin_case_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.product_fact
    ADD CONSTRAINT product_fact_origin_case_id_fkey FOREIGN KEY (origin_case_id) REFERENCES public.product_case(case_id);


--
-- Name: provider_lease provider_lease_provider_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_lease
    ADD CONSTRAINT provider_lease_provider_fkey FOREIGN KEY (provider) REFERENCES public.provider_gate(provider);


--
-- Name: staff_work_acknowledgement staff_work_acknowledgement_case_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_work_acknowledgement
    ADD CONSTRAINT staff_work_acknowledgement_case_id_fkey FOREIGN KEY (case_id) REFERENCES public.product_case(case_id);


--
-- PostgreSQL database dump complete
--
