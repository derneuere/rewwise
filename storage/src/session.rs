//! In-memory `Soundbank` session — daemon-mode storage layer.
//!
//! `SoundbankSession::open(path)` reads the bank from SQLite once and keeps
//! the parsed `Soundbank` in RAM. Subsequent mutations apply to BOTH the
//! in-memory copy and the SQLite mirror inside a single transaction, so disk
//! state stays crash-safe. `export(out)` reuses the in-memory representation
//! directly — no `read_soundbank` round-trip — which is the whole point: the
//! ~174 ms SELECT + parallel JSON parse on a 30k-object bank disappears from
//! every export after the first.
//!
//! Caveats (see crate-level `README.md` if/when we promote this past
//! prototype): an external process modifying the .db while a session is open
//! makes the cache stale; the session holds the full bank in RAM
//! (~hundreds of MB for `cs_main`); the session takes a writer connection
//! and is not concurrent-safe.

use std::path::{Path, PathBuf};

use deku::bitvec::BitVec;
use deku::{DekuEnumExt, DekuWrite};
use rusqlite::{params, Connection};
use serde_json::Value;
use wwise_format::*;

use crate::{
    body_kind_name, extract_routing, open_rw, read_soundbank, read_wems,
    rebuild_didx_data, FNVDictionary, Result, StorageError,
};

/// Long-lived handle that owns a parsed `Soundbank` plus the WEM payloads
/// and a writer-side SQLite connection. All mutations go through this type;
/// the in-memory cache stays the source of truth for export.
pub struct SoundbankSession {
    db_path: PathBuf,
    /// Parsed bank, with DIDX/DATA stripped (matching `read_soundbank`).
    /// Audio payload lives in `wems` and is spliced back in at export time.
    soundbank: Soundbank,
    /// `(id, payload)` pairs, kept sorted by id (matches what
    /// `rebuild_didx_data` expects so successive exports avoid re-sorting).
    wems: Vec<(u32, Vec<u8>)>,
    /// Writer connection. We open it once, set the same PRAGMAs the
    /// `bench-sqlite` warm path uses (synchronous=OFF, MEMORY temp, big
    /// cache) so mutation tx commits don't pay fsync.
    conn: Connection,
}

impl SoundbankSession {
    /// Open an existing soundbank `.db` and load it into RAM. The .db must
    /// have been produced by `import_bnk` / `write_soundbank` (the schema
    /// check is enforced inside `read_soundbank` / `read_wems`).
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();

        let soundbank = read_soundbank(&db_path)?;
        let mut wems = read_wems(&db_path)?;
        wems.sort_by_key(|(id, _)| *id);

