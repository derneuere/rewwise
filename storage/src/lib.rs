//! SQLite-backed persistence for `wwise_format::Soundbank`.
//!
//! See [`schema.sql`] for the on-disk layout. The high-level entry points
//! (`import_bnk` / `export_bnk`) mirror what `bnk2json` does today: a `.bnk`
//! is split into one row per HIRC object plus a `wems` table holding the
//! audio payload, and the round-trip back through `prepare_soundbank` +
//! Deku produces a byte-identical (modulo size/length recompute) `.bnk`.

use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Seek, Write};
use std::path::Path;

use deku::bitvec::BitVec;
use deku::{DekuEnumExt, DekuWrite};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use wwise_format::*;

pub type FNVDictionary = HashMap<u32, String>;

pub const SCHEMA_VERSION: i32 = 1;
/// `b"REWU"` little-endian — pick something distinctive so that
/// `sqlite3 file.db 'pragma application_id'` confirms the format.
pub const APPLICATION_ID: i32 = 0x52455755u32 as i32;

const SCHEMA_SQL: &str = include_str!("schema.sql");

#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("schema version mismatch: file is v{found}, this build expects v{expected}")]
    VersionMismatch { found: i32, expected: i32 },
    #[error("file is not a rewwise soundbank database (application_id 0x{found:08X})")]
    BadApplicationId { found: i32 },
    #[error("invalid id_kind {0:?}")]
    InvalidIdKind(String),
    #[error("could not parse hash {0:?} from id_value")]
    BadHash(String),
    #[error("soundbank has no BKHD section — cannot rebuild WEM payload")]
    MissingBkhd,
    #[error(transparent)]
    Deku(#[from] deku::DekuError),
}

pub type Result<T> = std::result::Result<T, StorageError>;

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

