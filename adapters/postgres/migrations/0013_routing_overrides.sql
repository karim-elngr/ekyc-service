CREATE TABLE provider_routing_override (
    institution text NOT NULL,
    product text NOT NULL,
    provider text NOT NULL,
    version bigint NOT NULL CHECK(version > 0),
    mode text NOT NULL CHECK(mode IN ('normal','paused','draining','capped','recovering')),
    share_cap integer CHECK(share_cap BETWEEN 0 AND 100),
    expires_at timestamptz NOT NULL,
    actor text NOT NULL,
    reason text NOT NULL,
    changed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(institution, product, provider)
);
CREATE TABLE provider_override_history (
    institution text NOT NULL,
    product text NOT NULL,
    provider text NOT NULL,
    version bigint NOT NULL,
    command_id text NOT NULL,
    request jsonb NOT NULL,
    previous jsonb,
    effective jsonb NOT NULL,
    PRIMARY KEY(institution,product,provider,version),
    UNIQUE(institution,product,provider,command_id)
);
