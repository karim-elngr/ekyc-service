-- All workers sharing a vendor quota use the same provider row as their lock.
CREATE TABLE provider_gate (
    provider text PRIMARY KEY,
    generation bigint NOT NULL DEFAULT 0,
    failures integer NOT NULL DEFAULT 0,
    open_until timestamptz,
    throttle_until timestamptz
);

CREATE TABLE provider_lease (
    provider text NOT NULL REFERENCES provider_gate,
    token text NOT NULL,
    generation bigint NOT NULL,
    probe boolean NOT NULL,
    started_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    finished_at timestamptz,
    PRIMARY KEY (provider, token)
);

CREATE INDEX provider_lease_rate ON provider_lease (provider, started_at);
CREATE INDEX provider_lease_active ON provider_lease (provider, expires_at) WHERE finished_at IS NULL;

CREATE TABLE caller_quota (
    caller text PRIMARY KEY,
    bucket timestamptz NOT NULL,
    used bigint NOT NULL
);

-- The caller pays for each submission attempt. Reads bypass this quota.
CREATE FUNCTION ekyc_admit(p_caller text, p_quota bigint) RETURNS bigint
LANGUAGE plpgsql AS $$
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

CREATE FUNCTION ekyc_wait_ms(now_at timestamptz, until_at timestamptz) RETURNS bigint
LANGUAGE sql IMMUTABLE STRICT AS $$
    SELECT greatest(1, ceil(extract(epoch FROM until_at - now_at) * 1000)::bigint)
$$;

-- -1 means the token has already performed or exhausted its one dispatch.
--  0 grants a new lease. Positive values tell the worker how long to wait.
CREATE FUNCTION ekyc_acquire_provider(
    p_provider text, p_token text, p_per_minute bigint, p_max_in_flight bigint, p_lease_ms bigint
) RETURNS bigint
LANGUAGE plpgsql AS $$
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

CREATE FUNCTION ekyc_finish_provider(
    p_provider text, p_token text, p_failed boolean, p_threshold bigint, p_cooldown_ms bigint,
    p_retry_after_ms bigint
) RETURNS void
LANGUAGE plpgsql AS $$
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
