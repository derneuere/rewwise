//! Daemon-mode benchmark — `SoundbankSession` keeps the parsed bank in RAM
//! across mutations, so each export skips `read_soundbank` entirely.
//!
//! Mirrors `bench-sqlite`'s CLI shape so the two are A/B-comparable on the
//! same workload. The differentiator is `--iterations`: a single Session
//! can mutate-and-export N times in a row, and each cycle past the first
//! pays zero parsed-bank-into-RAM cost — that's the daemon's strongest
//! case.

use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use bench::{make_new_sound, now, Phases};
use clap::Parser;
use serde_json::Value;

#[derive(Parser)]
#[command(about = "Session-mode: in-memory Soundbank cached across mutate+export")]
struct Args {
    #[arg(long)]
    bnk_in: PathBuf,
    #[arg(long)]
    bnk_out: PathBuf,
    /// Where the SQLite cache lives. Created via `import_bnk` if missing —
    /// the import is timed separately so the warm steady-state numbers
    /// stay comparable to `bench-sqlite`.
    #[arg(long)]
    db_path: PathBuf,
    /// FNV hash of the master mixer to splice new SCs into.
    #[arg(long)]
    master_mixer: u32,
    /// Number of new sounds (and their associated subgraph) to add per
    /// iteration.
    #[arg(long, default_value_t = 100)]
    n: u32,
    /// Run M successive mutate+export cycles inside the same session.
    /// Cycles 2..M skip the read_soundbank cost — the daemon win.
    #[arg(long, default_value_t = 1)]
    iterations: u32,
    /// Treat the .db as cold (rebuild from bnk_in, time the import).
    #[arg(long)]
    cold: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut phases = Phases::new();
    let dict = wwise_storage::FNVDictionary::new();

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

    // ── PHASE 1: Session::open — pays parsed-bank-into-RAM cost ONCE ───
    let t = now();
    let mut session = wwise_storage::SoundbankSession::open(&args.db_path)
        .context("SoundbankSession::open")?;
    phases.record("Session::open (read+parse)", t.elapsed());

    // wem_seq is shared across iterations so freshly-introduced source_ids
    // don't collide. (Mirrors what JortPob does — a counter per build.)
    let mut wem_seq: u32 = 900_000_000;

    for it in 0..args.iterations {
        let it_label = if args.iterations > 1 {
            format!(" [iter {}/{}]", it + 1, args.iterations)
        } else {
            String::new()
        };

        // ── PHASE 2: build new objects' JSON. Same shape as the bench-
        // sqlite `patch in memory` block — kept outside the storage
        // call so its cost is visible.
        let t = now();
        let mut all_objects: Vec<Value> = Vec::with_capacity(args.n as usize * 6);
        let mut new_children: Vec<u32> = Vec::with_capacity(args.n as usize);
        let base_seq = it * args.n;
        for i in 0..args.n {
            let ns = make_new_sound(base_seq + i, args.master_mixer, wem_seq);
            wem_seq += 1;
            new_children.push(ns.sc_id);
            all_objects.extend(ns.objects);
        }
        phases.record(format!("build new objects{it_label}"), t.elapsed());

        // ── PHASE 3: apply mutation. The session updates its in-memory
        // Soundbank *and* mirrors the rows to SQLite in one tx.
        let t = now();
        session
            .add_hirc_subgraph(&dict, args.master_mixer, all_objects, new_children)
            .context("add_hirc_subgraph")?;
        phases.record(format!("mutate (mem + SQL tx){it_label}"), t.elapsed());

        // ── PHASE 4: export — runs entirely from in-memory state. No
        // read_soundbank, no read_wems on the hot path.
        let phased = session
            .export_phased(&args.bnk_out)
            .context("export_phased")?;
        phases.record(format!("export: rebuild DIDX{it_label}"), phased.didx_t);
        phases.record(format!("export: prepare_export{it_label}"), phased.prep_t);
        phases.record(format!("export: Deku encode{it_label}"), phased.enc_t);
        phases.record(format!("export: write file{it_label}"), phased.write_t);
    }

    let label = format!(
        "Session-mode    n={}  iterations={}  cold={}",
        args.n, args.iterations, args.cold
    );
    phases.print(&label);
    Ok(())
}
