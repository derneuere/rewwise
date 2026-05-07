//! SQLite-style benchmark — surgical edits against `cs_main.db`.
//!
//!   .bnk → bnk2sqlite (one-time, cached)  → cs_main.db
//!   open db → SELECT mixer body_json (indexed) → patch one row → INSERT N*6
//!   bnk-edit export → .bnk
//!
//! The `--cold` flag controls whether the bnk → db conversion is timed
//! (first run) or skipped because the db is already on disk (steady state,
//! which is what JortPob would amortize across iterative builds if they
//! switched).

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use bench::{make_new_sound, now, Phases};
use clap::Parser;
use rusqlite::params;
use serde_json::Value;

#[derive(Parser)]
#[command(about = "SQLite-style: surgical UPDATE + bulk INSERT against a cached .db")]
struct Args {
    #[arg(long)]
    bnk_in: PathBuf,
    #[arg(long)]
    bnk_out: PathBuf,
    /// Where the SQLite cache lives. Created via `import_bnk` if missing.
    #[arg(long)]
    db_path: PathBuf,
    /// FNV hash of the master mixer to splice new SCs into.
    #[arg(long)]
    master_mixer: u32,
    /// Number of new sounds (and their associated subgraph) to add.
    #[arg(long, default_value_t = 100)]
    n: u32,
    /// If set, treat the .db as cold and rebuild it from bnk_in (charges the
    /// import cost into the run). Otherwise the db is assumed cached and
    /// the bench measures the per-build delta only.
    #[arg(long)]
    cold: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut phases = Phases::new();
    let dict = wwise_storage::FNVDictionary::new();

    // ── PHASE 0: ensure the .db exists. Cold runs pay the import cost; ──
    // warm runs (the iterative case) skip it.
    if args.cold || !args.db_path.exists() {
        if args.db_path.exists() {
            fs::remove_file(&args.db_path).ok();
            fs::remove_file(args.db_path.with_extension("db-wal")).ok();
            fs::remove_file(args.db_path.with_extension("db-shm")).ok();
        }
        let t = now();
        wwise_storage::import_bnk(&args.bnk_in, &args.db_path, &dict)
            .context("import_bnk (cold)")?;
        phases.record("bnk → .db (cold import)", t.elapsed());
    }

