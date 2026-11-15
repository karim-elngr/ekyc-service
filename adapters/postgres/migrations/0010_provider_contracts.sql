-- The demo contracts count active HTTP calls in Restate and rolling starts here.
CREATE TABLE provider_rate_bucket (
    bucket text PRIMARY KEY,
    per_minute integer NOT NULL CHECK (per_minute > 0),
    lookup_reserve integer NOT NULL CHECK (lookup_reserve >= 0 AND lookup_reserve < per_minute),
    throttled_until timestamptz
);
CREATE TABLE provider_circuit (
    breaker_key text PRIMARY KEY,
    generation bigint NOT NULL DEFAULT 0,
    failures integer NOT NULL DEFAULT 0,
    open_until timestamptz,
    probe_token text,
    probe_until timestamptz
);
CREATE TABLE provider_billing_account (
    account text PRIMARY KEY,
    currency text NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    timezone text NOT NULL,
    monthly_ceiling bigint NOT NULL CHECK (monthly_ceiling >= 0),
    lookup_reserve bigint NOT NULL CHECK (lookup_reserve BETWEEN 0 AND monthly_ceiling)
);
CREATE TABLE provider_route_contract (
    kind text NOT NULL, provider text NOT NULL, pool text NOT NULL,
    rate_bucket text NOT NULL REFERENCES provider_rate_bucket,
    breaker_key text NOT NULL REFERENCES provider_circuit,
    billing_account text NOT NULL REFERENCES provider_billing_account,
    tariff_revision text NOT NULL,
    submit_cost bigint NOT NULL CHECK (submit_cost >= 0),
    lookup_cost bigint NOT NULL CHECK (lookup_cost >= 0),
    billing_unit text NOT NULL CHECK (billing_unit IN ('operation','request')),
    failure_threshold integer NOT NULL CHECK (failure_threshold > 0),
    cooldown_ms integer NOT NULL CHECK (cooldown_ms > 0),
    probe_ms integer NOT NULL CHECK (probe_ms > 0),
    PRIMARY KEY (kind,provider)
);
CREATE TABLE provider_attempt_budget (
    requirement_id text PRIMARY KEY,
    deadline_ms bigint NOT NULL,
    max_attempts integer NOT NULL CHECK (max_attempts > 0),
    submissions integer NOT NULL DEFAULT 0,
    lookups integer NOT NULL DEFAULT 0,
    max_lookups integer NOT NULL DEFAULT 256 CHECK (max_lookups > 0)
);
CREATE TABLE provider_spend_reservation (
    account text NOT NULL REFERENCES provider_billing_account,
    unit text NOT NULL, operation_id text NOT NULL, period_start date NOT NULL,
    currency text NOT NULL, cost_minor bigint NOT NULL CHECK (cost_minor >= 0),
    status text NOT NULL CHECK (status IN ('uncertain','confirmed','released')),
    tariff_revision text NOT NULL, lane text NOT NULL,
    PRIMARY KEY(account,unit)
);
CREATE INDEX provider_period_spend ON provider_spend_reservation(account,period_start,status);
CREATE TABLE provider_attempt (
    token text PRIMARY KEY, operation_id text NOT NULL, case_id text NOT NULL REFERENCES product_case,
    requirement_id text NOT NULL REFERENCES provider_attempt_budget, kind text NOT NULL, provider text NOT NULL,
    lane text NOT NULL CHECK (lane IN ('submit','lookup')), started_at timestamptz NOT NULL,
    finished_at timestamptz, elapsed_ms bigint,
    class text NOT NULL CHECK(class IN ('possible_send','ok','throttled','transient','permanent','expired')),
    rate_bucket text NOT NULL REFERENCES provider_rate_bucket, breaker_key text NOT NULL REFERENCES provider_circuit,
    generation bigint NOT NULL, probe boolean NOT NULL, billing_account text NOT NULL, billable_unit text NOT NULL,
    result jsonb, FOREIGN KEY (billing_account,billable_unit) REFERENCES provider_spend_reservation(account,unit)
);
CREATE INDEX provider_rate_window ON provider_attempt(rate_bucket,started_at,lane);
CREATE INDEX provider_operation_attempts ON provider_attempt(operation_id,lane,started_at);
CREATE INDEX provider_health_window ON provider_attempt(breaker_key,finished_at) WHERE finished_at IS NOT NULL;
CREATE INDEX provider_case_attempts ON provider_attempt(case_id,started_at);

CREATE FUNCTION ekyc_attempt_wait(reason text, until_at timestamptz) RETURNS jsonb
LANGUAGE sql VOLATILE AS $$
    SELECT jsonb_build_object('state','wait','reason',reason,'retry_ms',
        greatest(1,ceil(extract(epoch FROM until_at-clock_timestamp())*1000)::bigint))
$$;

CREATE FUNCTION ekyc_authorize_attempt(p_token text,p_case text,p_op text,p_kind text,p_provider text,
    p_pool text,p_lane text,p_deadline bigint,p_max integer) RETURNS jsonb LANGUAGE plpgsql AS $$
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

CREATE FUNCTION ekyc_finish_attempt(p_token text,p_class text,p_result jsonb,p_elapsed_ms bigint,p_backoff_ms bigint)
RETURNS void LANGUAGE plpgsql AS $$
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

-- Invoice lines are imported by their immutable upstream reference. A later receipt
-- settles the original billing period even if the current monthly period has moved.
CREATE TABLE provider_billing_receipt (
    account text NOT NULL, receipt_id text NOT NULL, unit text NOT NULL,
    amount_minor bigint NOT NULL CHECK(amount_minor>=0), currency text NOT NULL,
    received_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(account,receipt_id),
    FOREIGN KEY(account,unit) REFERENCES provider_spend_reservation(account,unit)
);
CREATE FUNCTION ekyc_reconcile_charge(p_account text,p_unit text,p_receipt text,p_amount bigint,p_currency text)
RETURNS void LANGUAGE plpgsql AS $$
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
