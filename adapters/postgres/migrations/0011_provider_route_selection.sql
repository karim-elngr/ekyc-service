-- Full issuer locators stay in protected bindings; staff read explicit selection columns.
CREATE TABLE provider_route_binding (
    requirement_id text NOT NULL,
    provider text NOT NULL,
    binding jsonb NOT NULL,
    PRIMARY KEY(requirement_id,provider)
);
CREATE TABLE provider_route_selection (
    operation_id text PRIMARY KEY,
    case_id text NOT NULL,
    institution text NOT NULL,
    product text NOT NULL,
    kind text NOT NULL,
    requirement_id text NOT NULL,
    evidence_revision text NOT NULL,
    selection_seq integer NOT NULL,
    policy_version text NOT NULL,
    candidates text[] NOT NULL,
    bucket integer CHECK(bucket BETWEEN 0 AND 99),
    provider text NOT NULL,
    reason text NOT NULL,
    override_version bigint NOT NULL,
    placement_snapshot jsonb NOT NULL,
    block_reason text,
    predecessor text REFERENCES provider_route_selection(operation_id),
    deadline_ms bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    retired boolean NOT NULL DEFAULT false,
    possible_send boolean NOT NULL DEFAULT false,
    status text NOT NULL DEFAULT 'assigned' CHECK(status IN
        ('assigned','outstanding','retired','complete','not_accepted','inconclusive','blocked')),
    result jsonb,
    received_at timestamptz,
    observed_at timestamptz,
    owner text NOT NULL DEFAULT 'provider-operations',
    UNIQUE(requirement_id,evidence_revision,selection_seq),
    FOREIGN KEY(requirement_id,provider) REFERENCES provider_route_binding
);
CREATE INDEX provider_routing_queue ON provider_route_selection
    (institution,product,provider,status,created_at);
CREATE INDEX provider_routing_case ON provider_route_selection(case_id,kind,selection_seq);

-- Fixture recovery evidence is persisted per institution/product/failure domain.
CREATE TABLE provider_route_health (
    institution text NOT NULL, product text NOT NULL, provider text NOT NULL,
    stage integer NOT NULL DEFAULT 80 CHECK(stage IN (5,20,50,80)),
    cause text NOT NULL DEFAULT 'healthy', override_version bigint NOT NULL DEFAULT 0,
    since timestamptz NOT NULL DEFAULT clock_timestamp(),
    samples integer NOT NULL DEFAULT 0, p95_ms double precision, error_rate double precision,
    window_end timestamptz,
    PRIMARY KEY(institution,product,provider)
);

CREATE FUNCTION ekyc_check_route(p_operation text,p_lane text,p_possible_send boolean)
RETURNS text LANGUAGE plpgsql AS $$
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
