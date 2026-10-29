-- Phase-one CORE persistence contract, used by storage_core runtime and migration.
-- Activation remains explicit and restricted to isolated repository .tmp copies.
-- Distinct from storage_v2::CORE_SCHEMA, the earlier full offline prototype.
-- Requires SQLite JSON/STRICT and per-connection functions:
--   cliphost_writer_generation() -> integer (2 for the new writer)
--   cliphost_write_scope() -> history/favorite/migration/NULL
-- These functions are read-only getters of private runtime state, not SQL setters.
-- The migration runner owns BEGIN/COMMIT, copies/guards all auxiliary tables,
-- seals old tables, and verifies the unchanged file/configuration snapshot.
PRAGMA foreign_keys = ON;
PRAGMA recursive_triggers = ON;

CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    checksum TEXT NOT NULL,
    applied_at TEXT NOT NULL
) STRICT;

CREATE TABLE store_info (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    dataset_id TEXT NOT NULL CHECK (length(dataset_id) > 0),
    format_family TEXT NOT NULL CHECK (format_family = 'cliphost'),
    format_version INTEGER NOT NULL CHECK (format_version = 2),
    layout_id TEXT NOT NULL CHECK (layout_id = 'core'),
    min_writer_generation INTEGER NOT NULL CHECK (min_writer_generation = 2),
    migration_id TEXT NOT NULL CHECK (length(migration_id) > 0)
) STRICT;

CREATE TABLE items (
    seq INTEGER PRIMARY KEY AUTOINCREMENT CHECK (seq > 0),
    scope TEXT NOT NULL CHECK (scope IN ('history', 'favorite')),
    id TEXT NOT NULL CHECK (length(id) > 0),
    -- Stable business token, not a Rust enum/class name. Adding a supported
    -- leaf kind must not require rebuilding this table. Repository validates
    -- the (kind,payload_version) contract and rejects unsupported semantics.
    kind TEXT NOT NULL CHECK (length(trim(kind)) > 0),
    title TEXT,
    icon_override TEXT,
    description TEXT NOT NULL DEFAULT '',
    payload_version INTEGER NOT NULL DEFAULT 1 CHECK (payload_version > 0),
    payload_json TEXT NOT NULL CHECK (
        json_valid(payload_json) AND json_type(payload_json) = 'object'
    ),
    source_extensions_json TEXT NOT NULL DEFAULT '{}' CHECK (
        json_valid(source_extensions_json) AND json_type(source_extensions_json) = 'object'
    ),
    dedupe_key TEXT NOT NULL DEFAULT '',
    source_process TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL CHECK (
        length(created_at) = 30 AND substr(created_at, 11, 1) = 'T'
        AND substr(created_at, 20, 1) = '.' AND substr(created_at, 30, 1) = 'Z'
    ),
    updated_at TEXT NOT NULL CHECK (
        length(updated_at) = 30 AND substr(updated_at, 11, 1) = 'T'
        AND substr(updated_at, 20, 1) = '.' AND substr(updated_at, 30, 1) = 'Z'
    ),
    sub_index INTEGER NOT NULL DEFAULT 0,
    is_pinned INTEGER NOT NULL DEFAULT 0 CHECK (is_pinned IN (0, 1)),
    revision TEXT NOT NULL CHECK (length(revision) > 0),
    owner_parent_id TEXT,
    children_json TEXT CHECK (children_json IS NULL OR (
        json_valid(children_json) AND json_type(children_json) = 'array'
    )),
    source_history_id TEXT,
    UNIQUE (scope, id),
    FOREIGN KEY (scope, owner_parent_id) REFERENCES items(scope, id)
        DEFERRABLE INITIALLY DEFERRED,
    CHECK (owner_parent_id IS NULL OR owner_parent_id <> id),
    CHECK (scope = 'favorite' OR (
        owner_parent_id IS NULL AND children_json IS NULL AND source_history_id IS NULL
    )),
    CHECK (scope = 'history' OR is_pinned = 0),
    CHECK (
        (scope = 'favorite' AND kind = 'folder' AND children_json IS NOT NULL)
        OR (kind <> 'folder' AND children_json IS NULL)
    )
) STRICT;

