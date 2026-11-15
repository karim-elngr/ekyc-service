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
-- Name: ekyc_attempt_wait(text, timestamp with time zone); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_attempt_wait(reason text, until_at timestamp with time zone) RETURNS jsonb
    LANGUAGE sql
    AS $$
    SELECT jsonb_build_object('state','wait','reason',reason,'retry_ms',
        greatest(1,ceil(extract(epoch FROM until_at-clock_timestamp())*1000)::bigint))
$$;


--
-- Name: ekyc_authorize_attempt(text, text, text, text, text, text, text, bigint, integer); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_authorize_attempt(p_token text, p_case text, p_op text, p_kind text, p_provider text, p_pool text, p_lane text, p_deadline bigint, p_max integer) RETURNS jsonb
    LANGUAGE plpgsql
    AS $$
DECLARE
    route provider_route_contract; rate provider_rate_bucket; circuit provider_circuit;
    billing provider_billing_account; budget provider_attempt_budget; prior provider_attempt;
    current_revision bigint; checked jsonb; req text := p_op; reason text; at_time timestamptz; period date;
    amount bigint; charged bigint; submitted bigint; bill_unit text; previous_status text;
    window_total integer; window_submits integer; oldest timestamptz; oldest_submit timestamptz;
    probe boolean; routed boolean := p_kind='address' AND p_provider IN ('address-a','address-b');
    strict_screen boolean := p_kind='sanctions' AND p_provider IN ('sanctions-a','sanctions-b');