fn open_rw(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

fn open_ro(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    conn.pragma_update(None, "application_id", APPLICATION_ID)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn check_schema(conn: &Connection) -> Result<()> {
    let app_id: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if app_id != APPLICATION_ID {
        return Err(StorageError::BadApplicationId { found: app_id });
    }
    let ver: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if ver != SCHEMA_VERSION {
        return Err(StorageError::VersionMismatch {
            found: ver,
            expected: SCHEMA_VERSION,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Low-level: Soundbank metadata persistence
// ---------------------------------------------------------------------------

/// Persist the metadata + sections of a soundbank. DIDX and DATA are skipped
/// here — the audio payload belongs in the `wems` table; call [`write_wems`].
pub fn write_soundbank(
    db_path: &Path,
    soundbank: &Soundbank,
    dictionary: &FNVDictionary,
) -> Result<()> {
    if db_path.exists() {
        // Drop stale WAL/SHM files alongside.
        let _ = fs::remove_file(db_path);
        let _ = fs::remove_file(db_path.with_extension("db-wal"));
        let _ = fs::remove_file(db_path.with_extension("db-shm"));
    }
    let mut conn = open_rw(db_path)?;
    init_schema(&conn)?;

    let tx = conn.transaction()?;
    {
        let mut sec_stmt = tx.prepare(
            "INSERT INTO sections(ord, magic, body_json) VALUES (?,?,?)",
        )?;
        let mut hirc_stmt = tx.prepare(
            "INSERT INTO hirc_objects(
                 section_ord, ord, id_kind, id_value, id_hash, label,
                 body_kind, body_type, direct_parent, override_bus, body_json)
             VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        )?;

        for (section_ord, section) in soundbank.sections.iter().enumerate() {
            let magic_str = magic_to_string(&section.magic);
            match &section.body {
                SectionBody::DIDX(_) | SectionBody::DATA(_) => {
                    // Audio payload lives in `wems`; rebuilt at export time.
                    continue;
                }
                SectionBody::HIRC(hirc) => {
                    sec_stmt.execute(params![
                        section_ord as i64,
                        magic_str,
                        Option::<String>::None
                    ])?;
                    for (ord, obj) in hirc.objects.iter().enumerate() {
                        write_hirc_row(&mut hirc_stmt, section_ord, ord, obj, dictionary)?;
                    }
                }
                _ => {
                    let body_json = serde_json::to_string(&section.body)?;
                    sec_stmt.execute(params![section_ord as i64, magic_str, Some(body_json)])?;
                }
            }
        }

        let mut meta_stmt =
            tx.prepare("INSERT OR REPLACE INTO meta(key, value) VALUES (?,?)")?;
        meta_stmt.execute(params![
            "created_by",
            concat!("rewwise ", env!("CARGO_PKG_VERSION"))
        ])?;
    }
    tx.commit()?;
    Ok(())
}

fn write_hirc_row(
    stmt: &mut rusqlite::Statement<'_>,
    section_ord: usize,
    ord: usize,
    obj: &HIRCObject,
    dictionary: &FNVDictionary,
) -> Result<()> {
    let body_kind = body_kind_name(&obj.body);
    let body_type: i64 = obj.body.deku_id()? as i64;
    let id_hash = obj.id.as_hash();
    let (id_kind, id_value) = match &obj.id {
        ObjectId::String(s) => ("String", s.clone()),
        ObjectId::Hash(h) => ("Hash", h.to_string()),
    };
    let label: Option<String> = match &obj.id {
        ObjectId::String(s) => Some(s.clone()),
        ObjectId::Hash(h) => dictionary.get(h).cloned(),
    };
    let (parent, bus) = extract_routing(&obj.body);
    let body_json = serde_json::to_string(&obj.body)?;
    stmt.execute(params![
        section_ord as i64,
        ord as i64,
        id_kind,
        id_value,
        id_hash as i64,
        label,
        body_kind,
        body_type,
        parent.map(|p| p as i64),
        bus.map(|b| b as i64),
        body_json,
    ])?;
    Ok(())
}

/// Reverse of [`write_soundbank`]. The returned `Soundbank` has no DIDX/DATA;
/// those are rebuilt by [`export_bnk`] from the `wems` table.
pub fn read_soundbank(db_path: &Path) -> Result<Soundbank> {
    let conn = open_ro(db_path)?;
    check_schema(&conn)?;

    let mut sec_stmt = conn.prepare("SELECT ord, magic, body_json FROM sections ORDER BY ord ASC")?;
    let row_iter = sec_stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)? as usize,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;

    let mut sections: Vec<Section> = Vec::new();
    for row in row_iter {
        let (ord, magic, body_json) = row?;
        let body = match body_json {
            Some(j) => serde_json::from_str::<SectionBody>(&j)?,
            None => SectionBody::HIRC(load_hirc_section(&conn, ord)?),
        };
        sections.push(Section {
            magic: string_to_magic(&magic),
            size: 0,
            body,
            cached_body: None,
        });
    }

    Ok(Soundbank { sections })
}

fn load_hirc_section(conn: &Connection, section_ord: usize) -> Result<HIRCSection> {
    use rayon::prelude::*;

    // SELECT all rows first, then parallelize the per-row JSON parse. The
    // parse dominates load time (≈75% on a 30k-object bank), and serde_json
    // is thread-safe so this scales well across cores.
    let mut stmt = conn.prepare(
        "SELECT id_kind, id_value, body_json
         FROM hirc_objects
         WHERE section_ord = ?
         ORDER BY ord ASC",
    )?;
    let raw: Vec<(String, String, String)> = stmt
        .query_map(params![section_ord as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let objects: Vec<HIRCObject> = raw
        .into_par_iter()
        .map(|(id_kind, id_value, body_json)| -> Result<HIRCObject> {
            let id = match id_kind.as_str() {
                "String" => ObjectId::String(id_value),
                "Hash" => ObjectId::Hash(
                    id_value
                        .parse::<u32>()
                        .map_err(|_| StorageError::BadHash(id_value))?,
                ),
                other => return Err(StorageError::InvalidIdKind(other.to_string())),
            };
            let body: HIRCObjectBody = serde_json::from_str(&body_json)?;
            Ok(HIRCObject {
                body_type: body.deku_id()?,
                size: 0,
                id,
                body,
                cached_body: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(HIRCSection::from_objects(objects))
}

// ---------------------------------------------------------------------------
// WEM payload table
// ---------------------------------------------------------------------------

pub fn write_wems<I>(db_path: &Path, items: I) -> Result<()>
where
    I: IntoIterator<Item = (u32, Vec<u8>)>,
{
    let mut conn = open_rw(db_path)?;
    check_schema(&conn)?;
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare("INSERT OR REPLACE INTO wems(id, payload) VALUES (?,?)")?;
        for (id, bytes) in items {
            stmt.execute(params![id as i64, bytes])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn read_wems(db_path: &Path) -> Result<Vec<(u32, Vec<u8>)>> {
    let conn = open_ro(db_path)?;
    check_schema(&conn)?;
    let mut stmt = conn.prepare("SELECT id, payload FROM wems ORDER BY id ASC")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)? as u32, r.get::<_, Vec<u8>>(1)?))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn copy_wems(src_db: &Path, dst_db: &Path, ids: &[u32]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let src = open_ro(src_db)?;
    check_schema(&src)?;
    let mut dst = open_rw(dst_db)?;
    check_schema(&dst)?;

    let tx = dst.transaction()?;
    let mut copied = 0usize;
    {
        let mut select = src.prepare("SELECT payload FROM wems WHERE id = ?")?;
        let mut insert = tx.prepare("INSERT OR REPLACE INTO wems(id, payload) VALUES (?,?)")?;
        for id in ids {
            let bytes: Option<Vec<u8>> = select
                .query_row(params![*id as i64], |r| r.get::<_, Vec<u8>>(0))
                .optional()?;
            if let Some(bytes) = bytes {
                insert.execute(params![*id as i64, bytes])?;
                copied += 1;
            }
        }
    }
    tx.commit()?;
    Ok(copied)
}

// ---------------------------------------------------------------------------
// High-level: round-trip with `.bnk`
// ---------------------------------------------------------------------------

/// Parse a `.bnk` and persist its contents to a fresh `.db`.
///
/// HIRC `ObjectId::Hash` values that are present in `dictionary` are upgraded
/// to `ObjectId::String`, matching what `bnk2json` does for its JSON output.
pub fn import_bnk(bnk_path: &Path, db_path: &Path, dictionary: &FNVDictionary) -> Result<()> {
    let bytes = fs::read(bnk_path)?;
    let mut soundbank = wwise_format::parse_soundbank(&bytes)?;

    // Replace numeric ObjectIds with names where the dictionary has them.
    if let Some(hirc) = soundbank.sections.iter_mut().find_map(|s| match &mut s.body {
        SectionBody::HIRC(h) => Some(h),
        _ => None,
    }) {
        for object in hirc.objects.iter_mut() {
            object.id = match dictionary.get(&object.id.as_hash()) {
                Some(s) => ObjectId::String(s.to_string()),
                None => object.id.clone(),
            };
        }
    }

    let wems = extract_wems(&soundbank);
    soundbank
        .sections
        .retain(|s| !matches!(&s.body, SectionBody::DIDX(_) | SectionBody::DATA(_)));

    write_soundbank(db_path, &soundbank, dictionary)?;
    if !wems.is_empty() {
        write_wems(db_path, wems)?;
    }
    Ok(())
}

fn extract_wems(soundbank: &Soundbank) -> Vec<(u32, Vec<u8>)> {
    let didx = soundbank.sections.iter().find_map(|s| match &s.body {
        SectionBody::DIDX(d) => Some(d),
        _ => None,
    });
    let data = soundbank.sections.iter().find_map(|s| match &s.body {
        SectionBody::DATA(d) => Some(d),
        _ => None,
    });
    let (didx, data) = match (didx, data) {
        (Some(d), Some(da)) => (d, da),
        _ => return Vec::new(),
    };
    didx.descriptors
        .iter()
        .map(|desc| {
            let start = desc.offset as usize;
            let end = start + desc.size as usize;
            (desc.id, data.data[start..end].to_vec())
        })
        .collect()
}

/// Reconstruct a `.bnk` from the database.
pub fn export_bnk(db_path: &Path, bnk_path: &Path) -> Result<()> {
    let mut soundbank = read_soundbank(db_path)?;
    let wems = read_wems(db_path)?;

    if !wems.is_empty() {
        rebuild_didx_data(&mut soundbank, wems)?;
    }

    wwise_format::prepare_soundbank(&mut soundbank);

    let mut bits = BitVec::default();
    soundbank.write(&mut bits, ())?;
    fs::write(bnk_path, bits.as_raw_slice())?;
    Ok(())
}

/// Test-only re-export so the bench binary can call the otherwise-private
/// `rebuild_didx_data` directly to time it in isolation.
#[doc(hidden)]
pub fn rebuild_didx_data_for_bench(
    sb: &mut Soundbank,
    wems: Vec<(u32, Vec<u8>)>,
) -> Result<()> {
    rebuild_didx_data(sb, wems)
}

/// Mirrors the descriptor/data assembly in `format/src/bin/bnk2json.rs`.
fn rebuild_didx_data(soundbank: &mut Soundbank, mut wems: Vec<(u32, Vec<u8>)>) -> Result<()> {
    let wem_alignment = soundbank
        .sections
        .iter()
        .find_map(|s| match &s.body {
            SectionBody::BKHD(b) => Some(b.wem_alignment),
            _ => None,
        })
        .ok_or(StorageError::MissingBkhd)?;

    wems.sort_by_key(|(id, _)| *id);

    let mut descriptors = Vec::with_capacity(wems.len());
    let mut data_buf: Vec<u8> = Vec::new();
    let mut cursor = Cursor::new(&mut data_buf);

    for (i, (id, bytes)) in wems.iter().enumerate() {
        let offset = cursor.stream_position()? as u32;
        cursor.write_all(bytes)?;

        let current = cursor.stream_position()? as u32;
        let padded = (current + wem_alignment - 1) & !(wem_alignment - 1);

        // Last entry has no trailing pad — this matches the existing repacker.
        if i != wems.len() - 1 {
            for _ in 0..(padded - current) {
                cursor.write_all(&[0])?;
            }
        }

        descriptors.push(DIDXDescriptor {
            id: *id,
            offset,
            size: bytes.len() as u32,
        });
    }

    let didx = DIDXSection { descriptors };
    let data_section = DATASection { data: data_buf };

    let bkhd_pos = soundbank
        .sections
        .iter()
        .position(|s| matches!(&s.body, SectionBody::BKHD(_)))
        .ok_or(StorageError::MissingBkhd)?;

    soundbank.sections.insert(
        bkhd_pos + 1,
        Section {
            magic: [0; 4],
            size: 0,
            body: SectionBody::DIDX(didx),
            cached_body: None,
        },
    );
    soundbank.sections.insert(
        bkhd_pos + 2,
        Section {
            magic: [0; 4],
            size: 0,
            body: SectionBody::DATA(data_section),
            cached_body: None,
        },
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Editor convenience queries
// ---------------------------------------------------------------------------

/// Look up an HIRC object by exact label match (e.g. `"Play_c211006013"`).
pub fn find_id_by_label(db_path: &Path, label: &str) -> Result<Option<u32>> {
    let conn = open_ro(db_path)?;
    check_schema(&conn)?;
    let mut stmt =
        conn.prepare("SELECT id_hash FROM hirc_objects WHERE label = ? LIMIT 1")?;
    let res: Option<i64> = stmt
        .query_row(params![label], |r| r.get(0))
        .optional()?;
    Ok(res.map(|v| v as u32))
}

#[derive(Debug, Clone)]
pub struct ObjectSummary {
    pub id_hash: u32,
    pub label: Option<String>,
    pub body_kind: String,
    pub direct_parent: Option<u32>,
    pub override_bus: Option<u32>,
}

pub fn list_objects(
    db_path: &Path,
    body_kind: Option<&str>,
    name_like: Option<&str>,
) -> Result<Vec<ObjectSummary>> {
    list_objects_capped(db_path, body_kind, name_like, i64::MAX)
}

/// Variant that returns at most `limit` rows. Used by the web viewer to keep
/// huge banks (`cs_main` has 30k+ objects) from rendering 30 MB of HTML.
pub fn list_objects_capped(
    db_path: &Path,
    body_kind: Option<&str>,
    name_like: Option<&str>,
    limit: i64,
) -> Result<Vec<ObjectSummary>> {
    let conn = open_ro(db_path)?;
    check_schema(&conn)?;

    let kind_pat = body_kind.unwrap_or("%").to_string();
    let label_pat = name_like.unwrap_or("%").to_string();

    let mut stmt = conn.prepare(
        "SELECT id_hash, label, body_kind, direct_parent, override_bus
         FROM hirc_objects
         WHERE body_kind LIKE :kind
           AND COALESCE(label, '') LIKE :label
         ORDER BY label, id_hash
         LIMIT :limit",
    )?;
    let iter = stmt.query_map(
        rusqlite::named_params! {
            ":kind": kind_pat,
            ":label": label_pat,
            ":limit": limit,
        },
        |r| {
            Ok(ObjectSummary {
                id_hash: r.get::<_, i64>(0)? as u32,
                label: r.get::<_, Option<String>>(1)?,
                body_kind: r.get::<_, String>(2)?,
                direct_parent: r.get::<_, Option<i64>>(3)?.map(|v| v as u32),
                override_bus: r.get::<_, Option<i64>>(4)?.map(|v| v as u32),
            })
        },
    )?;
    iter.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Total number of HIRC objects in the bank — used by the viewer header.
pub fn count_objects(db_path: &Path) -> Result<u64> {
    let conn = open_ro(db_path)?;
    check_schema(&conn)?;
    let n: i64 =
        conn.query_row("SELECT COUNT(*) FROM hirc_objects", [], |r| r.get(0))?;
    Ok(n as u64)
}

/// Number of rows that would match the given filters, ignoring any limit.
pub fn count_objects_filtered(
    db_path: &Path,
    body_kind: Option<&str>,
    name_like: Option<&str>,
) -> Result<u64> {
    let conn = open_ro(db_path)?;
    check_schema(&conn)?;
    let kind_pat = body_kind.unwrap_or("%").to_string();
    let label_pat = name_like.unwrap_or("%").to_string();
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM hirc_objects
         WHERE body_kind LIKE :kind
           AND COALESCE(label, '') LIKE :label",
        rusqlite::named_params! { ":kind": kind_pat, ":label": label_pat },
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

/// Look up every HIRC object whose `direct_parent` equals `parent_id`. This
/// is the "Sounds attached to this mixer / SC" reverse query — useful in the
/// viewer's referenced-by panel.
pub fn list_children_of(db_path: &Path, parent_id: u32) -> Result<Vec<ObjectSummary>> {
    let conn = open_ro(db_path)?;
    check_schema(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT id_hash, label, body_kind, direct_parent, override_bus
         FROM hirc_objects
         WHERE direct_parent = ?
         ORDER BY body_kind, label, id_hash",
    )?;
    let iter = stmt.query_map(rusqlite::params![parent_id as i64], |r| {
        Ok(ObjectSummary {
            id_hash: r.get::<_, i64>(0)? as u32,
            label: r.get::<_, Option<String>>(1)?,
            body_kind: r.get::<_, String>(2)?,
            direct_parent: r.get::<_, Option<i64>>(3)?.map(|v| v as u32),
            override_bus: r.get::<_, Option<i64>>(4)?.map(|v| v as u32),
        })
    })?;
    iter.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn body_kind_name(body: &HIRCObjectBody) -> &'static str {
    use HIRCObjectBody::*;
    match body {
        State(_) => "State",
        Sound(_) => "Sound",
        Action(_) => "Action",
        Event(_) => "Event",
        RandomSequenceContainer(_) => "RandomSequenceContainer",
        SwitchContainer(_) => "SwitchContainer",
        ActorMixer(_) => "ActorMixer",
        Bus(_) => "Bus",
        LayerContainer(_) => "LayerContainer",
        MusicSegment(_) => "MusicSegment",
        MusicTrack(_) => "MusicTrack",
        MusicSwitchContainer(_) => "MusicSwitchContainer",
        MusicRandomSequenceContainer(_) => "MusicRandomSequenceContainer",
        Attenuation(_) => "Attenuation",
        DialogueEvent(_) => "DialogueEvent",
        EffectShareSet(_) => "EffectShareSet",
        EffectCustom(_) => "EffectCustom",
        AuxiliaryBus(_) => "AuxiliaryBus",
        LFOModulator(_) => "LFOModulator",
        EnvelopeModulator(_) => "EnvelopeModulator",
        AudioDevice(_) => "AudioDevice",
        TimeModulator(_) => "TimeModulator",
    }
}

fn extract_routing(body: &HIRCObjectBody) -> (Option<u32>, Option<u32>) {
    use HIRCObjectBody::*;
    match body {
        Sound(b) => (
            Some(b.node_base_params.direct_parent_id),
            Some(b.node_base_params.override_bus_id),
        ),
        RandomSequenceContainer(b) => (
            Some(b.node_base_params.direct_parent_id),
            Some(b.node_base_params.override_bus_id),
        ),
        SwitchContainer(b) => (
            Some(b.node_base_params.direct_parent_id),
            Some(b.node_base_params.override_bus_id),
        ),
        ActorMixer(b) => (
            Some(b.node_base_params.direct_parent_id),
            Some(b.node_base_params.override_bus_id),
        ),
        LayerContainer(b) => (
            Some(b.node_base_params.direct_parent_id),
            Some(b.node_base_params.override_bus_id),
        ),
        MusicSegment(b) => (
            Some(b.music_node_params.node_base_params.direct_parent_id),
            Some(b.music_node_params.node_base_params.override_bus_id),
        ),
        MusicTrack(b) => (
            Some(b.node_base_params.direct_parent_id),
            Some(b.node_base_params.override_bus_id),
        ),
        MusicSwitchContainer(b) => (
            Some(
                b.music_trans_node_params
                    .music_node_params
                    .node_base_params
                    .direct_parent_id,
            ),
            Some(
                b.music_trans_node_params
                    .music_node_params
                    .node_base_params
                    .override_bus_id,
            ),
        ),
        MusicRandomSequenceContainer(b) => (
            Some(
                b.music_trans_node_params
                    .music_node_params
                    .node_base_params
                    .direct_parent_id,
            ),
            Some(
                b.music_trans_node_params
                    .music_node_params
                    .node_base_params
                    .override_bus_id,
            ),
        ),
        Bus(b) => (None, Some(b.initial_values.override_bus_id)),
        AuxiliaryBus(b) => (None, Some(b.initial_values.override_bus_id)),
        _ => (None, None),
    }
}

fn magic_to_string(m: &[u8; 4]) -> String {
    String::from_utf8_lossy(m).into_owned()
}

fn string_to_magic(s: &str) -> [u8; 4] {
    let bytes = s.as_bytes();
    let mut out = [0u8; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        if let Some(&b) = bytes.get(i) {
            *slot = b;
        }
    }
    out
}

/// Parse a dictionary file (one symbol per line, blank/`#`-comment lines
/// ignored). Re-exported here so the editor and bnk2json don't have to
/// duplicate the parser.
pub fn parse_dictionary(input: &str) -> FNVDictionary {
    input
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| (ObjectId::String(l.to_string()).as_hash(), l.to_string()))
        .collect()
}
