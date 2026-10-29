-- Storage v2 design draft. Not wired into the product or a production migration.
-- Run on an empty isolated SQLite database. Requires JSON functions and STRICT.
-- The migration runner owns BEGIN/COMMIT; every connection enables foreign keys.
PRAGMA foreign_keys = ON;

CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    checksum TEXT NOT NULL,
    applied_at TEXT NOT NULL
) STRICT;

-- A favorite is an independent content snapshot, not a flag on a history row.
-- Scope-qualified IDs preserve both old ID spaces without inventing aliases.
CREATE TABLE items (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL CHECK (scope IN ('history', 'favorite')),
    id TEXT NOT NULL CHECK (length(id) > 0),
    kind TEXT NOT NULL CHECK (kind IN (
        'text', 'html', 'im_rich', 'image', 'files', 'link',
        'command', 'steps', 'action', 'folder'
    )),
    title TEXT, -- User override; NULL, empty and source title remain distinguishable.
    icon_override TEXT,
    description TEXT NOT NULL DEFAULT '',
    payload_version INTEGER NOT NULL DEFAULT 1 CHECK (payload_version > 0),
    payload_json TEXT NOT NULL CHECK (
        json_valid(payload_json) AND json_type(payload_json) = 'object'
    ),
    search_text TEXT NOT NULL DEFAULT '', -- Rebuildable, never the original payload.
    dedupe_key TEXT NOT NULL DEFAULT '',  -- Not unique: intentional duplicates exist.
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
    usage_count INTEGER NOT NULL DEFAULT 0 CHECK (usage_count >= 0),
    last_used_at TEXT CHECK (last_used_at IS NULL OR (
        length(last_used_at) = 30 AND substr(last_used_at, 11, 1) = 'T'
        AND substr(last_used_at, 20, 1) = '.' AND substr(last_used_at, 30, 1) = 'Z'
    )),
    revision TEXT NOT NULL CHECK (length(revision) > 0),
    owner_parent_id TEXT,
    children_json TEXT CHECK (children_json IS NULL OR (
        json_valid(children_json) AND json_type(children_json) = 'array'
    )),
    system_role TEXT CHECK (system_role IS NULL OR system_role = 'favorite_root'),
    source_history_id TEXT, -- Provenance only; history retention must not delete favorites.
    UNIQUE (scope, id),
    FOREIGN KEY (scope, owner_parent_id) REFERENCES items(scope, id)
        DEFERRABLE INITIALLY DEFERRED,
    CHECK (owner_parent_id IS NULL OR owner_parent_id <> id),
    CHECK (scope = 'favorite' OR (
        owner_parent_id IS NULL AND children_json IS NULL
        AND system_role IS NULL AND source_history_id IS NULL
    )),
    CHECK (scope = 'history' OR is_pinned = 0),
    CHECK (
        (scope = 'favorite' AND kind = 'folder' AND children_json IS NOT NULL)
        OR (kind <> 'folder' AND children_json IS NULL)
    ),
    CHECK (system_role IS NULL OR (
        scope = 'favorite' AND kind = 'folder' AND owner_parent_id IS NULL
    ))
) STRICT;

CREATE UNIQUE INDEX items_one_favorite_root ON items(system_role)
    WHERE system_role = 'favorite_root';
CREATE INDEX items_history_order ON items(
    is_pinned DESC, created_at DESC, sub_index ASC, seq ASC
) WHERE scope = 'history';
CREATE INDEX items_history_kind_order ON items(
    kind, is_pinned DESC, created_at DESC, sub_index ASC, seq ASC
) WHERE scope = 'history';
CREATE INDEX items_history_dedupe ON items(dedupe_key, created_at DESC)
    WHERE scope = 'history';
CREATE INDEX items_owner ON items(scope, owner_parent_id);
CREATE INDEX items_usage ON items(scope, last_used_at DESC, seq ASC);

CREATE TABLE store_info (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    dataset_id TEXT NOT NULL CHECK (length(dataset_id) > 0),
    format_version INTEGER NOT NULL CHECK (format_version = 2),
    favorite_scope TEXT NOT NULL DEFAULT 'favorite' CHECK (favorite_scope = 'favorite'),
    favorite_root_id TEXT NOT NULL,
    FOREIGN KEY (favorite_scope, favorite_root_id) REFERENCES items(scope, id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE TABLE assets (
    id TEXT PRIMARY KEY CHECK (
        length(id) = 64 AND id NOT GLOB '*[^0-9a-f]*'
    ), -- SHA-256; path is assets/<first two characters>/<id>.
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    media_type TEXT NOT NULL,
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE item_assets (
    scope TEXT NOT NULL,
    item_id TEXT NOT NULL,
    slot TEXT NOT NULL CHECK (slot IN (
        'image', 'html', 'im', 'content_icon', 'override_icon'
    )),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    asset_id TEXT REFERENCES assets(id),
    original_locator TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'ready' CHECK (status IN ('ready', 'missing', 'unresolved')),
    PRIMARY KEY (scope, item_id, slot, ordinal),
    FOREIGN KEY (scope, item_id) REFERENCES items(scope, id) ON DELETE CASCADE,
    CHECK (
        (status = 'ready' AND asset_id IS NOT NULL)
        OR (status <> 'ready' AND asset_id IS NULL AND length(original_locator) > 0)
    )
) STRICT;
CREATE INDEX item_assets_asset ON item_assets(asset_id);

-- Persistent preferences, filters and text actions have one authority here.
-- JSON files are explicit exports or migration inputs, never another live writer.
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value_version INTEGER NOT NULL DEFAULT 1 CHECK (value_version > 0),
    value_json TEXT NOT NULL CHECK (json_valid(value_json)),
    revision TEXT NOT NULL CHECK (length(revision) > 0)
) STRICT;

CREATE TABLE setting_assets (
    setting_key TEXT NOT NULL REFERENCES settings(key) ON DELETE CASCADE,
    slot TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    asset_id TEXT NOT NULL REFERENCES assets(id),
    PRIMARY KEY (setting_key, slot, ordinal)
) STRICT;
CREATE INDEX setting_assets_asset ON setting_assets(asset_id);

-- Favorite graph validity, per-kind payloads, normalized calendar values and
-- payload/asset-map completeness are checked by the repository in its write
-- transaction. JSON arrays intentionally do not have per-element SQL FKs.
-- LAN messages, attachments and transfer receipts are domain tables specified
-- in storage-v2.md; this file is the executable core, not a full migration.
