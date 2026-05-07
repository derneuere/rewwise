//! Shared bits for the two benchmark binaries.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wwise_format::ObjectId;

/// Build the JSON for a freshly-generated `Sound + RanSeqCntr + Action(Play) +
/// Action(Stop) + Event(Play) + Event(Stop)` chain rooted at `mixer_id`.
///
/// Mirrors what JortPob's `MainSoundBank.cs` generates per dialog line, but
/// produced as `serde_json::Value` rather than cloned from a hand-crafted
/// template — avoids needing template fixtures for the bench.
pub struct NewSound {
    pub sc_id: u32,                // SequenceContainer hash (the splice point)
    pub objects: Vec<Value>,       // 6 HIRC objects to insert into HIRC.objects
}

pub fn make_new_sound(seq: u32, mixer_id: u32, source_id: u32) -> NewSound {
    let sound_label = format!("Snd_new_{seq:06}");
    let cntr_label = format!("Cntr_new_{seq:06}");
    let play_action_label = format!("Action_play_new_{seq:06}");
    let stop_action_label = format!("Action_stop_new_{seq:06}");
    let play_event_label = format!("Play_new_{seq:06}");
    let stop_event_label = format!("Stop_new_{seq:06}");

    let sound_id = ObjectId::String(sound_label.clone()).as_hash();
    let cntr_id = ObjectId::String(cntr_label.clone()).as_hash();
    let play_action_id = ObjectId::String(play_action_label.clone()).as_hash();
    let stop_action_id = ObjectId::String(stop_action_label.clone()).as_hash();

    NewSound {
        sc_id: cntr_id,
        objects: vec![
            hirc_obj("Sound", &sound_label, sound_body(cntr_id, source_id)),
            hirc_obj(
                "RandomSequenceContainer",
                &cntr_label,
                ran_seq_body(mixer_id, sound_id),
            ),
            hirc_obj("Action", &play_action_label, action_play_body(cntr_id)),
            hirc_obj("Action", &stop_action_label, action_stop_body(cntr_id)),
            hirc_obj("Event", &play_event_label, event_body(play_action_id)),
            hirc_obj("Event", &stop_event_label, event_body(stop_action_id)),
        ],
    }
}

fn hirc_obj(kind: &str, label: &str, body_inner: Value) -> Value {
    let body_type: u8 = match kind {
        "State" => 1,
        "Sound" => 2,
        "Action" => 3,
        "Event" => 4,
        "RandomSequenceContainer" => 5,
        "ActorMixer" => 7,
        _ => panic!("unsupported kind {kind}"),
    };
    json!({
        "body_type": body_type,
        "size": 0,
        "id": { "String": label },
        "body": { kind: body_inner },
    })
}

fn sound_body(parent_id: u32, source_id: u32) -> Value {
    json!({
        "bank_source_data": {
            "plugin": "PCM",
            "source_type": "Embedded",
            "media_information": { "source_id": source_id, "in_memory_media_size": 0, "source_flags": 0 },
            "params_size": 0,
            // base64-encoded Vec<u8>; empty bytes is "" not [].
            "params": "",
        },
        "node_base_params": node_base_params(parent_id),
    })
}

fn ran_seq_body(parent_id: u32, only_child: u32) -> Value {
    json!({
        "node_base_params": node_base_params(parent_id),
        "loop_count": 0,
        "loop_mod_min": 0,
        "loop_mod_max": 0,
        "transition_time": 0.0,
        "transition_time_mod_min": 0.0,
        "transition_time_mod_max": 0.0,
        "avoid_repeat_count": 0,
        "transition_mode": 0,
        "random_mode": 0,
        "mode": 0,
        "flags": 0,
        "children": { "count": 1, "items": [only_child] },
        "playlist": { "count": 0, "items": [] },
    })
}

fn action_play_body(target: u32) -> Value {
    json!({
        "action_type": 0x0403,
        "external_id": target,
        "is_bus": 0,
        "prop_bundle": [],
        "ranged_modifiers": { "count": 0, "entries": [] },
        "params": { "Play": { "fade_curve": 0, "bank_id": 0 } },
    })
}

