//! Synthetic soundbanks for the viewer + benchmarks.
//!
//!     cargo run -p viewer --example smoke --release -- smoke.db
//!     cargo run -p viewer --example smoke --release -- --scale large smoke-large.db
//!
//! `--scale tiny` (default) writes a 10-object bank suitable for sanity-
//! testing the viewer UI.
//!
//! `--scale large` writes a `cs_main`-sized bank (~30k HIRC objects) suitable
//! for benchmarking the bnk → patch → bnk round-trip cost. Always produces
//! both a `.db` (SQLite) and a sibling `.bnk` (binary) so each approach can
//! start from the format it wants.

use std::path::{Path, PathBuf};
use std::time::Instant;

use wwise_format::*;

#[derive(Clone, Copy, Debug)]
enum Scale {
    Tiny,
    Large,
}

struct Args {
    scale: Scale,
    out: PathBuf,
}

fn parse_args() -> Args {
    let mut scale = Scale::Tiny;
    let mut positional: Vec<String> = Vec::new();
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--scale" => match iter.next().as_deref() {
                Some("tiny") => scale = Scale::Tiny,
                Some("large") => scale = Scale::Large,
                Some(other) => panic!("unknown --scale {other:?} (tiny|large)"),
                None => panic!("--scale takes an argument"),
            },
            "--help" | "-h" => {
                eprintln!("usage: smoke [--scale tiny|large] [out.db]");
                std::process::exit(0);
            }
            _ if a.starts_with("--") => panic!("unknown flag {a}"),
            _ => positional.push(a),
        }
    }
    let out = positional
        .into_iter()
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("smoke.db"));
    Args { scale, out }
}

fn main() -> anyhow::Result<()> {
    let args = parse_args();

    let t0 = Instant::now();
    let (sb, master_mixer_id) = match args.scale {
        Scale::Tiny => build_tiny(),
        Scale::Large => build_large(),
    };
    let n_objects = count_hirc(&sb);
    eprintln!(
        "built {} HIRC objects ({:.1?})",
        n_objects,
        t0.elapsed()
    );

    let dict = wwise_storage::FNVDictionary::new();
    let t1 = Instant::now();
    wwise_storage::write_soundbank(&args.out, &sb, &dict)?;
    eprintln!("wrote {} ({:.1?})", args.out.display(), t1.elapsed());

    // Also produce a sibling .bnk so the JortPob-style benchmark can start
    // from binary, matching their actual input.
    let bnk_path = bnk_sibling(&args.out);
    let t2 = Instant::now();
    wwise_storage::export_bnk(&args.out, &bnk_path)?;
    eprintln!("wrote {} ({:.1?})", bnk_path.display(), t2.elapsed());

    eprintln!("master_mixer_id = {master_mixer_id} (0x{master_mixer_id:08x})");
    Ok(())
}

fn bnk_sibling(db_path: &Path) -> PathBuf {
    let stem = db_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "smoke".to_string());
    db_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{stem}.bnk"))
}