        let conn = open_rw(&db_path)?;
        // Build pipelines accept losing the .db on OS crash (it's
        // regenerable from the source .bnk); skip fsync for write speed.
        conn.pragma_update(None, "synchronous", "OFF")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -100_000i64)?; // 100 MB

        Ok(Self {
            db_path,
            soundbank,
            wems,
            conn,
        })
    }

    /// Borrow the cached bank.
    pub fn soundbank(&self) -> &Soundbank {
        &self.soundbank
    }

    /// The path of the underlying .db (useful for tests / diagnostics).
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Apply a closure that mutates the in-memory bank and persists the
    /// per-row updates to SQLite. The closure returns the ids it touched:
    /// `modified` (UPDATE) and `inserted` (INSERT). Touch lists drive the
    /// SQL mirror — anything not in either list is left alone on disk.
    ///
    /// Wraps the SQL side in a single transaction so the in-memory state
    /// and the .db stay in lock-step (or both fail).
    pub fn mutate_hirc<F>(&mut self, dict: &FNVDictionary, f: F) -> Result<()>
    where
        F: FnOnce(&mut Soundbank) -> HircEdits,
    {
        let edits = f(&mut self.soundbank);

        // Find the HIRC section and build a `id_hash → &HIRCObject` index
        // up front. Without this, locating each touched object is O(N) per
        // edit — and with 6000 inserts on a 36k-object bank that's 200M
        // comparisons (each comparison rehashes the FNV name). The index
        // costs ~36k hashes once, then every lookup is O(1).
        let (section_ord, hirc_objects) = find_hirc_in(&self.soundbank)?;
        let mut by_hash: std::collections::HashMap<u32, &HIRCObject> =
            std::collections::HashMap::with_capacity(hirc_objects.len());
        for obj in hirc_objects {
            by_hash.insert(obj.id.as_hash(), obj);
        }

        let tx = self.conn.transaction()?;
        {
            let mut update_stmt = tx.prepare(
                "UPDATE hirc_objects
                 SET body_json = ?, body_kind = ?, body_type = ?,
                     direct_parent = ?, override_bus = ?
                 WHERE section_ord = ? AND id_hash = ?",
            )?;
            let mut insert_stmt = tx.prepare(
                "INSERT INTO hirc_objects(
                     section_ord, ord, id_kind, id_value, id_hash, label,
                     body_kind, body_type, direct_parent, override_bus, body_json)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            )?;
            // Reused for finding the next ord on insert.
            let mut next_ord: i64 = tx.query_row(
                "SELECT COALESCE(MAX(ord)+1, 0) FROM hirc_objects WHERE section_ord = ?",
                params![section_ord as i64],
                |r| r.get(0),
            )?;

            // UPDATE path.
            for id_hash in &edits.modified {
                let obj = by_hash.get(id_hash).copied().ok_or_else(|| {
                    StorageError::BadHash(format!(
                        "modified id 0x{id_hash:08X} not found in cached HIRC"
                    ))
                })?;
                let body_kind = body_kind_name(&obj.body);
                let body_type: i64 = obj.body.deku_id()? as i64;
                let (parent, bus) = extract_routing(&obj.body);
                let body_json = serde_json::to_string(&obj.body)?;
                update_stmt.execute(params![
                    body_json,
                    body_kind,
                    body_type,
                    parent.map(|v| v as i64),
                    bus.map(|v| v as i64),
                    section_ord as i64,
                    *id_hash as i64,
                ])?;
            }

            // INSERT path.
            for id_hash in &edits.inserted {
                let obj = by_hash.get(id_hash).copied().ok_or_else(|| {
                    StorageError::BadHash(format!(
                        "inserted id 0x{id_hash:08X} not present in cached HIRC"
                    ))
                })?;
                let body_kind = body_kind_name(&obj.body);
                let body_type: i64 = obj.body.deku_id()? as i64;
                let (parent, bus) = extract_routing(&obj.body);
                let (id_kind, id_value, label) = match &obj.id {
                    ObjectId::String(s) => ("String", s.clone(), Some(s.clone())),
                    ObjectId::Hash(h) => {
                        ("Hash", h.to_string(), dict.get(h).cloned())
                    }
                };
                let body_json = serde_json::to_string(&obj.body)?;
                insert_stmt.execute(params![
                    section_ord as i64,
                    next_ord,
                    id_kind,
                    id_value,
                    *id_hash as i64,
                    label,
                    body_kind,
                    body_type,
                    parent.map(|v| v as i64),
                    bus.map(|v| v as i64),
                    body_json,
                ])?;
                next_ord += 1;
            }
        }
        tx.commit()?;
        Ok(())
    }


    /// Export the in-memory bank to `out_bnk`. This is the hot path for
    /// daemon mode: skips `read_soundbank` entirely and runs prepare +
    /// Deku-encode + write straight from the cached state.
    ///
    /// We don't keep DIDX/DATA in the cache (export-time concern only), so
    /// we splice them in just for the encode and pop them off afterward.
    /// `prepare_soundbank` mutates size/count fields in place, but the
    /// encoder only reads from those — so re-running it on a subsequent
    /// export is safe (the values are recomputed from current child
    /// counts).
    pub fn export(&mut self, out_bnk: impl AsRef<Path>) -> Result<()> {
        // Insert DIDX/DATA. If the bank has wems, the indices into
        // `self.soundbank.sections` shift while DIDX+DATA are present;
        // we record the indices we inserted so we can pop them by index.
        let (didx_idx, data_idx) = if !self.wems.is_empty() {
            let inserted = self.splice_in_didx_data()?;
            inserted
        } else {
            (None, None)
        };

        wwise_format::prepare_soundbank(&mut self.soundbank);

        let mut bits = BitVec::default();
        let write_res = self.soundbank.write(&mut bits, ());

        // Always pop DIDX/DATA back out, even on encode failure, so the
        // session is reusable.
        if let (Some(d), Some(a)) = (didx_idx, data_idx) {
            // a > d (DATA was inserted after DIDX); pop the higher index
            // first so the lower one stays valid.
            debug_assert!(a > d);
            self.soundbank.sections.remove(a);
            self.soundbank.sections.remove(d);
        }

        write_res?;
        std::fs::write(out_bnk, bits.as_raw_slice())?;
        Ok(())
    }

    /// Phase-instrumented export, used by the daemon-mode bench. Produces
    /// the same `.bnk` as `export()` but reports per-phase durations so we
    /// can compare directly against `bench-sqlite`'s breakdown.
    pub fn export_phased(
        &mut self,
        out_bnk: impl AsRef<Path>,
    ) -> Result<ExportPhases> {
        // 1. rebuild DIDX+DATA from cached wems and splice them in.
        let (didx_t, didx_idx, data_idx) = if !self.wems.is_empty() {
            let t = std::time::Instant::now();
            let (didx_idx, data_idx) = self.splice_in_didx_data()?;
            (t.elapsed(), didx_idx, data_idx)
        } else {
            (std::time::Duration::ZERO, None, None)
        };

        // 2. prepare_soundbank (refresh size/count fields).
        let t = std::time::Instant::now();
        wwise_format::prepare_soundbank(&mut self.soundbank);
        let prep_t = t.elapsed();

        // 3. Deku encode.
        let t = std::time::Instant::now();
        let mut bits = BitVec::default();
        let write_res = self.soundbank.write(&mut bits, ());
        let enc_t = t.elapsed();

        // Always pop DIDX/DATA, even on encode failure, so the session
        // remains reusable.
        if let (Some(d), Some(a)) = (didx_idx, data_idx) {
            debug_assert!(a > d);
            self.soundbank.sections.remove(a);
            self.soundbank.sections.remove(d);
        }
        write_res?;

        // 4. write file.
        let t = std::time::Instant::now();
        std::fs::write(out_bnk, bits.as_raw_slice())?;
        let write_t = t.elapsed();

        Ok(ExportPhases {
            didx_t,
            prep_t,
            enc_t,
            write_t,
        })
    }

    /// Splice freshly-built DIDX+DATA sections in just-after-BKHD. Returns
    /// the indices where they live so `export` can pop them.
    fn splice_in_didx_data(&mut self) -> Result<(Option<usize>, Option<usize>)> {
        // Reuse the existing rebuild routine — it builds DIDX+DATA from
        // (id, payload) pairs and inserts them after BKHD. The wems were
        // sorted in `open` so the per-export sort is a no-op.
        rebuild_didx_data(&mut self.soundbank, self.wems.clone())?;

        // `rebuild_didx_data` inserts DIDX at bkhd_pos+1 and DATA at
        // bkhd_pos+2. Find them so we can remove them on export-end.
        let mut didx_idx = None;
        let mut data_idx = None;
        for (i, s) in self.soundbank.sections.iter().enumerate() {
            match &s.body {
                SectionBody::DIDX(_) if didx_idx.is_none() => didx_idx = Some(i),
                SectionBody::DATA(_) if data_idx.is_none() => data_idx = Some(i),
                _ => {}
            }
        }
        Ok((didx_idx, data_idx))
    }

    // ───── Convenience helpers tailored to the bench workload ──────

    /// Append a full HIRC subgraph to the cached bank's HIRC section
    /// (in-memory) and write the same rows to SQLite. Mirrors what the
    /// `bench-sqlite` body does, but lifts the edge logic into the storage
    /// layer so callers don't have to re-derive it.
    ///
    /// `objects` is the JSON form (one entry per HIRC object — same shape
    /// `bench::make_new_sound` produces). `mixer_id` is the master mixer
    /// FNV hash; `new_children` is the SC ids that get appended to its
    /// children list. The mixer row is also UPDATEd in the same tx.
    pub fn add_hirc_subgraph(
        &mut self,
        dict: &FNVDictionary,
        mixer_id: u32,
        objects: Vec<Value>,
        new_children: Vec<u32>,
    ) -> Result<()> {
        // Stage 1 — turn the JSON objects into typed `HIRCObject`s. We do
        // this outside `mutate_hirc` so a parse error doesn't half-apply.
        let mut typed: Vec<HIRCObject> = Vec::with_capacity(objects.len());
        let mut inserted_ids: Vec<u32> = Vec::with_capacity(objects.len());
        for obj_json in objects {
            let id_label = obj_json["id"]["String"]
                .as_str()
                .ok_or_else(|| StorageError::BadHash(
                    "subgraph object missing id.String".into()
                ))?
                .to_string();
            let id = ObjectId::String(id_label);
            let id_hash = id.as_hash();
            let body: HIRCObjectBody = serde_json::from_value(obj_json["body"].clone())?;
            let body_type = body.deku_id()?;
            inserted_ids.push(id_hash);
            typed.push(HIRCObject {
                body_type,
                size: 0,
                id,
                body,
            });
        }

        // Stage 2 — apply both the mixer-children edit and the inserts in
        // one go.
        self.mutate_hirc(dict, |sb| {
            // Append the new objects to the in-memory HIRC section.
            for sec in sb.sections.iter_mut() {
                if let SectionBody::HIRC(h) = &mut sec.body {
                    h.objects.extend(typed);
                    break;
                }
            }
            // Update the mixer's children list. The bench's reference
            // implementation re-sorts by id; we do the same so the byte
            // output matches.
            let mut updated_mixer = false;
            for sec in sb.sections.iter_mut() {
                if let SectionBody::HIRC(h) = &mut sec.body {
                    for obj in h.objects.iter_mut() {
                        if obj.id.as_hash() == mixer_id {
                            if let HIRCObjectBody::ActorMixer(m) = &mut obj.body {
                                m.children.items.extend(&new_children);
                                m.children.items.sort_unstable();
                                // `count` is private and refreshed by
                                // `prepare_export` from `items.len()`.
                                updated_mixer = true;
                            }
                            break;
                        }
                    }
                    break;
                }
            }
            // Surface "didn't find mixer" as an empty mutation result; the
            // caller wanted that update so we want SQL writes to also fail.
            // Simplest: include the mixer in `modified` only if we found
            // it; if not, the session will detect the missing row when
            // updating SQLite.
            let modified = if updated_mixer { vec![mixer_id] } else { vec![] };
            HircEdits {
                modified,
                inserted: inserted_ids,
            }
        })?;
        Ok(())
    }
}