fn action_stop_body(target: u32) -> Value {
    json!({
        "action_type": 0x0102,
        "external_id": target,
        "is_bus": 0,
        "prop_bundle": [],
        "ranged_modifiers": { "count": 0, "entries": [] },
        "params": { "StopE": {
            "stop":   { "flags1": 0, "flags2": 0 },
            "except": { "count": 0, "exceptions": [] },
        } },
    })
}

fn event_body(action_id: u32) -> Value {
    json!({ "action_count": 1, "actions": [action_id] })
}

fn node_base_params(parent_id: u32) -> Value {
    json!({
        "node_initial_fx_parameters": {
            "is_override_parent_fx": 0, "fx_chunk_count": 0,
            "fx_bypass_bits": 0, "fx_chunks": []
        },
        "override_attachment_params": 0,
        "override_bus_id": 0,
        "direct_parent_id": parent_id,
        "unknown_flags": 0,
        "node_initial_params": {
            "prop_initial_values": [],
            "prop_ranged_modifiers": { "count": 0, "entries": [] }
        },
        "positioning_params": positioning_params(),
        "aux_params": aux_params(),
        "adv_settings_params": adv_settings_params(),
        "state_chunk": {
            "state_property_count": 0, "state_property_info": [],
            "state_group_count": 0, "state_group_chunks": []
        },
        "initial_rtpc": { "count": 0, "rtpcs": [] },
    })
}

fn positioning_params() -> Value {
    json!({
        "unk1": false,
        "three_dimensional_position_type": "Emitter",
        "speaker_panning_type": "DirectSpeakerAssignment",
        "listener_relative_routing": false,
        "override_parent": false,
        "unk2": false,
        "enable_diffraction": false,
        "hold_listener_orientation": false,
        "hold_emitter_position_and_orientation": false,
        "enable_attenuation": false,
        "three_dimensional_spatialization_mode": "None",
        "path_mode": "StepSequence",
        "transition_time": 0,
        "vertex_count": 0,
        "vertices": [],
        "path_list_item_count": 0,
        "path_list_item_offsets": [],
        "three_dimensional_automation_params": [],
    })
}

fn aux_params() -> Value {
    json!({
        "unk1": false, "unk2": false, "unk3": false,
        "override_reflections_aux_bus": false,
        "has_aux": false,
        "override_user_aux_sends": false,
        "unk4": 0,
        "aux1": 0, "aux2": 0, "aux3": 0, "aux4": 0,
        "reflections_aux_bus": 0,
    })
}

fn adv_settings_params() -> Value {
    json!({
        "unk1": false, "unk2": false, "unk3": false,
        "is_virtual_voices_opt_override_parent": false,
        "ignore_parent_maximum_instances": false,
        "unk4": false,
        "use_virtual_behavior": false,
        "kill_newest": false,
        "virtual_queue_behavior": "PlayFromBeginning",
        "max_instance_count": 0,
        "below_threshold_behavior": "ContinueToPlay",
        "unk5": false, "unk6": false, "unk7": false, "unk8": false,
        "enable_envelope": false,
        "normalize_loudness": false,
        "override_analysis": false,
        "override_hdr_envelope": false,
    })
}

// ---------------------------------------------------------------------------
// Phase timer
// ---------------------------------------------------------------------------

pub struct Phases {
    rows: Vec<(String, Duration)>,
}

impl Phases {
    pub fn new() -> Self { Self { rows: Vec::new() } }
    pub fn record(&mut self, name: impl Into<String>, dur: Duration) {
        self.rows.push((name.into(), dur));
    }
    pub fn print(&self, label: &str) {
        let total: Duration = self.rows.iter().map(|(_, d)| *d).sum();
        eprintln!();
        eprintln!("┌─ {label}");
        for (name, dur) in &self.rows {
            eprintln!("│ {name:<30} {dur:>10.2?}");
        }
        eprintln!("├─ total{:>34.2?}", total);
        eprintln!("└─");
    }
}

pub fn now() -> Instant { Instant::now() }
