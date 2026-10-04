-- Initial schema (covers design document chapters 4, 6, 9 and 11)
--
-- Policy:
--  - Put tenant ID in every table from the start (15.2; retrofitting it is a costly migration)
--  - Ever-growing tables assume partitioning (15.1)
--  - Raw data bodies live in object storage. Only metadata and hashes are stored here
--  - Values acquired from the vehicle are immutable. Only annotations and work records are editable (4.3)

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ================================================================ Tenants / users

CREATE TABLE tenants (
    id              uuid PRIMARY KEY,
    name            text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE operators (
    id              uuid PRIMARY KEY,
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    subject         text NOT NULL,              -- OIDC sub
    display_name    text NOT NULL,
    disabled_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, subject)
);

-- 6.8 axis 1: operator roles. Scope can be narrowed by vehicle model or OEM
CREATE TABLE operator_roles (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    operator_id     uuid NOT NULL REFERENCES operators(id),
    role            text NOT NULL
        CHECK (role IN ('viewer','worker','reprogrammer','remote','approver','admin')),
    scope           jsonb NOT NULL DEFAULT '{}'::jsonb,  -- {"oem":["X"],"models":[...]}
    PRIMARY KEY (tenant_id, operator_id, role)
);

-- ================================================================ Agents

CREATE TABLE agents (
    id              uuid PRIMARY KEY,
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    display_name    text NOT NULL,
    mode            text NOT NULL CHECK (mode IN ('user','machine')),
    -- Device identification. Not used for authentication (6.4)
    machine_hint    text,
    os_user_hint    text,
    public_key      bytea NOT NULL,
    -- 6.6 connection restriction preset. Relaxing it requires action on the device
    preset          text NOT NULL DEFAULT 'personal'
        CHECK (preset IN ('personal','personalRemote','shared')),
    allow_remote    boolean NOT NULL DEFAULT false,
    registered_by   uuid REFERENCES operators(id),
    registered_at   timestamptz NOT NULL DEFAULT now(),
    revoked_at      timestamptz,
    -- 9.5 capabilities. Updated on every connection
    capabilities    jsonb NOT NULL DEFAULT '{}'::jsonb,
    agent_version   text,
    -- Not updated on heartbeat; batch-written periodically (15.1)
    last_seen_at    timestamptz
);

CREATE INDEX agents_tenant_online ON agents (tenant_id, last_seen_at DESC)
    WHERE revoked_at IS NULL;

-- 6.8 axis 2: agent-side acceptance settings (list of allowed operators)
CREATE TABLE agent_allowed_operators (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    agent_id        uuid NOT NULL REFERENCES agents(id),
    operator_id     uuid NOT NULL REFERENCES operators(id),
    PRIMARY KEY (tenant_id, agent_id, operator_id)
);

-- Detected VCIs. Expanded from capabilities to make them searchable
CREATE TABLE agent_vcis (
    tenant_id       uuid NOT NULL,
    agent_id        uuid NOT NULL REFERENCES agents(id),
    vci_id          text NOT NULL,
    name            text NOT NULL,
    vendor          text NOT NULL,
    standard        text NOT NULL,              -- 'j2534' | 'dpdu'
    abi_interpretation text NOT NULL
        CHECK (abi_interpretation IN ('standard','productDefined','inferred')),
    loadable        boolean NOT NULL,
    reason          text,
    signature_verifier text CHECK (signature_verifier IN ('library','adapter','ecuOnly')),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, agent_id, vci_id)
);

-- ================================================================ Artifacts / extensions

-- 11.2 ECU artifacts. Stored byte-for-byte unchanged and identified by hash
CREATE TABLE artifacts (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    digest          text NOT NULL,              -- sha256:...
    size_bytes      bigint NOT NULL,
    storage_key     text NOT NULL,              -- key in object storage
    -- Metadata from the manifest. jsonb because the format differs per provider
    manifest        jsonb,
    manifest_parsed boolean NOT NULL DEFAULT false,
    -- Administrator declaration when there is no manifest (11.2)
    declared_targets jsonb,
    signature_verifier text NOT NULL DEFAULT 'ecuOnly'
        CHECK (signature_verifier IN ('library','adapter','ecuOnly')),
    imported_by     uuid REFERENCES operators(id),
    imported_at     timestamptz NOT NULL DEFAULT now(),
    published_at    timestamptz,
    rollout_percent smallint NOT NULL DEFAULT 0,
    superseded_by   text,                       -- predecessor/successor link for rollback
    PRIMARY KEY (tenant_id, digest)
);