CREATE INDEX items_history_order ON items(
    is_pinned DESC, created_at DESC, sub_index ASC, seq ASC
) WHERE scope = 'history';
CREATE INDEX items_history_kind_order ON items(
    kind, is_pinned DESC, created_at DESC, sub_index ASC, seq ASC
) WHERE scope = 'history';
CREATE INDEX items_history_dedupe ON items(dedupe_key, created_at DESC)
    WHERE scope = 'history';
CREATE INDEX items_owner ON items(scope, owner_parent_id);

-- Keep the existing usage-key contract in phase one, including orphan keys.
-- No item FK: source usage rows without a current item must survive conversion.
CREATE TABLE usage_info (
    id TEXT PRIMARY KEY CHECK (length(id) > 0),
    last_use_time TEXT CHECK (last_use_time IS NULL OR (
        length(last_use_time) = 30 AND substr(last_use_time, 11, 1) = 'T'
        AND substr(last_use_time, 20, 1) = '.' AND substr(last_use_time, 30, 1) = 'Z'
    )),
    usage_count INTEGER NOT NULL DEFAULT 0 CHECK (usage_count >= 0)
) STRICT;
CREATE INDEX usage_info_recent ON usage_info(last_use_time DESC, id);

-- Row guards reject a mixed-scope statement atomically. Scope is established
-- by HistoryRepository/FavoriteRepository, never accepted from an edit DTO.
CREATE TRIGGER items_insert_guard BEFORE INSERT ON items BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2
        OR cliphost_write_scope() IS NOT NEW.scope
        THEN RAISE(ABORT, 'ITEM_WRITE_SCOPE_MISMATCH') END;
    SELECT CASE WHEN EXISTS(
        SELECT 1 FROM items WHERE seq=NEW.seq OR (scope=NEW.scope AND id=NEW.id)
    ) THEN RAISE(ABORT, 'ITEM_INSERT_IDENTITY_CONFLICT') END;
END;
CREATE TRIGGER items_update_guard BEFORE UPDATE ON items BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2
        OR cliphost_write_scope() IS NOT OLD.scope
        OR NEW.scope IS NOT OLD.scope OR NEW.id IS NOT OLD.id OR NEW.seq IS NOT OLD.seq
        THEN RAISE(ABORT, 'ITEM_WRITE_SCOPE_MISMATCH') END;
    -- Capture provenance is set by INSERT, never by an edit/usage/recapture.
    -- created_at is intentionally excluded: legacy duplicate recapture refreshes
    -- this ordering timestamp. Content edits still cannot arbitrarily set it.
    SELECT CASE WHEN NEW.source_process IS NOT OLD.source_process
        OR NEW.source_history_id IS NOT OLD.source_history_id
        THEN RAISE(ABORT, 'ITEM_CAPTURE_FACT_IMMUTABLE') END;
END;
CREATE TRIGGER items_delete_guard BEFORE DELETE ON items BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2
        OR cliphost_write_scope() IS NOT OLD.scope
        THEN RAISE(ABORT, 'ITEM_WRITE_SCOPE_MISMATCH') END;
END;

-- Migration may retain unknown legacy usage keys. Normal writes must match
-- the history/favorite prefix; they cannot change a key's identity.
CREATE TRIGGER usage_insert_guard BEFORE INSERT ON usage_info BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2
        OR cliphost_write_scope() IS NULL
        OR cliphost_write_scope() NOT IN ('history','favorite','migration') OR (
        cliphost_write_scope() IS NOT 'migration' AND cliphost_write_scope() IS NOT (
            CASE WHEN substr(NEW.id,1,10)='clip_item_' AND length(NEW.id)>10 THEN 'history'
                 WHEN substr(NEW.id,1,11)='favor_item_' AND length(NEW.id)>11 THEN 'favorite'
                 ELSE 'unclassified' END
        )
    ) THEN RAISE(ABORT, 'USAGE_WRITE_SCOPE_MISMATCH') END;
