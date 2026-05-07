//! Build a synthetic soundbank that exercises the viewer's full layout.
//!
//!   cargo run -p viewer --example smoke -- smoke.db
//!
//! Lays down a small but realistic event tree (matching what Themyys' guide
//! describes for a single chr roar):
//!
//!   ActorMixer "Mixer_smoke"
//!   └── RanSeqCntr "Container_smoke_roar"
//!       ├── Sound "Sound_smoke_roar_a"   (wem 1001)
//!       └── Sound "Sound_smoke_roar_b"   (wem 1002)
//!
//!   Event "Play_smoke_roar" → Action "Action_play_smoke_roar" → Container_smoke_roar
//!   Event "Stop_smoke_roar" → Action "Action_stop_smoke_roar" → Container_smoke_roar
//!
//! Plus a couple of unlabeled hash-only Sounds so the search/filter UI has
//! both labeled and unlabeled rows. None of these are Wwise-correct in
//! detail but they round-trip cleanly through serde + the storage layer.

use std::path::PathBuf;

use wwise_format::*;

fn id_of(name: &str) -> u32 {
    ObjectId::String(name.to_string()).as_hash()
}

fn labeled(name: &str, body: HIRCObjectBody) -> HIRCObject {
    HIRCObject::new(ObjectId::String(name.to_string()), body)
}

fn unlabeled(hash: u32, body: HIRCObjectBody) -> HIRCObject {
    HIRCObject::new(ObjectId::Hash(hash), body)
}

fn play_action(target: u32) -> CAkAction {
    CAkAction {
        action_type: 0x0403, // Play
        external_id: target,
        params: CAkActionParams::Play(CAkActionPlay::default()),
        ..Default::default()
    }
}

fn stop_action(target: u32) -> CAkAction {
    CAkAction {
        action_type: 0x0102, // StopE
        external_id: target,
        params: CAkActionParams::StopE(CAkActionStop::default()),
        ..Default::default()
    }
}

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("smoke.db"));

    // ----------------------------------------------------------------- ids
    let mixer_id    = id_of("Mixer_smoke");
    let cntr_id     = id_of("Container_smoke_roar");
    let sound_a_id  = id_of("Sound_smoke_roar_a");
    let sound_b_id  = id_of("Sound_smoke_roar_b");
    let act_play_id = id_of("Action_play_smoke_roar");
    let act_stop_id = id_of("Action_stop_smoke_roar");

    // ------------------------------------------------------------- objects
    let objects = vec![
        labeled(
            "Mixer_smoke",
            HIRCObjectBody::ActorMixer(CAkActorMixer::new(0, vec![cntr_id])),
        ),
        labeled(
            "Container_smoke_roar",
            HIRCObjectBody::RandomSequenceContainer(CAkRanSeqCntr::new(
                mixer_id,
                vec![sound_a_id, sound_b_id],
            )),
        ),
        labeled(
            "Sound_smoke_roar_a",
            HIRCObjectBody::Sound(CAkSound::new(cntr_id, 1001)),
        ),
        labeled(
            "Sound_smoke_roar_b",
            HIRCObjectBody::Sound(CAkSound::new(cntr_id, 1002)),
        ),
        labeled(
            "Action_play_smoke_roar",
            HIRCObjectBody::Action(play_action(cntr_id)),
        ),
        labeled(
            "Action_stop_smoke_roar",
            HIRCObjectBody::Action(stop_action(cntr_id)),
        ),
        labeled(
            "Play_smoke_roar",
            HIRCObjectBody::Event(CAkEvent::from_actions(vec![act_play_id])),
        ),
        labeled(
            "Stop_smoke_roar",
            HIRCObjectBody::Event(CAkEvent::from_actions(vec![act_stop_id])),
        ),
        // Hash-only sounds so the table shows what unlabeled rows look like.
        unlabeled(
            0xDEADBEEF,
            HIRCObjectBody::Sound(CAkSound::new(0, 9001)),
        ),
        unlabeled(
            0xCAFEF00D,
            HIRCObjectBody::Sound(CAkSound::new(0, 9002)),
        ),
    ];

    let sb = Soundbank {
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
    };

    let dict = wwise_storage::FNVDictionary::new();
    wwise_storage::write_soundbank(&out, &sb, &dict)?;
    println!("wrote {}", out.display());
    Ok(())
}