    // ── PHASE 1: open db, SELECT mixer body_json, patch, UPDATE ────────
    let t = now();
    let mut conn = rusqlite::Connection::open(&args.db_path)?;
    // For a build pipeline, db corruption on OS crash is acceptable — the
    // .db is always rebuildable from the source .bnk. Drop the fsync cost.
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -100_000i64)?; // 100 MB
    phases.record("open SQLite", t.elapsed());

    let t = now();
    let tx = conn.transaction()?;
    phases.record("BEGIN tx", t.elapsed());

    let t = now();
    let mixer_json: String = tx
        .query_row(
            "SELECT body_json FROM hirc_objects WHERE id_hash = ?",
            [args.master_mixer as i64],
            |r| r.get(0),
        )
        .context("SELECT master mixer (no row at that id_hash)")?;
    let mut mixer_body: Value = serde_json::from_str(&mixer_json)?;
    phases.record("SELECT + parse mixer", t.elapsed());

    let t = now();
    // Generate N new sound subgraphs, in-memory.
    // Row tuple: (id_label, id_hash, body_kind, body_value, direct_parent, override_bus)
    let mut new_rows: Vec<(String, u32, &'static str, Value, Option<u32>, Option<u32>)> =
        Vec::with_capacity(args.n as usize * 6);
    let mut new_children: Vec<u32> = Vec::with_capacity(args.n as usize);
    let mut wem_seq: u32 = 900_000_000;
    for i in 0..args.n {
        let ns = make_new_sound(i, args.master_mixer, wem_seq);
        wem_seq += 1;
        new_children.push(ns.sc_id);

        // Each object becomes one row. body_kind is the externally-tagged
        // enum variant name; direct_parent / override_bus we extract for
        // the indexed columns when present.
        for obj in ns.objects {
            let id_label = obj["id"]["String"].as_str().unwrap_or("").to_string();
            // Hash the LABEL (matches Deku's writer) — not the numeric form
            // of any pre-computed hash we might have on hand.
            let id_hash = wwise_format::ObjectId::String(id_label.clone()).as_hash();
            let (kind, body) = obj["body"]
                .as_object()
                .and_then(|o| o.iter().next())
                .map(|(k, v)| (k.clone(), v.clone()))
                .ok_or_else(|| anyhow::anyhow!("object body missing variant"))?;
            let parent = body
                .pointer("/node_base_params/direct_parent_id")
                .and_then(Value::as_u64)
                .map(|v| v as u32);
            let bus = body
                .pointer("/node_base_params/override_bus_id")
                .and_then(Value::as_u64)
                .map(|v| v as u32);
            // Reconstruct the wrapper {kind: body} the storage layer expects.
            let wrapped = serde_json::json!({ kind.clone(): body });
            new_rows.push((id_label, id_hash, leaked(kind), wrapped, parent, bus));
        }
    }

    // Splice into mixer.children.items
    let children = mixer_body
        .pointer_mut("/ActorMixer/children/items")
        .and_then(Value::as_array_mut)
        .context("mixer has no children.items")?;
    for c in &new_children {
        children.push(serde_json::json!(*c));
    }
    children.sort_by_key(|v| v.as_u64().unwrap_or(0));
    let new_count = children.len();
    if let Some(count) = mixer_body.pointer_mut("/ActorMixer/children/count") {
        *count = serde_json::json!(new_count);
    }
    let new_mixer_json = serde_json::to_string(&mixer_body)?;
    phases.record("patch in memory", t.elapsed());

    let t = now();
    tx.execute(
        "UPDATE hirc_objects SET body_json = ? WHERE id_hash = ?",
        params![new_mixer_json, args.master_mixer as i64],
    )?;
    phases.record("UPDATE mixer row", t.elapsed());

    let t = now();
    {
        // Find the next free `ord` within the HIRC section to keep ordering
        // stable (storage assigns contiguous ords during write_soundbank).
        let (section_ord, mut next_ord): (i64, i64) = tx.query_row(
            "SELECT section_ord, COALESCE(MAX(ord)+1, 0)
             FROM hirc_objects
             WHERE section_ord = (SELECT ord FROM sections WHERE magic = 'HIRC')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut stmt = tx.prepare(
            "INSERT INTO hirc_objects(
                 section_ord, ord, id_kind, id_value, id_hash, label,
                 body_kind, body_type, direct_parent, override_bus, body_json)
             VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        )?;
        for (id_label, id_hash, kind, body, parent, bus) in &new_rows {
            let body_type: i64 = match *kind {
                "State" => 1,
                "Sound" => 2,
                "Action" => 3,
                "Event" => 4,
                "RandomSequenceContainer" => 5,
                "ActorMixer" => 7,
                _ => 0,
            };
            let body_json = serde_json::to_string(body)?;
            stmt.execute(params![
                section_ord,
                next_ord,
                "String",
                id_label,                  // id_value MUST round-trip back to the label
                *id_hash as i64,
                id_label,                  // label column for indexed search
                kind,
                body_type,
                parent.map(|v| v as i64),
                bus.map(|v| v as i64),
                body_json,
            ])?;
            next_ord += 1;
        }
    }
    phases.record(format!("INSERT {} new rows", new_rows.len()), t.elapsed());

    let t = now();
    tx.commit()?;
    phases.record("COMMIT", t.elapsed());

    // ── PHASE 2: export the patched DB back to a .bnk ──────────────────
    // Split into sub-phases so we can see what dominates.
    let t = now();
    let mut sb = wwise_storage::read_soundbank(&args.db_path)?;
    phases.record("export: read_soundbank", t.elapsed());

    let t = now();
    let wems = wwise_storage::read_wems(&args.db_path)?;
    phases.record("export: read_wems", t.elapsed());

    let t = now();
    if !wems.is_empty() {
        // mirror what export_bnk does internally
        wwise_storage::rebuild_didx_data_for_bench(&mut sb, wems)?;
    }
    phases.record("export: rebuild DIDX", t.elapsed());

    let t = now();
    wwise_format::prepare_soundbank(&mut sb);
    phases.record("export: prepare_export", t.elapsed());

    let t = now();
    let mut bits = deku::bitvec::BitVec::default();
    use deku::DekuWrite;
    sb.write(&mut bits, ())?;
    phases.record("export: Deku encode", t.elapsed());

    let t = now();
    std::fs::write(&args.bnk_out, bits.as_raw_slice())?;
    phases.record("export: write file", t.elapsed());

    let label = format!(
        "SQLite-style    n={} sounds  cold={}",
        args.n, args.cold
    );
    phases.print(&label);
    Ok(())
}

/// `static_str` from a String — used so we can pass `&'static str` body_kind
/// values into the SQL params! macro without lifetime gymnastics. The leak
/// is bounded: 6 unique kinds × N is ~24 strings per run.
fn leaked(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}
