//! Verify that the two benchmark approaches produced equivalent banks.
//!
//!     bench-compare a.bnk b.bnk
//!
//! Reports: file sizes, HIRC object counts, set-equality of object ids,
//! and (if a master mixer hash is given) the contents of its children list.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use clap::Parser;
use wwise_format::*;

#[derive(Parser)]
struct Args {
    a: PathBuf,
    b: PathBuf,
    /// Optional FNV hash of a mixer to compare children.items[] across the two.
    #[arg(long)]
    mixer: Option<u32>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let bytes_a = fs::read(&args.a)?;
    let bytes_b = fs::read(&args.b)?;
    let sb_a = wwise_format::parse_soundbank(&bytes_a)?;
    let sb_b = wwise_format::parse_soundbank(&bytes_b)?;

    println!("              {:>20}    {:>20}", args.a.display(), args.b.display());
    println!("file size     {:>20}    {:>20}", bytes_a.len(), bytes_b.len());

    let hirc_a = hirc_objects(&sb_a);
    let hirc_b = hirc_objects(&sb_b);
    println!("hirc count    {:>20}    {:>20}", hirc_a.len(), hirc_b.len());

    let ids_a: BTreeSet<u32> = hirc_a.iter().map(|o| o.id.as_hash()).collect();
    let ids_b: BTreeSet<u32> = hirc_b.iter().map(|o| o.id.as_hash()).collect();
    let only_a: Vec<u32> = ids_a.difference(&ids_b).copied().collect();
    let only_b: Vec<u32> = ids_b.difference(&ids_a).copied().collect();
    println!("ids only in a {:>20}    ", only_a.len());
    println!("ids only in b                              {:>20}", only_b.len());

    if !only_a.is_empty() {
        println!("  sample a-only: {:?}", &only_a[..only_a.len().min(5)]);
    }
    if !only_b.is_empty() {
        println!("  sample b-only: {:?}", &only_b[..only_b.len().min(5)]);
    }

    if let Some(mixer_id) = args.mixer {
        let ch_a = mixer_children(&hirc_a, mixer_id);
        let ch_b = mixer_children(&hirc_b, mixer_id);
        let set_a: BTreeSet<u32> = ch_a.iter().copied().collect();
        let set_b: BTreeSet<u32> = ch_b.iter().copied().collect();
        println!();
        println!("mixer 0x{mixer_id:08x} children:");
        println!("  count in a: {}", ch_a.len());
        println!("  count in b: {}", ch_b.len());
        println!("  symmetric difference: {}", set_a.symmetric_difference(&set_b).count());
    }

    // Byte-by-byte diff (will normally differ by section ordering / counts).
    if bytes_a == bytes_b {
        println!();
        println!("bytes:        IDENTICAL");
    } else {
        let common = bytes_a.iter().zip(bytes_b.iter()).take_while(|(x, y)| x == y).count();
        println!();
        println!(
            "bytes:        differ — first {} bytes match, then diverge",
            common
        );
    }

    // Round-trip stability check on each: parse → prepare_export → write → reparse.
    for (label, sb_owned) in [(args.a.display().to_string(), sb_a), (args.b.display().to_string(), sb_b)] {
        let mut sb = sb_owned;
        wwise_format::prepare_soundbank(&mut sb);
        let mut bits = deku::bitvec::BitVec::default();
        use deku::DekuWrite;
        sb.write(&mut bits, ())?;
        // Re-parse the freshly-rewritten bytes
        let _re: Soundbank = wwise_format::parse_soundbank(bits.as_raw_slice())?;
        println!("round-trip ok: {}", label);
    }

    Ok(())
}

fn hirc_objects(sb: &Soundbank) -> &[HIRCObject] {
    sb.sections
        .iter()
        .find_map(|s| match &s.body {
            SectionBody::HIRC(h) => Some(h.objects.as_slice()),
            _ => None,
        })
        .unwrap_or(&[])
}

fn mixer_children(objects: &[HIRCObject], id: u32) -> Vec<u32> {
    for o in objects {
        if o.id.as_hash() != id {
            continue;
        }
        return match &o.body {
            HIRCObjectBody::ActorMixer(b) => b.children.items.clone(),
            HIRCObjectBody::RandomSequenceContainer(b) => b.children.items.clone(),
            HIRCObjectBody::SwitchContainer(b) => b.children.items.clone(),
            HIRCObjectBody::LayerContainer(b) => b.children.items.clone(),
            _ => Vec::new(),
        };
    }
    Vec::new()
}
