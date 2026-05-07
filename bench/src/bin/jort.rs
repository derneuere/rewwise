//! JortPob-style benchmark — port of their `MainSoundBank.Write()` flow:
//!
//!   .bnk → bnk2json (decompile) → soundbank.json
//!   read JSON → JsonNode (here: serde_json::Value) → patch → write JSON
//!   bnk2json (compile) → .bnk
//!
//! We collapse "shell out to bnk2json" into in-process equivalents so the
//! comparison against the SQLite path is apples-to-apples (both pay the
//! same Deku decode + encode); the *additional* JSON serialize/parse round-
//! trips are what JortPob's pipeline actually pays on every build.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use bench::{make_new_sound, now, Phases};
use clap::Parser;
use deku::bitvec::BitVec;
use deku::DekuWrite;
use serde_json::Value;
use wwise_format::*;

#[derive(Parser)]
#[command(about = "JortPob-style: full bnk → JSON → patch → JSON → bnk round-trip")]
struct Args {
    #[arg(long)]
    bnk_in: PathBuf,
    #[arg(long)]
    bnk_out: PathBuf,
    /// Where to write the intermediate soundbank.json (mimics what
    /// `bnk2json.exe` produces for JortPob to read in-process).
    #[arg(long)]
    json_path: PathBuf,
    /// FNV hash of the master mixer to splice new SCs into.
    #[arg(long)]
    master_mixer: u32,
    /// Number of new sounds (and their associated subgraph) to add.
    #[arg(long, default_value_t = 100)]
    n: u32,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut phases = Phases::new();

    // ── PHASE 1: decompile the input bnk to a JSON file on disk ─────────
    // Equivalent to `bnk2json.exe cs_main.bnk` in JortPob's pipeline.
    let t = now();
    let bytes = fs::read(&args.bnk_in).context("read input bnk")?;
    phases.record("read input bnk", t.elapsed());

    let t = now();
    let mut sb = wwise_format::parse_soundbank(&bytes)?;
    sb.sections.retain(|s| !matches!(&s.body, SectionBody::DIDX(_) | SectionBody::DATA(_)));
    phases.record("Deku parse (decompile)", t.elapsed());

    let t = now();
    let json_text = serde_json::to_string_pretty(&sb)?;
    phases.record("serde → JSON string", t.elapsed());

    let t = now();
    fs::write(&args.json_path, &json_text).context("write soundbank.json")?;
    let json_size = json_text.len();
    phases.record("write soundbank.json", t.elapsed());

    drop(sb);
    drop(json_text);

    // ── PHASE 2: C# side — read JSON, parse to Value, patch, serialize ──
    let t = now();
    let json_text = fs::read_to_string(&args.json_path).context("re-read soundbank.json")?;
    phases.record("read soundbank.json", t.elapsed());

    let t = now();
    let mut value: Value = serde_json::from_str(&json_text)?;
    drop(json_text);
    phases.record("JSON → Value (parse)", t.elapsed());

    // ── PHASE 3: linear scan for the master mixer + patch ──────────────
    let t = now();
    let objects = value
        .pointer_mut("/sections/1/body/HIRC/objects")
        .and_then(Value::as_array_mut)
        .context("missing HIRC.objects")?;
    let mixer_idx = objects
        .iter()
        .position(|o| o["id"]["Hash"].as_u64() == Some(args.master_mixer as u64))
        .context("master mixer not found in HIRC")?;
    phases.record("scan for master mixer", t.elapsed());

    let t = now();
    // Generate N new sounds. Append SC ids to the mixer's children, and
    // append the 6N new HIRC objects to the bank's HIRC array.
    let mut new_objects: Vec<Value> = Vec::with_capacity(args.n as usize * 6);
    let mut new_children: Vec<u32> = Vec::with_capacity(args.n as usize);
    let mut wem_seq: u32 = 900_000_000;
    for i in 0..args.n {
        let ns = make_new_sound(i, args.master_mixer, wem_seq);
        wem_seq += 1;
        new_children.push(ns.sc_id);
        new_objects.extend(ns.objects);
    }

    // Splice into the mixer's children list (this is the part Themyys' guide
    // calls "Adding ID to Children List").
    let mixer = &mut objects[mixer_idx];
    let children = mixer
        .pointer_mut("/body/ActorMixer/children/items")
        .and_then(Value::as_array_mut)
        .context("mixer has no children.items")?;
    for c in &new_children {
        children.push(serde_json::json!(*c));
    }
    children.sort_by_key(|v| v.as_u64().unwrap_or(0));
    let new_count = children.len();
    if let Some(count_node) = mixer.pointer_mut("/body/ActorMixer/children/count") {
        *count_node = serde_json::json!(new_count);
    }

    // Append the new HIRC objects to the array.
    objects.extend(new_objects);
    let total_objects = objects.len();
    if let Some(count) = value.pointer_mut("/sections/1/body/HIRC/object_count") {
        *count = serde_json::json!(total_objects);
    }
    phases.record("patch (clone+append)", t.elapsed());

    // ── PHASE 4: serialize Value → JSON string + disk ──────────────────
    let t = now();
    let new_json = serde_json::to_string_pretty(&value)?;
    let new_json_size = new_json.len();
    phases.record("Value → JSON string", t.elapsed());

    let t = now();
    fs::write(&args.json_path, &new_json).context("write patched JSON")?;
    phases.record("write patched JSON", t.elapsed());

    drop(value);
    drop(new_json);

    // ── PHASE 5: compile JSON → bnk (the second bnk2json invocation) ───
    let t = now();
    let json_text = fs::read_to_string(&args.json_path).context("re-read patched JSON")?;
    phases.record("read patched JSON", t.elapsed());

    let t = now();
    let mut sb: Soundbank = serde_json::from_str(&json_text)?;
    drop(json_text);
    phases.record("JSON → Soundbank (parse)", t.elapsed());

    let t = now();
    wwise_format::prepare_soundbank(&mut sb);
    phases.record("prepare_export", t.elapsed());

    let t = now();
    let mut bits = BitVec::default();
    sb.write(&mut bits, ())?;
    phases.record("Deku encode (compile)", t.elapsed());

    let t = now();
    fs::write(&args.bnk_out, bits.as_raw_slice()).context("write output bnk")?;
    phases.record("write output bnk", t.elapsed());

    let label = format!(
        "JortPob-style    n={} sounds  json={:.1} MB → {:.1} MB",
        args.n,
        json_size as f64 / 1_048_576.0,
        new_json_size as f64 / 1_048_576.0,
    );
    phases.print(&label);
    Ok(())
}
