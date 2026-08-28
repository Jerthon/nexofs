-- Schema inicial do metadata store. SPEC §10.3.
-- Aplicado dentro de uma transação única pelo runner de migrations
-- (nexofs-metadata-store); `PRAGMA user_version` marca a versão aplicada.

CREATE TABLE providers (
    provider_id          TEXT PRIMARY KEY,
    display_name         TEXT NOT NULL,
    capabilities_json    TEXT NOT NULL,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL
);

CREATE TABLE accounts (
    account_id           TEXT PRIMARY KEY,
    provider_id          TEXT NOT NULL REFERENCES providers(provider_id),
    provider_account_id  TEXT NOT NULL,
    account_type         TEXT NOT NULL,
    display_name         TEXT NOT NULL,
    tenant_id            TEXT,
    auth_state           TEXT NOT NULL,
    enabled              INTEGER NOT NULL DEFAULT 1,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    UNIQUE(provider_id, provider_account_id)
);

CREATE TABLE namespaces (
    namespace_id         TEXT PRIMARY KEY,
    account_id           TEXT NOT NULL REFERENCES accounts(account_id),
    remote_namespace_id  TEXT NOT NULL,
    display_name         TEXT NOT NULL,
    namespace_type       TEXT NOT NULL,
    mount_path           TEXT NOT NULL UNIQUE,
    mount_state          TEXT NOT NULL,
    change_cursor        TEXT,
    cursor_state         TEXT NOT NULL DEFAULT 'UNINITIALIZED',
    last_change_check_at INTEGER,
    last_full_scan_at    INTEGER,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    UNIQUE(account_id, remote_namespace_id)
);

CREATE TABLE items (
    item_id               TEXT PRIMARY KEY,
    namespace_id          TEXT NOT NULL REFERENCES namespaces(namespace_id),
    remote_item_id        TEXT,
    parent_item_id        TEXT REFERENCES items(item_id),
    name                   TEXT NOT NULL,
    normalized_name       TEXT NOT NULL,
    item_type              TEXT NOT NULL,
    size_bytes             INTEGER NOT NULL DEFAULT 0,
    mime_type              TEXT,
    remote_version         TEXT,
    remote_content_version TEXT,
    remote_modified_at     INTEGER,
    remote_created_at      INTEGER,
    children_state         TEXT NOT NULL DEFAULT 'UNKNOWN',
    remote_state           TEXT NOT NULL DEFAULT 'PRESENT',
    source_layer           TEXT NOT NULL DEFAULT 'REMOTE',
    provider_metadata_json TEXT,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL,
    UNIQUE(namespace_id, remote_item_id)
);

CREATE UNIQUE INDEX idx_items_parent_name
ON items(namespace_id, parent_item_id, normalized_name)
WHERE remote_state <> 'DELETED';

CREATE INDEX idx_items_parent
ON items(namespace_id, parent_item_id);

CREATE TABLE local_states (
    item_id                TEXT PRIMARY KEY REFERENCES items(item_id),
    hydration_state        TEXT NOT NULL,
    pin_state              TEXT NOT NULL,
    sync_state             TEXT NOT NULL,
    local_version          INTEGER NOT NULL DEFAULT 0,
    base_remote_version    TEXT,
    base_content_version   TEXT,
    cache_object_id        TEXT,
    overlay_path           TEXT,
    local_size_bytes       INTEGER,
    local_modified_at      INTEGER,
    last_access_at         INTEGER,
    open_handle_count      INTEGER NOT NULL DEFAULT 0,
    dirty_since            INTEGER,
    error_code             TEXT,
    error_message          TEXT,
    updated_at             INTEGER NOT NULL
);

CREATE TABLE operations (
    operation_id           TEXT PRIMARY KEY,
    namespace_id           TEXT NOT NULL REFERENCES namespaces(namespace_id),
    item_id                 TEXT REFERENCES items(item_id),
    operation_type         TEXT NOT NULL,
    state                  TEXT NOT NULL,
    priority               INTEGER NOT NULL,
    idempotency_key        TEXT NOT NULL,
    attempt_count          INTEGER NOT NULL DEFAULT 0,
    next_attempt_at        INTEGER,
    base_remote_version    TEXT,
    payload_json           TEXT NOT NULL,
    last_error_kind        TEXT,
    last_error_message     TEXT,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL,
    UNIQUE(idempotency_key)
);

CREATE INDEX idx_operations_scheduler
ON operations(state, next_attempt_at, priority, created_at);

CREATE TABLE conflicts (
    conflict_id            TEXT PRIMARY KEY,
    namespace_id           TEXT NOT NULL REFERENCES namespaces(namespace_id),
    item_id                 TEXT REFERENCES items(item_id),
    conflict_type          TEXT NOT NULL,
    state                  TEXT NOT NULL,
    local_version          INTEGER,
    base_remote_version    TEXT,
    current_remote_version TEXT,
    local_snapshot_path    TEXT,
    remote_snapshot_path   TEXT,
    resolution             TEXT,
    resolution_payload_json TEXT,
    detected_at            INTEGER NOT NULL,
    resolved_at            INTEGER
);

CREATE TABLE ignore_rules (
    rule_id                TEXT PRIMARY KEY,
    namespace_id           TEXT REFERENCES namespaces(namespace_id),
    root_item_id            TEXT REFERENCES items(item_id),
    source_type             TEXT NOT NULL,
    pattern                 TEXT NOT NULL,
    negated                 INTEGER NOT NULL DEFAULT 0,
    directory_only          INTEGER NOT NULL DEFAULT 0,
    enabled                 INTEGER NOT NULL DEFAULT 1,
    precedence              INTEGER NOT NULL,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
);

CREATE TABLE cache_objects (
    cache_object_id         TEXT PRIMARY KEY,
    storage_path            TEXT NOT NULL UNIQUE,
    size_bytes              INTEGER NOT NULL,
    content_hash            TEXT,
    state                   TEXT NOT NULL,
    created_at              INTEGER NOT NULL,
    last_access_at          INTEGER NOT NULL,
    ref_count               INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE inode_map (
    namespace_id           TEXT NOT NULL REFERENCES namespaces(namespace_id),
    item_id                 TEXT NOT NULL REFERENCES items(item_id),
    inode                   INTEGER NOT NULL,
    PRIMARY KEY(namespace_id, item_id),
    UNIQUE(namespace_id, inode)
);