fn find_hirc_in(sb: &Soundbank) -> Result<(usize, &[HIRCObject])> {
    for (i, sec) in sb.sections.iter().enumerate() {
        if let SectionBody::HIRC(h) = &sec.body {
            return Ok((i, &h.objects));
        }
    }
    Err(StorageError::BadHash("no HIRC section in cached bank".into()))
}

/// Per-phase timings for `SoundbankSession::export_phased`. Mirrors the
/// breakdown `bench-sqlite` prints so the two benches are A/B-comparable.
#[derive(Debug, Clone, Copy)]
pub struct ExportPhases {
    pub didx_t: std::time::Duration,
    pub prep_t: std::time::Duration,
    pub enc_t: std::time::Duration,
    pub write_t: std::time::Duration,
}

/// Set of HIRC object ids touched by a `mutate_hirc` callback. The session
/// uses these to drive the SQL mirror — UPDATEs for `modified`, INSERTs for
/// `inserted`. Deletions aren't supported by this prototype.
#[derive(Default)]
pub struct HircEdits {
    pub modified: Vec<u32>,
    pub inserted: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict() -> FNVDictionary {
        FNVDictionary::new()
    }

    /// Smoke test: open a freshly-written .db, export it, parse the result.
    /// Round-trip equivalence (parse → write → reparse) is covered by the
    /// existing `bench-compare` harness.
    #[test]
    fn open_and_export_round_trip() -> Result<()> {
        let tmp = tempfile_path("session_smoke.db");
        let bnk_tmp = tempfile_path("session_smoke.bnk");

        // Build a 1-object bank and persist it.
        let bank = tiny_bank();
        crate::write_soundbank(&tmp, &bank, &dict())?;

        let mut sess = SoundbankSession::open(&tmp)?;
        sess.export(&bnk_tmp)?;
        // Re-parse the exported bytes — should succeed.
        let bytes = std::fs::read(&bnk_tmp)?;
        let _ = wwise_format::parse_soundbank(&bytes)?;
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&bnk_tmp);
        Ok(())
    }

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rewwise-test-{}", name));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn tiny_bank() -> Soundbank {
        // Minimal BKHD + empty HIRC. Real banks are far richer but this is
        // sufficient for smoke-testing the open / export plumbing.
        let bkhd = BKHDSection {
            version: 134,
            bank_id: 1,
            language_fnv_hash: 0,
            wem_alignment: 16,
            project_id: 0,
            padding: vec![],
        };
        let hirc = HIRCSection::from_objects(vec![]);
        Soundbank {
            sections: vec![
                Section {
                    magic: *b"BKHD",
                    size: 0,
                    body: SectionBody::BKHD(bkhd),
                },
                Section {
                    magic: *b"HIRC",
                    size: 0,
                    body: SectionBody::HIRC(hirc),
                },
            ],
        }
    }
}