fn count_hirc(sb: &Soundbank) -> usize {
    sb.sections
        .iter()
        .find_map(|s| match &s.body {
            SectionBody::HIRC(h) => Some(h.objects.len()),
            _ => None,
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tiny fixture (matches the viewer's existing demo bank).
// ---------------------------------------------------------------------------

fn build_tiny() -> (Soundbank, u32) {
    let mixer_id = id_of("Mixer_smoke");
    let cntr_id = id_of("Container_smoke_roar");
    let sound_a = id_of("Sound_smoke_roar_a");
    let sound_b = id_of("Sound_smoke_roar_b");
    let act_play = id_of("Action_play_smoke_roar");
    let act_stop = id_of("Action_stop_smoke_roar");

    let objects = vec![
        labeled("Mixer_smoke", HIRCObjectBody::ActorMixer(CAkActorMixer::new(0, vec![cntr_id]))),
        labeled(
            "Container_smoke_roar",
            HIRCObjectBody::RandomSequenceContainer(CAkRanSeqCntr::new(mixer_id, vec![sound_a, sound_b])),
        ),
        labeled("Sound_smoke_roar_a", HIRCObjectBody::Sound(CAkSound::new(cntr_id, 1001))),
        labeled("Sound_smoke_roar_b", HIRCObjectBody::Sound(CAkSound::new(cntr_id, 1002))),
        labeled("Action_play_smoke_roar", HIRCObjectBody::Action(action_play(cntr_id))),
        labeled("Action_stop_smoke_roar", HIRCObjectBody::Action(action_stop(cntr_id))),
        labeled("Play_smoke_roar", HIRCObjectBody::Event(CAkEvent::from_actions(vec![act_play]))),
        labeled("Stop_smoke_roar", HIRCObjectBody::Event(CAkEvent::from_actions(vec![act_stop]))),
        unlabeled(0xDEADBEEF, HIRCObjectBody::Sound(CAkSound::new(0, 9001))),
        unlabeled(0xCAFEF00D, HIRCObjectBody::Sound(CAkSound::new(0, 9002))),
    ];
    (assemble(objects), mixer_id)
}

// ---------------------------------------------------------------------------
// Large fixture — cs_main-shaped (~30k HIRC objects).
// ---------------------------------------------------------------------------
//
// Structure:
//   1   master ActorMixer            (the modder's "splice point")
//   100 sub-mixers under master
//   2000 RanSeq containers (~20 per sub-mixer)
//   8000 Sounds (~4 per container)
//   2000 Action  pairs (Play/Stop) → 4000 Actions
//   2000 Event   pairs (Play/Stop) → 4000 Events
//   ─────
//   ~18101 total HIRC objects, body JSON ≈ 25-35 MB depending on serde
//
// Plus a few thousand "filler" sounds with no parent so total objects land
// closer to cs_main's ~30k.

fn build_large() -> (Soundbank, u32) {
    let master_label = "MasterMixer_bench";
    let master_id = id_of(master_label);

    let n_sub_mixers: usize = 100;
    let containers_per_mixer: usize = 20;
    let sounds_per_container: usize = 4;
    let event_pairs: usize = 2000;
    let filler_sounds: usize = 12000;

    let mut sub_mixer_ids = Vec::with_capacity(n_sub_mixers);
    for i in 0..n_sub_mixers {
        sub_mixer_ids.push(id_of(&format!("SubMixer_{i:03}")));
    }
    let master = labeled(
        master_label,
        HIRCObjectBody::ActorMixer(CAkActorMixer::new(0, sub_mixer_ids.clone())),
    );

    // Containers + their child sounds.
    let mut sub_mixers = Vec::with_capacity(n_sub_mixers);
    let mut containers = Vec::new();
    let mut sounds = Vec::new();
    let mut wem_id_seq: u32 = 100_000;

    for (mi, &sub_id) in sub_mixer_ids.iter().enumerate() {
        let mut child_cntrs = Vec::with_capacity(containers_per_mixer);
        for ci in 0..containers_per_mixer {
            let cntr_label = format!("Cntr_{mi:03}_{ci:03}");
            let cntr_id = id_of(&cntr_label);
            child_cntrs.push(cntr_id);

            let mut child_sounds = Vec::with_capacity(sounds_per_container);
            for si in 0..sounds_per_container {
                let snd_label = format!("Snd_{mi:03}_{ci:03}_{si:02}");
                let snd_id = id_of(&snd_label);
                child_sounds.push(snd_id);
                sounds.push(labeled(
                    &snd_label,
                    HIRCObjectBody::Sound(CAkSound::new(cntr_id, wem_id_seq)),
                ));
                wem_id_seq += 1;
            }
            containers.push(labeled(
                &cntr_label,
                HIRCObjectBody::RandomSequenceContainer(CAkRanSeqCntr::new(sub_id, child_sounds)),
            ));
        }
        sub_mixers.push(labeled(
            &format!("SubMixer_{mi:03}"),
            HIRCObjectBody::ActorMixer(CAkActorMixer::new(master_id, child_cntrs)),
        ));
    }

    // Filler sounds without a parent — they bulk up the HIRC list to match
    // the cs_main object count without bloating the routing graph.
    for fi in 0..filler_sounds {
        sounds.push(unlabeled(
            0x70000000 + fi as u32,
            HIRCObjectBody::Sound(CAkSound::new(0, wem_id_seq)),
        ));
        wem_id_seq += 1;
    }

    // Event/Action pairs that point at random existing containers.
    let mut events = Vec::with_capacity(event_pairs * 2);
    let mut actions = Vec::with_capacity(event_pairs * 2);
    let total_cntrs = n_sub_mixers * containers_per_mixer;
    for ei in 0..event_pairs {
        let target_cntr_idx = (ei * 7919) % total_cntrs; // pseudo-random spread
        let target_cntr_id = id_of(&format!(
            "Cntr_{:03}_{:03}",
            target_cntr_idx / containers_per_mixer,
            target_cntr_idx % containers_per_mixer
        ));
        let play_action_label = format!("Action_play_{ei:05}");
        let stop_action_label = format!("Action_stop_{ei:05}");
        let play_action_id = id_of(&play_action_label);
        let stop_action_id = id_of(&stop_action_label);

        actions.push(labeled(
            &play_action_label,
            HIRCObjectBody::Action(action_play(target_cntr_id)),
        ));
        actions.push(labeled(
            &stop_action_label,
            HIRCObjectBody::Action(action_stop(target_cntr_id)),
        ));
        events.push(labeled(
            &format!("Play_v{ei:08}0"),
            HIRCObjectBody::Event(CAkEvent::from_actions(vec![play_action_id])),
        ));
        events.push(labeled(
            &format!("Stop_v{ei:08}0"),
            HIRCObjectBody::Event(CAkEvent::from_actions(vec![stop_action_id])),
        ));
    }

    let mut objects = Vec::with_capacity(
        1 + sub_mixers.len() + containers.len() + sounds.len() + actions.len() + events.len(),
    );
    objects.extend(sounds);
    objects.extend(containers);
    objects.extend(sub_mixers);
    objects.push(master);
    objects.extend(actions);
    objects.extend(events);

    (assemble(objects), master_id)
}

fn assemble(objects: Vec<HIRCObject>) -> Soundbank {
    Soundbank {
        sections: vec![
            Section {
                magic: *b"BKHD",
                size: 0,
                body: SectionBody::BKHD(BKHDSection {
                    version: 145,
                    bank_id: 0xCAFEBABE,
                    language_fnv_hash: 0,
                    wem_alignment: 16,
                    project_id: 0,
                    padding: Vec::new(),
                }),
            },
            Section {
                magic: *b"HIRC",
                size: 0,
                body: SectionBody::HIRC(HIRCSection::from_objects(objects)),
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn id_of(name: &str) -> u32 {
    ObjectId::String(name.to_string()).as_hash()
}

fn labeled(name: &str, body: HIRCObjectBody) -> HIRCObject {
    HIRCObject::new(ObjectId::String(name.to_string()), body)
}

fn unlabeled(hash: u32, body: HIRCObjectBody) -> HIRCObject {
    HIRCObject::new(ObjectId::Hash(hash), body)
}

fn action_play(target: u32) -> CAkAction {
    CAkAction {
        action_type: 0x0403,
        external_id: target,
        params: CAkActionParams::Play(CAkActionPlay::default()),
        ..Default::default()
    }
}

fn action_stop(target: u32) -> CAkAction {
    CAkAction {
        action_type: 0x0102,
        external_id: target,
        params: CAkActionParams::StopE(CAkActionStop::default()),
        ..Default::default()
    }
}
