CREATE TABLE case_view (
    case_id text PRIMARY KEY,
    version bigint NOT NULL CHECK (version > 0),
    terminal boolean NOT NULL,
    snapshot jsonb NOT NULL
);

CREATE TABLE case_revision (
    case_id text NOT NULL REFERENCES case_view,
    version bigint NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    snapshot jsonb NOT NULL,
    PRIMARY KEY (case_id, version)
);

CREATE TABLE decision_outbox (
    event_id text PRIMARY KEY,
    case_id text NOT NULL UNIQUE REFERENCES case_view,
    version bigint NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (case_id, version) REFERENCES case_revision
);