-- 9.4 extension packages. Distributed after import signing (11.1)
CREATE TABLE extensions (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    id              text NOT NULL,              -- reverse-domain format
    version         text NOT NULL,
    kind            text NOT NULL
        CHECK (kind IN ('vciProfile','ir','screen','recordTemplate','message','unit')),
    schema_version  integer NOT NULL,
    core_version_range text NOT NULL,
    manifest_digest text NOT NULL,
    storage_key     text NOT NULL,
    -- 9.4 license record
    license_origin  text,
    license_scope   text CHECK (license_scope IN ('tenantOnly','redistributable')),
    signed_at       timestamptz,
    imported_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, id, version)
);

-- An IR is a file per ECU variant. Deduplicated via content addressing (8.2.6)
CREATE TABLE ir_documents (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    digest          text NOT NULL,
    extension_id    text NOT NULL,
    schema_version  integer NOT NULL,
    variant_name    text NOT NULL,
    part_numbers    text[] NOT NULL DEFAULT '{}',
    sw_versions     text[] NOT NULL DEFAULT '{}',
    source_format   text NOT NULL,              -- 'odx' | 'csv-js'
    source_digest   text,
    storage_key     text NOT NULL,
    PRIMARY KEY (tenant_id, digest)
);

CREATE INDEX ir_documents_lookup ON ir_documents USING gin (part_numbers);

-- ================================================================ Jobs

CREATE TABLE jobs (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    id              uuid NOT NULL,              -- UUIDv7 (client-generated)
    kind            text NOT NULL CHECK (kind IN
        ('acquire','monitorSession','monitorCapture','runSequence','writeSettings','reprogram')),
    agent_id        uuid NOT NULL REFERENCES agents(id),
    operator_id     uuid NOT NULL REFERENCES operators(id),
    vci_id          text NOT NULL,
    vin             text,                       -- NULL until known (two-stage locking in 8.8)
    ir_digest       text,
    artifact_digest text,
    parameters      jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- 6.3 confirmation requirements
    confirmation_level text NOT NULL
        CHECK (confirmation_level IN ('notifyOnly','gracePeriod','onSiteApproval','twoPerson')),
    approved_by     uuid REFERENCES operators(id),
    approved_at     timestamptz,
    on_site_approved_at timestamptz,
    remote          boolean NOT NULL DEFAULT false,
    on_site_contact text,
    -- 5.3 start deadline / execution window
    start_deadline  timestamptz NOT NULL,
    window_from     timestamptz,
    window_to       timestamptz,
    -- Signed instruction (11.1). Not regenerated on each delivery
    signed_job      bytea NOT NULL,
    signing_key_id  text NOT NULL,
    state           text NOT NULL DEFAULT 'received',
    progress_permille smallint NOT NULL DEFAULT 0,
    failure_code    text,
    failure_detail  text,
    -- While disconnected, shown as "running (last confirmed at)" (5.3)
    last_reported_at timestamptz,
    dataset_id      uuid,
    created_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz,
    idempotency_key text NOT NULL,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, idempotency_key)
);

-- Delivery queue. Dequeued with SELECT ... FOR UPDATE SKIP LOCKED (chapter 12)
CREATE INDEX jobs_pending ON jobs (tenant_id, agent_id, created_at)
    WHERE state IN ('received','precheck','ready');

-- Targets for reconciliation on reconnection (5.3)
CREATE INDEX jobs_open ON jobs (tenant_id, agent_id)
    WHERE finished_at IS NULL;

-- 8.2.5 checkpoint summary for handover to another device
CREATE TABLE job_checkpoints (
    tenant_id       uuid NOT NULL,
    job_id          uuid NOT NULL,
    section         integer NOT NULL,
    vin             text,
    artifact_digest text,
    interruptible   text NOT NULL CHECK (interruptible IN ('yes','no','recoveryRequired')),
    reported_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, job_id)
);

-- 8.8 per-vehicle soft lock. An auxiliary layer that does not work while offline
CREATE TABLE vehicle_locks (
    tenant_id       uuid NOT NULL,
    vin             text NOT NULL,
    job_id          uuid NOT NULL,
    agent_id        uuid NOT NULL,
    acquired_at     timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, vin)
);

-- ================================================================ Acquired data

CREATE TABLE datasets (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    id              uuid NOT NULL,
    job_id          uuid,
    kind            text NOT NULL CHECK (kind IN ('series','snapshot','selfTest','monitorCapture')),
    -- 4.5 acquisition metadata
    vin             text,
    ecu_address     text,
    part_number     text,
    sw_version      text,
    vci_model       text,
    vci_serial      text,
    library_version text,
    ir_digest       text,
    agent_version   text,
    clock_offset_ms bigint,
    -- 4.6 capture only
    capture_route   text CHECK (capture_route IN ('direct','relay')),
    capture_interval_ms integer,
    capture_frames_dropped integer,
    captured_at     timestamptz NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- 4.3 retention period
    retain_until    date,
    vin_redacted_at timestamptz,
    PRIMARY KEY (tenant_id, id)
);