END;
CREATE TRIGGER usage_update_guard BEFORE UPDATE ON usage_info BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2 OR NEW.id IS NOT OLD.id
        OR cliphost_write_scope() IS NULL
        OR cliphost_write_scope() NOT IN ('history','favorite','migration') OR (
        cliphost_write_scope() IS NOT 'migration' AND cliphost_write_scope() IS NOT (
            CASE WHEN substr(OLD.id,1,10)='clip_item_' AND length(OLD.id)>10 THEN 'history'
                 WHEN substr(OLD.id,1,11)='favor_item_' AND length(OLD.id)>11 THEN 'favorite'
                 ELSE 'unclassified' END
        )
    ) THEN RAISE(ABORT, 'USAGE_WRITE_SCOPE_MISMATCH') END;
END;
CREATE TRIGGER usage_delete_guard BEFORE DELETE ON usage_info BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2
        OR cliphost_write_scope() IS NULL
        OR cliphost_write_scope() NOT IN ('history','favorite','migration') OR (
        cliphost_write_scope() IS NOT 'migration' AND cliphost_write_scope() IS NOT (
            CASE WHEN substr(OLD.id,1,10)='clip_item_' AND length(OLD.id)>10 THEN 'history'
                 WHEN substr(OLD.id,1,11)='favor_item_' AND length(OLD.id)>11 THEN 'favorite'
                 ELSE 'unclassified' END
        )
    ) THEN RAISE(ABORT, 'USAGE_WRITE_SCOPE_MISMATCH') END;
END;

CREATE TRIGGER store_info_insert_guard BEFORE INSERT ON store_info BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2 OR cliphost_write_scope() IS NOT 'migration'
        THEN RAISE(ABORT, 'FORMAT_WRITE_REQUIRES_MIGRATION') END;
END;
CREATE TRIGGER store_info_update_guard BEFORE UPDATE ON store_info BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2 OR cliphost_write_scope() IS NOT 'migration'
        THEN RAISE(ABORT, 'FORMAT_WRITE_REQUIRES_MIGRATION') END;
END;
CREATE TRIGGER store_info_delete_guard BEFORE DELETE ON store_info BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2 OR cliphost_write_scope() IS NOT 'migration'
        THEN RAISE(ABORT, 'FORMAT_WRITE_REQUIRES_MIGRATION') END;
END;
CREATE TRIGGER schema_migrations_insert_guard BEFORE INSERT ON schema_migrations BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2 OR cliphost_write_scope() IS NOT 'migration'
        THEN RAISE(ABORT, 'FORMAT_WRITE_REQUIRES_MIGRATION') END;
END;
CREATE TRIGGER schema_migrations_update_guard BEFORE UPDATE ON schema_migrations BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2 OR cliphost_write_scope() IS NOT 'migration'
        THEN RAISE(ABORT, 'FORMAT_WRITE_REQUIRES_MIGRATION') END;
END;
CREATE TRIGGER schema_migrations_delete_guard BEFORE DELETE ON schema_migrations BEGIN
    SELECT CASE WHEN cliphost_writer_generation() IS NOT 2 OR cliphost_write_scope() IS NOT 'migration'
        THEN RAISE(ABORT, 'FORMAT_WRITE_REQUIRES_MIGRATION') END;
END;

-- Repository checks still own per-kind payloads, real calendar values, graph
-- validity and compare-and-swap revision semantics. This DDL is not a complete
-- migrated database: existing auxiliary domains and their writer guards are
-- mandatory parts of the migration transaction described in storage-v2-core.md.