BEGIN
    -- Serialize with revision publication and all authorizations for this case.
    -- Lock before reading the revision. READ COMMITTED refetches a changed case after
    -- a lock wait; a join in that same statement cannot see its freshly inserted revision.
    SELECT version INTO current_revision FROM product_case WHERE case_id=p_case FOR UPDATE;
    IF NOT FOUND THEN RETURN jsonb_build_object('state','blocked','reason','missing_case'); END IF;
    SELECT assessment->'checks'->p_kind INTO STRICT checked FROM product_case_revision
        WHERE case_id=p_case AND version=current_revision;
    SELECT * INTO prior FROM provider_attempt WHERE token=p_token;
    IF FOUND THEN
        IF (prior.case_id,prior.operation_id,prior.provider,prior.lane) IS DISTINCT FROM (p_case,p_op,p_provider,p_lane)
            THEN RAISE EXCEPTION 'attempt identity mismatch'; END IF;
        IF prior.result IS NOT NULL THEN RETURN jsonb_build_object('state','saved','result',prior.result); END IF;
        RETURN jsonb_build_object('state','unknown');
    END IF;

    SELECT * INTO STRICT route FROM provider_route_contract WHERE kind=p_kind AND provider=p_provider FOR SHARE;
    IF p_pool <> route.pool OR p_lane NOT IN ('lookup','submit') THEN
        RETURN jsonb_build_object('state','blocked','reason','contract_mismatch');
    END IF;
    IF strict_screen THEN routed := true; END IF;
    IF routed THEN
        SELECT requirement_id INTO req FROM provider_route_selection WHERE operation_id=p_op AND case_id=p_case AND provider=p_provider;
        IF req IS NULL THEN RETURN jsonb_build_object('state','blocked','reason','unassigned'); END IF;
        reason := ekyc_check_route(p_op,p_lane,false);
        IF reason IS NOT NULL THEN RETURN jsonb_build_object('state','blocked','reason',reason); END IF;
    END IF;
    IF strict_screen THEN
        PERFORM 1 FROM strict_screen_obligation WHERE case_id=p_case
            AND (original_operation=p_op OR replacement_operation=p_op)
            AND (p_lane='lookup' OR p_provider='sanctions-b' OR status='outstanding') FOR UPDATE;
        IF NOT FOUND THEN RETURN jsonb_build_object('state','blocked','reason','missing_obligation'); END IF;
    END IF;
    IF NOT routed AND p_lane='submit' AND
        (checked->>'status',checked->>'operation_id',checked#>>'{evidence,provider}')
        IS DISTINCT FROM ('PENDING',p_op,p_provider) THEN
        RETURN jsonb_build_object('state','blocked','reason','superseded');
    END IF;
    IF p_lane='lookup' AND NOT EXISTS(SELECT FROM provider_attempt WHERE operation_id=p_op AND lane='submit'
        AND class <> 'expired') THEN RETURN jsonb_build_object('state','blocked','reason','unsent'); END IF;

    INSERT INTO provider_attempt_budget(requirement_id,deadline_ms,max_attempts) VALUES(req,p_deadline,p_max) ON CONFLICT DO NOTHING;
    SELECT * INTO STRICT budget FROM provider_attempt_budget WHERE requirement_id=req FOR UPDATE;
    -- Every acquisition and completion locks shared contract rows in this order.
    SELECT * INTO STRICT rate FROM provider_rate_bucket WHERE bucket=route.rate_bucket FOR UPDATE;
    SELECT * INTO STRICT circuit FROM provider_circuit WHERE breaker_key=route.breaker_key FOR UPDATE;
    SELECT * INTO STRICT billing FROM provider_billing_account WHERE account=route.billing_account FOR UPDATE;
    IF routed AND p_lane='submit' THEN
        -- A competing probe can close the circuit between selection and this lock.
        reason := ekyc_check_route(p_op,p_lane,false);
        IF reason IS NOT NULL THEN RETURN jsonb_build_object('state','blocked','reason',reason); END IF;
    END IF;
    at_time := clock_timestamp();
    IF p_lane='submit' AND extract(epoch FROM at_time)*1000 >= least(p_deadline,budget.deadline_ms) THEN
        RETURN jsonb_build_object('state','blocked','reason','deadline');
    END IF;
    IF (p_lane='submit' AND budget.submissions >= budget.max_attempts) OR
       (p_lane='lookup' AND budget.lookups >= budget.max_lookups) THEN
        RETURN jsonb_build_object('state','blocked','reason','attempt_budget');
    END IF;
    IF rate.throttled_until > at_time THEN RETURN ekyc_attempt_wait('vendor_throttle',rate.throttled_until); END IF;
    SELECT count(*), count(*) FILTER (WHERE lane='submit'),min(started_at),min(started_at) FILTER (WHERE lane='submit')
        INTO window_total,window_submits,oldest,oldest_submit FROM provider_attempt WHERE rate_bucket=rate.bucket
        AND started_at > at_time - interval '60 seconds';
    IF window_total >= rate.per_minute THEN RETURN ekyc_attempt_wait('rate',oldest+interval '60 seconds'); END IF;
    IF p_lane='submit' AND window_submits >= rate.per_minute-rate.lookup_reserve THEN
        RETURN ekyc_attempt_wait('submit_rate',oldest_submit+interval '60 seconds');
    END IF;
    -- Retrieval is independent of a submit outage on the same upstream account.
    probe := p_lane='submit' AND circuit.open_until IS NOT NULL;
    IF probe AND greatest(circuit.open_until,circuit.probe_until)>at_time THEN
        RETURN ekyc_attempt_wait('breaker_open',greatest(circuit.open_until,circuit.probe_until));
    END IF;
    period := date_trunc('month', at_time AT TIME ZONE billing.timezone)::date;
    amount := CASE WHEN p_lane='submit' THEN route.submit_cost ELSE route.lookup_cost END;
    bill_unit := CASE WHEN route.billing_unit='operation' AND p_lane='submit' THEN 'op/'||p_op ELSE 'request/'||p_token END;
    SELECT status INTO previous_status FROM provider_spend_reservation WHERE account=billing.account AND unit=bill_unit;
    IF previous_status IS NULL OR previous_status='released' THEN
        SELECT coalesce(sum(cost_minor),0),coalesce(sum(cost_minor) FILTER (WHERE lane='submit'),0)
            INTO charged,submitted FROM provider_spend_reservation WHERE account=billing.account
            AND period_start=period AND status <> 'released';
        IF amount > billing.monthly_ceiling-charged OR
            (p_lane='submit' AND amount > billing.monthly_ceiling-billing.lookup_reserve-submitted) THEN
            RETURN ekyc_attempt_wait('spend_ceiling',(period+interval '1 month') AT TIME ZONE billing.timezone);
        END IF;
        INSERT INTO provider_spend_reservation VALUES(billing.account,bill_unit,p_op,period,billing.currency,amount,
            'uncertain',route.tariff_revision,p_lane) ON CONFLICT(account,unit) DO UPDATE SET
            status='uncertain',period_start=excluded.period_start,cost_minor=excluded.cost_minor,tariff_revision=excluded.tariff_revision;
    END IF;
    IF routed THEN
        reason := ekyc_check_route(p_op,p_lane,true);
        IF reason IS NOT NULL THEN RAISE EXCEPTION 'route changed under lock: %',reason; END IF;
    END IF;
    IF probe THEN UPDATE provider_circuit SET probe_token=p_token,probe_until=at_time+route.probe_ms*interval '1 ms'
        WHERE breaker_key=circuit.breaker_key; END IF;
    UPDATE provider_attempt_budget SET submissions=submissions+(p_lane='submit')::integer,
        lookups=lookups+(p_lane='lookup')::integer WHERE requirement_id=req;
    INSERT INTO provider_attempt(token,operation_id,case_id,requirement_id,kind,provider,lane,started_at,class,
        rate_bucket,breaker_key,generation,probe,billing_account,billable_unit) VALUES(p_token,p_op,p_case,req,p_kind,p_provider,
        p_lane,at_time,'possible_send',rate.bucket,circuit.breaker_key,circuit.generation,probe,billing.account,bill_unit);
    RETURN jsonb_build_object('state','send','deadline_ms',least(p_deadline,budget.deadline_ms));
END $$;


--
-- Name: ekyc_check_route(text, text, boolean); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_check_route(p_operation text, p_lane text, p_possible_send boolean) RETURNS text
    LANGUAGE plpgsql
    AS $$
DECLARE
    route provider_route_selection%ROWTYPE;
    active_operation text;
    override_mode text;
    cap integer;
    overridden_at timestamptz;
    position integer;
    probe_eligible boolean := false;
    current_generation bigint;
    current_open timestamptz;
    claimed_until timestamptz;
    expected_generation bigint;
    ordinary_share integer;
BEGIN
    SELECT * INTO route FROM provider_route_selection WHERE operation_id=p_operation FOR UPDATE;
    IF NOT FOUND THEN RETURN 'unassigned'; END IF;
    IF p_lane = 'lookup' THEN
        IF route.possible_send AND route.status='outstanding' THEN RETURN NULL; END IF;
        RETURN 'no_outstanding_operation';
    END IF;
    IF p_lane != 'submit' THEN RETURN 'invalid_lane'; END IF;
    IF route.retired OR route.status NOT IN ('assigned','outstanding') THEN RETURN 'retired'; END IF;
    IF route.deadline_ms <= extract(epoch FROM clock_timestamp())*1000 THEN RETURN 'deadline'; END IF;
    SELECT r.assessment #>> ARRAY['checks',route.kind,'operation_id'] INTO active_operation
      FROM product_case c JOIN product_case_revision r USING(case_id,version)
      WHERE c.case_id=route.case_id AND c.state NOT IN ('APPROVED','REJECTED')
        AND r.assessment #>> ARRAY['checks',route.kind,'status'] = 'PENDING'
        AND r.assessment #>> ARRAY['checks',route.kind,'evidence','revision'] = route.evidence_revision;
    IF active_operation IS DISTINCT FROM p_operation THEN RETURN 'retired'; END IF;
    PERFORM pg_advisory_xact_lock(hashtextextended(jsonb_build_array(route.institution,route.product,route.provider)::text,729));
    SELECT mode,share_cap,changed_at INTO override_mode,cap,overridden_at
      FROM provider_routing_override WHERE institution=route.institution AND product=route.product
      AND provider=route.provider AND expires_at>clock_timestamp() FOR SHARE;
    IF override_mode='paused' THEN RETURN 'paused'; END IF;
    -- Draining keeps previously committed work. A selection made after the override
    -- had to wait because all eligible alternatives were restricted.
    IF override_mode='draining' AND overridden_at<=route.created_at THEN RETURN 'draining'; END IF;
    position := CASE WHEN route.provider='address-a' THEN route.bucket ELSE 99-route.bucket END;
    IF route.reason='recovery_probe' THEN
        SELECT b.generation,b.open_until,b.probe_until INTO current_generation,current_open,claimed_until
            FROM provider_route_contract contract JOIN provider_circuit b USING(breaker_key)
            WHERE contract.kind=route.kind AND contract.provider=route.provider;
        expected_generation := (route.placement_snapshot #>> ARRAY[route.provider,'probe_generation'])::bigint;
        probe_eligible := coalesce(current_generation=expected_generation AND current_open<=clock_timestamp()
            AND (claimed_until IS NULL OR claimed_until<=clock_timestamp()),false);
        IF NOT probe_eligible AND current_open IS NOT NULL AND current_generation IS DISTINCT FROM expected_generation THEN
            RETURN 'share_cap';
        END IF;
    END IF;
    -- Several placements may compete for the same half-open slot. Only a probe
    -- that can actually claim this generation may bypass ordinary share. The
    -- caller repeats this check under the circuit lock, before any debit.
    IF NOT probe_eligible THEN
        IF cap IS NOT NULL AND position>=cap THEN RETURN 'share_cap'; END IF;
        IF route.reason='recovery_probe' THEN
            ordinary_share := (route.placement_snapshot #>> ARRAY[route.provider,'ordinary_share'])::integer;
            IF position>=coalesce(ordinary_share,0) THEN RETURN 'share_cap'; END IF;
        END IF;
        IF route.reason IN ('recovery_probe','waiting_for_eligible_route') AND route.provider='address-a' AND EXISTS(
            SELECT FROM provider_route_health WHERE institution=route.institution AND product=route.product
             AND provider=route.provider AND stage<80 AND position>=stage) THEN RETURN 'share_cap'; END IF;
    END IF;
    IF p_possible_send THEN
        UPDATE provider_route_selection SET possible_send=true,status='outstanding'
            WHERE operation_id=p_operation;
    END IF;
    RETURN NULL;
END;
$$;


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
-- Name: ekyc_finish_attempt(text, text, jsonb, bigint, bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_finish_attempt(p_token text, p_class text, p_result jsonb, p_elapsed_ms bigint, p_backoff_ms bigint) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE a provider_attempt; route provider_route_contract; circuit provider_circuit; at_time timestamptz;
BEGIN
    SELECT * INTO STRICT a FROM provider_attempt WHERE token=p_token FOR UPDATE;
    IF a.result IS NOT NULL THEN RETURN; END IF;
    SELECT * INTO STRICT route FROM provider_route_contract WHERE kind=a.kind AND provider=a.provider;
    PERFORM 1 FROM provider_rate_bucket WHERE bucket=a.rate_bucket FOR UPDATE;
    SELECT * INTO STRICT circuit FROM provider_circuit WHERE breaker_key=a.breaker_key FOR UPDATE;
    PERFORM 1 FROM provider_billing_account WHERE account=a.billing_account FOR UPDATE;
    at_time := clock_timestamp();
    UPDATE provider_attempt SET class=p_class,result=p_result,elapsed_ms=p_elapsed_ms,finished_at=at_time WHERE token=p_token;
    IF p_class='throttled' THEN
        UPDATE provider_rate_bucket SET throttled_until=greatest(throttled_until,
            at_time+greatest(1,p_backoff_ms)*interval '1 ms') WHERE bucket=a.rate_bucket;
    END IF;
    IF a.lane='submit' AND circuit.generation=a.generation AND
        ((a.probe AND circuit.probe_token=p_token) OR (NOT a.probe AND circuit.open_until IS NULL)) THEN
        IF p_class='transient' THEN
            IF a.probe OR circuit.failures+1 >= route.failure_threshold THEN
                UPDATE provider_circuit SET generation=generation+1,failures=0,
                    open_until=at_time+route.cooldown_ms*interval '1 ms',probe_token=NULL,probe_until=NULL
                    WHERE breaker_key=a.breaker_key;
            ELSE
                UPDATE provider_circuit SET failures=failures+1 WHERE breaker_key=a.breaker_key;
            END IF;
        ELSIF p_class='ok' THEN
            UPDATE provider_circuit SET failures=0,generation=generation+a.probe::integer,
                open_until=NULL,probe_token=NULL,probe_until=NULL WHERE breaker_key=a.breaker_key;
        END IF;
    END IF;
    IF p_class='ok' THEN
        UPDATE provider_spend_reservation SET status='confirmed' WHERE account=a.billing_account AND unit=a.billable_unit;
        -- A fenced lookup verifies the original operation's billing unit independently
        -- of the charge for this lookup. Individual lost per-request charges need invoices.
        IF a.lane='lookup' AND route.billing_unit='operation' THEN
            IF p_result #>> '{Called,0,Ok,Lookup}' = 'NotAccepted' THEN
                UPDATE provider_spend_reservation SET status='released'
                    WHERE account=a.billing_account AND unit='op/'||a.operation_id AND status='uncertain';
            ELSIF p_result #> '{Called,0,Ok,Lookup,Complete}' IS NOT NULL THEN
                UPDATE provider_spend_reservation SET status='confirmed'
                    WHERE account=a.billing_account AND unit='op/'||a.operation_id AND status='uncertain';
            END IF;
        END IF;
    ELSIF p_class='expired' AND NOT EXISTS (SELECT FROM provider_attempt WHERE billing_account=a.billing_account
        AND billable_unit=a.billable_unit AND token<>p_token AND class<>'expired') THEN
        UPDATE provider_spend_reservation SET status='released' WHERE account=a.billing_account AND unit=a.billable_unit;
    END IF;
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
-- Name: ekyc_guard_screening_approval(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_guard_screening_approval() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: ekyc_reconcile_charge(text, text, text, bigint, text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.ekyc_reconcile_charge(p_account text, p_unit text, p_receipt text, p_amount bigint, p_currency text) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE reservation provider_spend_reservation; previous provider_billing_receipt;
BEGIN
    PERFORM 1 FROM provider_billing_account WHERE account=p_account FOR UPDATE;
    SELECT * INTO STRICT reservation FROM provider_spend_reservation WHERE account=p_account AND unit=p_unit FOR UPDATE;
    IF p_amount<0 OR p_currency<>reservation.currency OR p_amount>reservation.cost_minor THEN
        RAISE EXCEPTION 'receipt differs from authorized currency or ceiling'; END IF;
    SELECT * INTO previous FROM provider_billing_receipt WHERE account=p_account AND receipt_id=p_receipt;
    IF FOUND THEN
        IF (previous.unit,previous.amount_minor,previous.currency) IS DISTINCT FROM (p_unit,p_amount,p_currency)
            THEN RAISE EXCEPTION 'billing receipt identity conflict'; END IF;
        RETURN;
    END IF;
    INSERT INTO provider_billing_receipt(account,receipt_id,unit,amount_minor,currency)
        VALUES(p_account,p_receipt,p_unit,p_amount,p_currency);
    UPDATE provider_spend_reservation SET cost_minor=p_amount,
        status=CASE WHEN p_amount=0 THEN 'released' ELSE 'confirmed' END WHERE account=p_account AND unit=p_unit;
END $$;


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
-- Name: provider_attempt; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_attempt (
    token text NOT NULL,
    operation_id text NOT NULL,
    case_id text NOT NULL,
    requirement_id text NOT NULL,
    kind text NOT NULL,
    provider text NOT NULL,
    lane text NOT NULL,
    started_at timestamp with time zone NOT NULL,
    finished_at timestamp with time zone,
    elapsed_ms bigint,
    class text NOT NULL,
    rate_bucket text NOT NULL,
    breaker_key text NOT NULL,
    generation bigint NOT NULL,
    probe boolean NOT NULL,
    billing_account text NOT NULL,
    billable_unit text NOT NULL,
    result jsonb,
    CONSTRAINT provider_attempt_class_check CHECK ((class = ANY (ARRAY['possible_send'::text, 'ok'::text, 'throttled'::text, 'transient'::text, 'permanent'::text, 'expired'::text]))),
    CONSTRAINT provider_attempt_lane_check CHECK ((lane = ANY (ARRAY['submit'::text, 'lookup'::text])))
);


--
-- Name: provider_attempt_budget; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_attempt_budget (
    requirement_id text NOT NULL,
    deadline_ms bigint NOT NULL,
    max_attempts integer NOT NULL,
    submissions integer DEFAULT 0 NOT NULL,
    lookups integer DEFAULT 0 NOT NULL,
    max_lookups integer DEFAULT 256 NOT NULL,
    CONSTRAINT provider_attempt_budget_max_attempts_check CHECK ((max_attempts > 0)),
    CONSTRAINT provider_attempt_budget_max_lookups_check CHECK ((max_lookups > 0))
);


--
-- Name: provider_billing_account; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_billing_account (
    account text NOT NULL,
    currency text NOT NULL,
    timezone text NOT NULL,
    monthly_ceiling bigint NOT NULL,
    lookup_reserve bigint NOT NULL,
    CONSTRAINT provider_billing_account_check CHECK (((lookup_reserve >= 0) AND (lookup_reserve <= monthly_ceiling))),
    CONSTRAINT provider_billing_account_currency_check CHECK ((currency ~ '^[A-Z]{3}$'::text)),
    CONSTRAINT provider_billing_account_monthly_ceiling_check CHECK ((monthly_ceiling >= 0))
);


--
-- Name: provider_billing_receipt; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_billing_receipt (
    account text NOT NULL,
    receipt_id text NOT NULL,
    unit text NOT NULL,
    amount_minor bigint NOT NULL,
    currency text NOT NULL,
    received_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    CONSTRAINT provider_billing_receipt_amount_minor_check CHECK ((amount_minor >= 0))
);


--
-- Name: provider_circuit; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_circuit (
    breaker_key text NOT NULL,
    generation bigint DEFAULT 0 NOT NULL,
    failures integer DEFAULT 0 NOT NULL,
    open_until timestamp with time zone,
    probe_token text,
    probe_until timestamp with time zone
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
-- Name: provider_override_history; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_override_history (
    institution text NOT NULL,
    product text NOT NULL,
    provider text NOT NULL,
    version bigint NOT NULL,
    command_id text NOT NULL,
    request jsonb NOT NULL,
    previous jsonb,
    effective jsonb NOT NULL
);


--
-- Name: provider_rate_bucket; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_rate_bucket (
    bucket text NOT NULL,
    per_minute integer NOT NULL,
    lookup_reserve integer NOT NULL,
    throttled_until timestamp with time zone,
    CONSTRAINT provider_rate_bucket_check CHECK (((lookup_reserve >= 0) AND (lookup_reserve < per_minute))),
    CONSTRAINT provider_rate_bucket_per_minute_check CHECK ((per_minute > 0))
);


--
-- Name: provider_route_binding; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_route_binding (
    requirement_id text NOT NULL,
    provider text NOT NULL,
    binding jsonb NOT NULL
);


--
-- Name: provider_route_contract; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_route_contract (
    kind text NOT NULL,
    provider text NOT NULL,
    pool text NOT NULL,
    rate_bucket text NOT NULL,
    breaker_key text NOT NULL,
    billing_account text NOT NULL,
    tariff_revision text NOT NULL,
    submit_cost bigint NOT NULL,
    lookup_cost bigint NOT NULL,
    billing_unit text NOT NULL,
    failure_threshold integer NOT NULL,
    cooldown_ms integer NOT NULL,
    probe_ms integer NOT NULL,
    CONSTRAINT provider_route_contract_billing_unit_check CHECK ((billing_unit = ANY (ARRAY['operation'::text, 'request'::text]))),
    CONSTRAINT provider_route_contract_cooldown_ms_check CHECK ((cooldown_ms > 0)),
    CONSTRAINT provider_route_contract_failure_threshold_check CHECK ((failure_threshold > 0)),
    CONSTRAINT provider_route_contract_lookup_cost_check CHECK ((lookup_cost >= 0)),
    CONSTRAINT provider_route_contract_probe_ms_check CHECK ((probe_ms > 0)),
    CONSTRAINT provider_route_contract_submit_cost_check CHECK ((submit_cost >= 0))
);


--
-- Name: provider_route_health; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_route_health (
    institution text NOT NULL,
    product text NOT NULL,
    provider text NOT NULL,
    stage integer DEFAULT 80 NOT NULL,
    cause text DEFAULT 'healthy'::text NOT NULL,
    override_version bigint DEFAULT 0 NOT NULL,
    since timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    samples integer DEFAULT 0 NOT NULL,
    p95_ms double precision,
    error_rate double precision,
    window_end timestamp with time zone,
    CONSTRAINT provider_route_health_stage_check CHECK ((stage = ANY (ARRAY[5, 20, 50, 80])))
);


--
-- Name: provider_route_selection; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_route_selection (
    operation_id text NOT NULL,
    case_id text NOT NULL,
    institution text NOT NULL,
    product text NOT NULL,
    kind text NOT NULL,
    requirement_id text NOT NULL,
    evidence_revision text NOT NULL,
    selection_seq integer NOT NULL,
    policy_version text NOT NULL,
    candidates text[] NOT NULL,
    bucket integer,
    provider text NOT NULL,
    reason text NOT NULL,
    override_version bigint NOT NULL,
    placement_snapshot jsonb NOT NULL,
    block_reason text,
    predecessor text,
    deadline_ms bigint NOT NULL,
    created_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    retired boolean DEFAULT false NOT NULL,
    possible_send boolean DEFAULT false NOT NULL,
    status text DEFAULT 'assigned'::text NOT NULL,
    result jsonb,
    received_at timestamp with time zone,
    observed_at timestamp with time zone,
    owner text DEFAULT 'provider-operations'::text NOT NULL,
    CONSTRAINT provider_route_selection_bucket_check CHECK (((bucket >= 0) AND (bucket <= 99))),
    CONSTRAINT provider_route_selection_status_check CHECK ((status = ANY (ARRAY['assigned'::text, 'outstanding'::text, 'retired'::text, 'complete'::text, 'not_accepted'::text, 'inconclusive'::text, 'blocked'::text])))
);


--
-- Name: provider_routing_override; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_routing_override (
    institution text NOT NULL,
    product text NOT NULL,
    provider text NOT NULL,
    version bigint NOT NULL,
    mode text NOT NULL,
    share_cap integer,
    expires_at timestamp with time zone NOT NULL,
    actor text NOT NULL,
    reason text NOT NULL,
    changed_at timestamp with time zone DEFAULT clock_timestamp() NOT NULL,
    CONSTRAINT provider_routing_override_mode_check CHECK ((mode = ANY (ARRAY['normal'::text, 'paused'::text, 'draining'::text, 'capped'::text, 'recovering'::text]))),
    CONSTRAINT provider_routing_override_share_cap_check CHECK (((share_cap >= 0) AND (share_cap <= 100))),
    CONSTRAINT provider_routing_override_version_check CHECK ((version > 0))
);


--
-- Name: provider_spend_reservation; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.provider_spend_reservation (
    account text NOT NULL,
    unit text NOT NULL,
    operation_id text NOT NULL,
    period_start date NOT NULL,
    currency text NOT NULL,
    cost_minor bigint NOT NULL,
    status text NOT NULL,
    tariff_revision text NOT NULL,
    lane text NOT NULL,
    CONSTRAINT provider_spend_reservation_cost_minor_check CHECK ((cost_minor >= 0)),
    CONSTRAINT provider_spend_reservation_status_check CHECK ((status = ANY (ARRAY['uncertain'::text, 'confirmed'::text, 'released'::text])))
);


--
-- Name: staff_legacy_work; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.staff_legacy_work AS
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
-- Name: strict_screen_obligation; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.strict_screen_obligation (
    case_id text NOT NULL,
    original_operation text NOT NULL,
    replacement_operation text,
    status text NOT NULL,
    assigned_to text,
    assign_at timestamp with time zone NOT NULL,
    receipt jsonb NOT NULL,
    CONSTRAINT strict_screen_obligation_status_check CHECK ((status = ANY (ARRAY['outstanding'::text, 'clear'::text, 'hit'::text, 'not_accepted'::text, 'inconclusive'::text])))
);


--
-- Name: staff_current_work; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.staff_current_work AS
 SELECT w.work_id,
    w.case_id,
    w.institution,
    w.product,
    w.state,
    w.kind,
    w.operation_id,
    w.opened_at
   FROM public.staff_legacy_work w
  WHERE (NOT (EXISTS ( SELECT
           FROM public.strict_screen_obligation h
          WHERE ((h.case_id = w.case_id) AND (h.original_operation = w.operation_id)))))
UNION ALL
 SELECT ('review:'::text || h.original_operation) AS work_id,
    c.case_id,
    c.institution,
    c.product,
    c.state,
    'screening_review'::text AS kind,
    h.original_operation AS operation_id,
    COALESCE(((h.receipt ->> 'received_at'::text))::timestamp with time zone, h.assign_at) AS opened_at
   FROM (public.strict_screen_obligation h
     JOIN public.product_case c USING (case_id))
  WHERE ((h.assigned_to IS NOT NULL) AND (h.status = ANY (ARRAY['outstanding'::text, 'hit'::text, 'inconclusive'::text])) AND ((h.receipt ->> 'review'::text) IS NULL));


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
-- Name: provider_attempt_budget provider_attempt_budget_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_attempt_budget
    ADD CONSTRAINT provider_attempt_budget_pkey PRIMARY KEY (requirement_id);


--
-- Name: provider_attempt provider_attempt_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_attempt
    ADD CONSTRAINT provider_attempt_pkey PRIMARY KEY (token);


--
-- Name: provider_billing_account provider_billing_account_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_billing_account
    ADD CONSTRAINT provider_billing_account_pkey PRIMARY KEY (account);


--
-- Name: provider_billing_receipt provider_billing_receipt_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_billing_receipt
    ADD CONSTRAINT provider_billing_receipt_pkey PRIMARY KEY (account, receipt_id);


--
-- Name: provider_circuit provider_circuit_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_circuit
    ADD CONSTRAINT provider_circuit_pkey PRIMARY KEY (breaker_key);


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
-- Name: provider_override_history provider_override_history_institution_product_provider_comm_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_override_history
    ADD CONSTRAINT provider_override_history_institution_product_provider_comm_key UNIQUE (institution, product, provider, command_id);


--
-- Name: provider_override_history provider_override_history_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_override_history
    ADD CONSTRAINT provider_override_history_pkey PRIMARY KEY (institution, product, provider, version);


--
-- Name: provider_rate_bucket provider_rate_bucket_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_rate_bucket
    ADD CONSTRAINT provider_rate_bucket_pkey PRIMARY KEY (bucket);


--
-- Name: provider_route_binding provider_route_binding_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_binding
    ADD CONSTRAINT provider_route_binding_pkey PRIMARY KEY (requirement_id, provider);


--
-- Name: provider_route_contract provider_route_contract_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_contract
    ADD CONSTRAINT provider_route_contract_pkey PRIMARY KEY (kind, provider);


--
-- Name: provider_route_health provider_route_health_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_health
    ADD CONSTRAINT provider_route_health_pkey PRIMARY KEY (institution, product, provider);


--
-- Name: provider_route_selection provider_route_selection_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_selection
    ADD CONSTRAINT provider_route_selection_pkey PRIMARY KEY (operation_id);


--
-- Name: provider_route_selection provider_route_selection_requirement_id_evidence_revision_s_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_selection
    ADD CONSTRAINT provider_route_selection_requirement_id_evidence_revision_s_key UNIQUE (requirement_id, evidence_revision, selection_seq);


--
-- Name: provider_routing_override provider_routing_override_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_routing_override
    ADD CONSTRAINT provider_routing_override_pkey PRIMARY KEY (institution, product, provider);


--
-- Name: provider_spend_reservation provider_spend_reservation_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_spend_reservation
    ADD CONSTRAINT provider_spend_reservation_pkey PRIMARY KEY (account, unit);


--
-- Name: staff_work_acknowledgement staff_work_acknowledgement_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_work_acknowledgement
    ADD CONSTRAINT staff_work_acknowledgement_pkey PRIMARY KEY (work_id);


--
-- Name: strict_screen_obligation strict_screen_obligation_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.strict_screen_obligation
    ADD CONSTRAINT strict_screen_obligation_pkey PRIMARY KEY (original_operation);


--
-- Name: strict_screen_obligation strict_screen_obligation_replacement_operation_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.strict_screen_obligation
    ADD CONSTRAINT strict_screen_obligation_replacement_operation_key UNIQUE (replacement_operation);


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
-- Name: provider_case_attempts; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_case_attempts ON public.provider_attempt USING btree (case_id, started_at);


--
-- Name: provider_health_window; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_health_window ON public.provider_attempt USING btree (breaker_key, finished_at) WHERE (finished_at IS NOT NULL);


--
-- Name: provider_lease_active; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_lease_active ON public.provider_lease USING btree (provider, expires_at) WHERE (finished_at IS NULL);


--
-- Name: provider_lease_rate; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_lease_rate ON public.provider_lease USING btree (provider, started_at);


--
-- Name: provider_operation_attempts; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_operation_attempts ON public.provider_attempt USING btree (operation_id, lane, started_at);


--
-- Name: provider_period_spend; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_period_spend ON public.provider_spend_reservation USING btree (account, period_start, status);


--
-- Name: provider_rate_window; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_rate_window ON public.provider_attempt USING btree (rate_bucket, started_at, lane);


--
-- Name: provider_routing_case; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_routing_case ON public.provider_route_selection USING btree (case_id, kind, selection_seq);


--
-- Name: provider_routing_queue; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX provider_routing_queue ON public.provider_route_selection USING btree (institution, product, provider, status, created_at);


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
-- Name: strict_screen_case; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX strict_screen_case ON public.strict_screen_obligation USING btree (case_id);


--
-- Name: strict_screen_outstanding; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX strict_screen_outstanding ON public.strict_screen_obligation USING btree (assigned_to, assign_at) WHERE (status = 'outstanding'::text);


--
-- Name: product_fact advance_subject_generation; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER advance_subject_generation AFTER INSERT ON public.product_fact FOR EACH ROW WHEN (((new.fact #>> '{evidence,kind}'::text[]) = ANY (ARRAY['document'::text, 'face'::text]))) EXECUTE FUNCTION public.ekyc_advance_fact_generation();


--
-- Name: product_fact fact_time; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER fact_time BEFORE INSERT ON public.product_fact FOR EACH ROW EXECUTE FUNCTION public.ekyc_fact_time();


--
-- Name: product_case product_screening_approval; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER product_screening_approval BEFORE INSERT OR UPDATE ON public.product_case FOR EACH ROW EXECUTE FUNCTION public.ekyc_guard_screening_approval();


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
-- Name: provider_attempt provider_attempt_billing_account_billable_unit_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_attempt
    ADD CONSTRAINT provider_attempt_billing_account_billable_unit_fkey FOREIGN KEY (billing_account, billable_unit) REFERENCES public.provider_spend_reservation(account, unit);


--
-- Name: provider_attempt provider_attempt_breaker_key_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_attempt
    ADD CONSTRAINT provider_attempt_breaker_key_fkey FOREIGN KEY (breaker_key) REFERENCES public.provider_circuit(breaker_key);


--
-- Name: provider_attempt provider_attempt_case_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_attempt
    ADD CONSTRAINT provider_attempt_case_id_fkey FOREIGN KEY (case_id) REFERENCES public.product_case(case_id);


--
-- Name: provider_attempt provider_attempt_rate_bucket_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_attempt
    ADD CONSTRAINT provider_attempt_rate_bucket_fkey FOREIGN KEY (rate_bucket) REFERENCES public.provider_rate_bucket(bucket);


--
-- Name: provider_attempt provider_attempt_requirement_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_attempt
    ADD CONSTRAINT provider_attempt_requirement_id_fkey FOREIGN KEY (requirement_id) REFERENCES public.provider_attempt_budget(requirement_id);


--
-- Name: provider_billing_receipt provider_billing_receipt_account_unit_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_billing_receipt
    ADD CONSTRAINT provider_billing_receipt_account_unit_fkey FOREIGN KEY (account, unit) REFERENCES public.provider_spend_reservation(account, unit);


--
-- Name: provider_lease provider_lease_provider_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_lease
    ADD CONSTRAINT provider_lease_provider_fkey FOREIGN KEY (provider) REFERENCES public.provider_gate(provider);


--
-- Name: provider_route_contract provider_route_contract_billing_account_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_contract
    ADD CONSTRAINT provider_route_contract_billing_account_fkey FOREIGN KEY (billing_account) REFERENCES public.provider_billing_account(account);


--
-- Name: provider_route_contract provider_route_contract_breaker_key_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_contract
    ADD CONSTRAINT provider_route_contract_breaker_key_fkey FOREIGN KEY (breaker_key) REFERENCES public.provider_circuit(breaker_key);


--
-- Name: provider_route_contract provider_route_contract_rate_bucket_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_contract
    ADD CONSTRAINT provider_route_contract_rate_bucket_fkey FOREIGN KEY (rate_bucket) REFERENCES public.provider_rate_bucket(bucket);


--
-- Name: provider_route_selection provider_route_selection_predecessor_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_selection
    ADD CONSTRAINT provider_route_selection_predecessor_fkey FOREIGN KEY (predecessor) REFERENCES public.provider_route_selection(operation_id);


--
-- Name: provider_route_selection provider_route_selection_requirement_id_provider_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_route_selection
    ADD CONSTRAINT provider_route_selection_requirement_id_provider_fkey FOREIGN KEY (requirement_id, provider) REFERENCES public.provider_route_binding(requirement_id, provider);


--
-- Name: provider_spend_reservation provider_spend_reservation_account_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.provider_spend_reservation
    ADD CONSTRAINT provider_spend_reservation_account_fkey FOREIGN KEY (account) REFERENCES public.provider_billing_account(account);


--
-- Name: staff_work_acknowledgement staff_work_acknowledgement_case_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.staff_work_acknowledgement
    ADD CONSTRAINT staff_work_acknowledgement_case_id_fkey FOREIGN KEY (case_id) REFERENCES public.product_case(case_id);


--
-- Name: strict_screen_obligation strict_screen_obligation_case_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.strict_screen_obligation
    ADD CONSTRAINT strict_screen_obligation_case_id_fkey FOREIGN KEY (case_id) REFERENCES public.product_case(case_id) DEFERRABLE INITIALLY DEFERRED;


--
-- PostgreSQL database dump complete
--