CREATE INDEX datasets_by_vin ON datasets (tenant_id, vin, captured_at DESC);

-- Raw data is immutable chunks. The bodies live in object storage
CREATE TABLE dataset_chunks (
    tenant_id       uuid NOT NULL,
    dataset_id      uuid NOT NULL,
    seq             integer NOT NULL,
    from_time       timestamptz NOT NULL,
    to_time         timestamptz NOT NULL,
    storage_key     text NOT NULL,
    digest          text NOT NULL,
    size_bytes      bigint NOT NULL,
    chunk_format    text NOT NULL,   -- the format carries a version (the basis for deferring P1)
    PRIMARY KEY (tenant_id, dataset_id, seq)
);

-- Annotations. Independent records, so concurrent additions do not conflict (4.3)
CREATE TABLE annotations (
    tenant_id       uuid NOT NULL,
    id              uuid NOT NULL,
    dataset_id      uuid NOT NULL,
    target          text NOT NULL,              -- DTC code, time range, etc.
    text_body       text,
    verdict         text CHECK (verdict IN ('ok','needsCheck','ng')),
    author_id       uuid NOT NULL REFERENCES operators(id),
    version         integer NOT NULL DEFAULT 1,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz,
    PRIMARY KEY (tenant_id, id)
);

CREATE INDEX annotations_by_dataset ON annotations (tenant_id, dataset_id);

-- 4.3.1 work records. The only editable data
CREATE TABLE work_records (
    tenant_id       uuid NOT NULL,
    id              uuid NOT NULL,
    dataset_id      uuid,
    template_id     text NOT NULL,
    template_version text NOT NULL,
    version         integer NOT NULL DEFAULT 1,  -- base version for 3-way merge
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE work_record_items (
    tenant_id       uuid NOT NULL,
    record_id       uuid NOT NULL,
    item_id         text NOT NULL,
    value           jsonb,
    -- Auto-filled values come from the vehicle and are not editable (4.3.1)
    origin          text NOT NULL CHECK (origin IN ('manual','autoFilled')),
    version         integer NOT NULL DEFAULT 1,  -- per-item version
    updated_by      uuid REFERENCES operators(id),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, record_id, item_id)
);

-- Edit history. Used as input for conflict resolution and for auditing
CREATE TABLE work_record_history (
    tenant_id       uuid NOT NULL,
    record_id       uuid NOT NULL,
    item_id         text NOT NULL,
    base_value      jsonb,
    new_value       jsonb,
    reason          text,                        -- required when correcting numeric values
    editor_id       uuid NOT NULL REFERENCES operators(id),
    edited_at       timestamptz NOT NULL DEFAULT now()
);

-- Change feed (4.4). Partitioned with a retention period
CREATE TABLE change_feed (
    tenant_id       uuid NOT NULL,
    seq             bigserial,
    dataset_id      uuid,
    kind            text NOT NULL,
    payload         jsonb NOT NULL,
    occurred_at     timestamptz NOT NULL DEFAULT now()
) PARTITION BY RANGE (occurred_at);

-- ================================================================ Audit log

-- 16.3 Server receive time is authoritative; device time is recorded alongside for reference
CREATE TABLE audit_log (
    tenant_id       uuid NOT NULL,
    id              bigserial,
    operator_id     uuid,
    agent_id        uuid,
    job_id          uuid,
    vin             text,
    action          text NOT NULL,
    confirmation_level text,
    on_site_approved boolean,
    detail          jsonb NOT NULL DEFAULT '{}'::jsonb,
    recorded_at     timestamptz NOT NULL DEFAULT now(),  -- server receive time (authoritative)
    agent_reported_at timestamptz,                       -- device time (reference)
    offline_executed boolean NOT NULL DEFAULT false
) PARTITION BY RANGE (recorded_at);

-- On a deletion request, raw data is deleted but the audit log is kept. Only the VIN is redacted (4.3)
CREATE INDEX audit_log_by_vin ON audit_log (tenant_id, vin, recorded_at DESC);

-- ================================================================ Event deduplication

-- 5.3 Deduplicate by "agent ID + sequence number"
CREATE TABLE agent_event_cursor (
    tenant_id       uuid NOT NULL,
    agent_id        uuid NOT NULL,
    last_seq        bigint NOT NULL DEFAULT 0,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, agent_id)
);
