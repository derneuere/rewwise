-- rewwise SQLite schema (version 1)
-- One soundbank per file. Round-trips losslessly through wwise_format::Soundbank.
--
-- High-level mapping:
--   Soundbank.sections[]                 -> sections rows (ordered by `ord`)
--   SectionBody::HIRC -> hirc_objects[]  -> hirc_objects rows (sections.body_json is NULL)
--   SectionBody::DIDX + DATA             -> wems rows (built on export)
--   Other SectionBody variants           -> serde-JSON in sections.body_json
--
-- Convention: body_json for non-HIRC sections stores the full SectionBody enum
-- (externally tagged, e.g. {"BKHD": {...}}). For hirc_objects.body_json we
-- store the HIRCObjectBody variant (e.g. {"Sound": {...}}). The magic / kind
-- columns are denormalized for queries; the JSON is authoritative on read.

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS sections (
    ord       INTEGER PRIMARY KEY,
    magic     TEXT NOT NULL,
    body_json TEXT
);

CREATE TABLE IF NOT EXISTS hirc_objects (
    section_ord   INTEGER NOT NULL REFERENCES sections(ord),
    ord           INTEGER NOT NULL,
    id_kind       TEXT NOT NULL CHECK (id_kind IN ('String','Hash')),
    id_value      TEXT NOT NULL,
    id_hash       INTEGER NOT NULL,
    label         TEXT,
    body_kind     TEXT NOT NULL,
    body_type     INTEGER NOT NULL,
    direct_parent INTEGER,
    override_bus  INTEGER,
    body_json     TEXT NOT NULL,
    PRIMARY KEY (section_ord, ord)
);

CREATE INDEX IF NOT EXISTS hirc_by_id_hash ON hirc_objects(id_hash);
CREATE INDEX IF NOT EXISTS hirc_by_label   ON hirc_objects(label);
CREATE INDEX IF NOT EXISTS hirc_by_kind    ON hirc_objects(body_kind);
CREATE INDEX IF NOT EXISTS hirc_by_parent  ON hirc_objects(direct_parent);

CREATE TABLE IF NOT EXISTS wems (
    id      INTEGER PRIMARY KEY,
    payload BLOB NOT NULL
);
