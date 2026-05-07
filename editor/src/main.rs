//! `bnk-edit` — small CLI for editing rewwise SQLite soundbanks.
//!
//! The big use case (per Themyys' guide) is *duplicating an event from one
//! soundbank into another*: locate `Play_cXXXXXXX` in a chr bank, walk the
//! Event → Action → SequenceContainer → Sound chain, copy every node + the
//! WEM payloads into `cs_main`, and splice the new container into a parent
//! ActorMixer's children list. This binary automates that flow end-to-end.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use wwise_format::*;
use wwise_storage::{self as st, FNVDictionary};

#[derive(Parser)]
#[command(name = "bnk-edit", version, about = "Edit rewwise SQLite soundbanks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Parse a `.bnk` and write its contents to a `.db`.
    Import {
        bnk: PathBuf,
        db: PathBuf,
        /// Optional FNV name dictionary (one symbol per line).
        #[arg(short = 'd', long)]
        dictionary: Option<PathBuf>,
    },

    /// Reconstruct a `.bnk` from a `.db`.
    Export { db: PathBuf, bnk: PathBuf },

    /// List HIRC objects, optionally filtered by kind / label glob.
    Find {
        db: PathBuf,
        /// Body kind, e.g. Event, Action, Sound, RandomSequenceContainer.
        #[arg(short = 'k', long)]
        kind: Option<String>,
        /// SQL LIKE pattern matched against label, e.g. `Play_c%`.
        #[arg(short = 'l', long, value_name = "PATTERN")]
        like: Option<String>,
    },

    /// Pretty-print the JSON body of one HIRC object.
    Inspect {
        db: PathBuf,
        /// Label or numeric hash (`0x…` or decimal).
        target: String,
    },

    /// Show the dependency tree rooted at `target` (event/action/SC/sound).
    Tree {
        db: PathBuf,
        target: String,
    },

    /// Copy an event and everything it depends on from `src` into `dst`.
    /// Optionally splice the resulting container into a destination
    /// ActorMixer's children list (the "Themyys workflow" final step).
    CopyEvent {
        src: PathBuf,
        dst: PathBuf,
        /// Source event label, e.g. `Play_c211006013`.
        event: String,
        /// Destination parent (ActorMixer, RanSeq, etc.) — label or hash.
        /// If omitted, the new container is left orphaned and you'll need
        /// to `add-child` manually.
        #[arg(long)]
        register_with: Option<String>,
        /// Print plan and exit without modifying `dst`.
        #[arg(long)]
        dry_run: bool,
    },

    /// Append a child id to a parent's children list (and keep it sorted).
    AddChild {
        db: PathBuf,
        parent: String,
        child: String,
    },
}

// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Import { bnk, db, dictionary } => cmd_import(&bnk, &db, dictionary.as_deref()),
        Cmd::Export { db, bnk } => cmd_export(&db, &bnk),
        Cmd::Find { db, kind, like } => cmd_find(&db, kind.as_deref(), like.as_deref()),
        Cmd::Inspect { db, target } => cmd_inspect(&db, &target),
        Cmd::Tree { db, target } => cmd_tree(&db, &target),
        Cmd::CopyEvent {
            src,
            dst,
            event,
            register_with,
            dry_run,
        } => cmd_copy_event(&src, &dst, &event, register_with.as_deref(), dry_run),
        Cmd::AddChild { db, parent, child } => cmd_add_child(&db, &parent, &child),
    }
}

// ---------------------------------------------------------------------------
// Subcommand: import / export
// ---------------------------------------------------------------------------

fn cmd_import(bnk: &Path, db: &Path, dict_path: Option<&Path>) -> Result<()> {
    let dict = load_dictionary(dict_path)?;
    st::import_bnk(bnk, db, &dict).context("import_bnk")?;
    println!("Imported {} → {}", bnk.display(), db.display());
    Ok(())
}

fn cmd_export(db: &Path, bnk: &Path) -> Result<()> {
    st::export_bnk(db, bnk).context("export_bnk")?;
    println!("Exported {} → {}", db.display(), bnk.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand: find / inspect / tree
// ---------------------------------------------------------------------------

fn cmd_find(db: &Path, kind: Option<&str>, like: Option<&str>) -> Result<()> {
    let rows = st::list_objects(db, kind, like)?;
    if rows.is_empty() {
        println!("(no matches)");
        return Ok(());
    }
    for row in rows {
        let label = row.label.unwrap_or_else(|| "—".to_string());
        let parent = row
            .direct_parent
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".to_string());
        println!(
            "{:>12}  {:<32}  parent={:<12}  {}",
            row.id_hash, row.body_kind, parent, label
        );
    }
    Ok(())
}

fn cmd_inspect(db: &Path, target: &str) -> Result<()> {
    let sb = st::read_soundbank(db)?;
    let idx = HircIndex::build(&sb);
    let id = idx.resolve(target).ok_or_else(|| anyhow!("no such object: {target}"))?;
    let obj = idx
        .by_hash
        .get(&id)
        .ok_or_else(|| anyhow!("internal: index says yes but lookup says no"))?;
    let body_json = serde_json::to_string_pretty(&obj.body)?;
    println!("# id: {}  ({})", id, label_of(obj));
    println!("{body_json}");
    Ok(())
}

fn cmd_tree(db: &Path, target: &str) -> Result<()> {
    let sb = st::read_soundbank(db)?;
    let idx = HircIndex::build(&sb);
    let root = idx.resolve(target).ok_or_else(|| anyhow!("no such object: {target}"))?;
    let mut visited = BTreeSet::new();
    print_tree(&idx, root, 0, &mut visited);
    Ok(())
}

fn print_tree(idx: &HircIndex, id: u32, depth: usize, visited: &mut BTreeSet<u32>) {
    let pad = "  ".repeat(depth);
    let cycle = !visited.insert(id);
    let Some(obj) = idx.by_hash.get(&id) else {
        println!("{pad}{id} (external — not in this bank)");
        return;
    };
    let kind = body_kind(obj);
    let label = label_of(obj);
    println!("{pad}{id}  {kind}  {label}{}", if cycle { "  (cycle)" } else { "" });
    if cycle {
        return;
    }
    for child in references_of(&obj.body) {
        print_tree(idx, child, depth + 1, visited);
    }
}

// ---------------------------------------------------------------------------
// Subcommand: copy-event
// ---------------------------------------------------------------------------

fn cmd_copy_event(
    src_db: &Path,
    dst_db: &Path,
    event_label: &str,
    register_with: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let src_sb = st::read_soundbank(src_db).context("read source")?;
    let mut dst_sb = st::read_soundbank(dst_db).context("read destination")?;

    let src_idx = HircIndex::build(&src_sb);
    // For the destination we only need lookup tables, not references —
    // we want to mutate dst_sb freely afterwards.
    let dst_ids = HircIds::build(&dst_sb);

    let event_id = src_idx
        .resolve(event_label)
        .ok_or_else(|| anyhow!("event {event_label:?} not found in {}", src_db.display()))?;
    if body_kind(src_idx.by_hash[&event_id]) != "Event" {
        bail!(
            "{event_label} is a {}, not an Event — start the copy from a Play_/Stop_ event",
            body_kind(src_idx.by_hash[&event_id])
        );
    }

    // Build the closure of objects we need to transplant.
    let plan = collect_dependencies(&src_idx, event_id);

    println!(
        "Would copy {} HIRC objects ({} new, {} already in dst):",
        plan.len(),
        plan.iter().filter(|id| !dst_ids.by_hash.contains(id)).count(),
        plan.iter().filter(|id| dst_ids.by_hash.contains(id)).count(),
    );
    for id in &plan {
        let obj = src_idx.by_hash[id];
        let mark = if dst_ids.by_hash.contains(id) {
            "skip"
        } else {
            " new"
        };
        println!("  [{mark}] {:>12}  {:<32}  {}", id, body_kind(obj), label_of(obj));
    }

    let wem_ids = collect_wem_ids(&src_idx, &plan);
    println!("Plus {} WEM(s): {:?}", wem_ids.len(), wem_ids);

    if dry_run {
        return Ok(());
    }

    // Resolve the optional parent name -> id while dst_ids is still alive.
    let parent_link = if let Some(parent_target) = register_with {
        let parent_id = dst_ids
            .resolve(parent_target)
            .ok_or_else(|| anyhow!("parent {parent_target:?} not found in {}", dst_db.display()))?;
        let top_id = top_container_for_event(&src_idx, event_id)
            .ok_or_else(|| anyhow!("could not determine container for event {event_label:?}"))?;
        Some((parent_id, top_id, parent_target.to_string()))
    } else {
        None
    };

    // Append cloned objects into dst's HIRC section, skipping collisions.
    let dst_hirc = find_hirc_mut(&mut dst_sb).ok_or_else(|| anyhow!("destination has no HIRC section"))?;
    let mut added = 0usize;
    for id in &plan {
        if dst_ids.by_hash.contains(id) {
            continue;
        }
        let src_obj = src_idx.by_hash[id];
        let cloned = clone_via_json(src_obj)?;
        dst_hirc.objects.push(cloned);
        added += 1;
    }

    // Splice into the chosen parent if requested.
    if let Some((parent_id, top_id, parent_label)) = parent_link {
        let parent = dst_hirc
            .objects
            .iter_mut()
            .find(|o| o.id.as_hash() == parent_id)
            .ok_or_else(|| anyhow!("parent {parent_id} not found in destination HIRC"))?;
        add_child_to(&mut parent.body, top_id)
            .with_context(|| format!("add child {top_id} to parent {parent_id}"))?;
        println!("Linked container {top_id} into parent {parent_id} ({parent_label})");
    }

    // Persist soundbank + WEMs.
    st::write_soundbank(dst_db, &dst_sb, &FNVDictionary::new()).context("write destination")?;
    let wem_ids_vec: Vec<u32> = wem_ids.into_iter().collect();
    let copied = st::copy_wems(src_db, dst_db, &wem_ids_vec).context("copy wems")?;

    println!("Done — wrote {} new HIRC object(s) and {} WEM(s)", added, copied);
    Ok(())
}

// ---------------------------------------------------------------------------
// Subcommand: add-child
// ---------------------------------------------------------------------------

fn cmd_add_child(db: &Path, parent: &str, child: &str) -> Result<()> {
    let mut sb = st::read_soundbank(db)?;
    let idx = HircIndex::build(&sb);
    let parent_id = idx.resolve(parent).ok_or_else(|| anyhow!("no such parent: {parent}"))?;
    let child_id = idx.resolve(child).ok_or_else(|| anyhow!("no such child: {child}"))?;

    let dst_hirc = find_hirc_mut(&mut sb).ok_or_else(|| anyhow!("no HIRC section"))?;
    let parent_obj = dst_hirc
        .objects
        .iter_mut()
        .find(|o| o.id.as_hash() == parent_id)
        .ok_or_else(|| anyhow!("parent {parent_id} not found"))?;
    add_child_to(&mut parent_obj.body, child_id)?;

    st::write_soundbank(db, &sb, &FNVDictionary::new())?;
    println!("Added {child_id} as child of {parent_id}");
    Ok(())
}

// ---------------------------------------------------------------------------
// HIRC index + lookup
// ---------------------------------------------------------------------------

/// Borrowed view: `&HIRCObject` references for everything in the bank.
/// Cheap to build, but pins `Soundbank` for its lifetime.
struct HircIndex<'a> {
    by_hash: HashMap<u32, &'a HIRCObject>,
    by_label: HashMap<String, u32>,
}

impl<'a> HircIndex<'a> {
    fn build(sb: &'a Soundbank) -> Self {
        let mut by_hash = HashMap::new();
        let mut by_label = HashMap::new();
        if let Some(hirc) = sb.sections.iter().find_map(|s| match &s.body {
            SectionBody::HIRC(h) => Some(h),
            _ => None,
        }) {
            for obj in &hirc.objects {
                let h = obj.id.as_hash();
                by_hash.insert(h, obj);
                if let ObjectId::String(s) = &obj.id {
                    by_label.insert(s.clone(), h);
                }
            }
        }
        Self { by_hash, by_label }
    }

    fn resolve(&self, target: &str) -> Option<u32> {
        if let Some(stripped) = target.strip_prefix("0x").or_else(|| target.strip_prefix("0X")) {
            if let Ok(v) = u32::from_str_radix(stripped, 16) {
                if self.by_hash.contains_key(&v) {
                    return Some(v);
                }
            }
        }
        if let Ok(v) = target.parse::<u32>() {
            if self.by_hash.contains_key(&v) {
                return Some(v);
            }
        }
        self.by_label.get(target).copied()
    }
}

/// Owned view: just the IDs and labels, no references. Use this for the
/// soundbank you need to mutate.
struct HircIds {
    by_hash: BTreeSet<u32>,
    by_label: HashMap<String, u32>,
}

impl HircIds {
    fn build(sb: &Soundbank) -> Self {
        let mut by_hash = BTreeSet::new();
        let mut by_label = HashMap::new();
        if let Some(hirc) = sb.sections.iter().find_map(|s| match &s.body {
            SectionBody::HIRC(h) => Some(h),
            _ => None,
        }) {
            for obj in &hirc.objects {
                let h = obj.id.as_hash();
                by_hash.insert(h);
                if let ObjectId::String(s) = &obj.id {
                    by_label.insert(s.clone(), h);
                }
            }
        }
        Self { by_hash, by_label }
    }

    fn resolve(&self, target: &str) -> Option<u32> {
        if let Some(stripped) = target.strip_prefix("0x").or_else(|| target.strip_prefix("0X")) {
            if let Ok(v) = u32::from_str_radix(stripped, 16) {
                if self.by_hash.contains(&v) {
                    return Some(v);
                }
            }
        }
        if let Ok(v) = target.parse::<u32>() {
            if self.by_hash.contains(&v) {
                return Some(v);
            }
        }
        self.by_label.get(target).copied()
    }
}

fn body_kind(obj: &HIRCObject) -> &'static str {
    use HIRCObjectBody::*;
    match obj.body {
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

fn label_of(obj: &HIRCObject) -> String {
    match &obj.id {
        ObjectId::String(s) => s.clone(),
        ObjectId::Hash(_) => String::from("—"),
    }
}

// ---------------------------------------------------------------------------
// Reference graph traversal
// ---------------------------------------------------------------------------

/// Other HIRC ids that this body refers to. Used both for `tree` printing
/// and for `copy-event`'s dependency closure.
fn references_of(body: &HIRCObjectBody) -> Vec<u32> {
    use HIRCObjectBody::*;
    let mut out = Vec::new();
    match body {
        Event(e) => out.extend(e.actions.iter().copied()),
        Action(a) => {
            if a.external_id != 0 {
                out.push(a.external_id);
            }
        }
        ActorMixer(b) => out.extend(b.children.items.iter().copied()),
        RandomSequenceContainer(b) => out.extend(b.children.items.iter().copied()),
        SwitchContainer(b) => {
            out.extend(b.children.items.iter().copied());
            for pkg in &b.switch_groups {
                out.extend(pkg.nodes.iter().copied());
            }
        }
        LayerContainer(b) => {
            out.extend(b.children.items.iter().copied());
            for layer in &b.layers {
                for assoc in &layer.associated_children {
                    out.push(assoc.associated_child_id);
                }
            }
        }
        MusicSegment(b) => out.extend(b.music_node_params.children.items.iter().copied()),
        MusicSwitchContainer(b) => out.extend(
            b.music_trans_node_params
                .music_node_params
                .children
                .items
                .iter()
                .copied(),
        ),
        MusicRandomSequenceContainer(b) => out.extend(
            b.music_trans_node_params
                .music_node_params
                .children
                .items
                .iter()
                .copied(),
        ),
        // Sound, Bus, AuxiliaryBus etc. — leaves (or refs we don't follow).
        _ => {}
    }
    out
}

/// BFS from `start`, collecting every HIRC id we'd need to copy along with it.
/// Stops at IDs that aren't in `idx` (those are external — already in dst or
/// part of a soundbank we're not processing).
fn collect_dependencies(idx: &HircIndex, start: u32) -> Vec<u32> {
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = std::collections::VecDeque::from([start]);
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let Some(obj) = idx.by_hash.get(&id) else { continue };
        order.push(id);
        for child in references_of(&obj.body) {
            if !seen.contains(&child) {
                queue.push_back(child);
            }
        }
    }
    order
}

/// The "top-level container for an event" is the target of the event's first
/// Play action. That's the SC ID Themyys' guide refers to.
fn top_container_for_event(idx: &HircIndex, event_id: u32) -> Option<u32> {
    let event = idx.by_hash.get(&event_id)?;
    let actions = match &event.body {
        HIRCObjectBody::Event(e) => &e.actions,
        _ => return None,
    };
    for action_id in actions {
        let action = idx.by_hash.get(action_id)?;
        if let HIRCObjectBody::Action(a) = &action.body {
            if a.external_id != 0 {
                return Some(a.external_id);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// WEM collection
// ---------------------------------------------------------------------------

fn collect_wem_ids(idx: &HircIndex, ids: &[u32]) -> BTreeSet<u32> {
    let mut wems = BTreeSet::new();
    for id in ids {
        let Some(obj) = idx.by_hash.get(id) else { continue };
        match &obj.body {
            HIRCObjectBody::Sound(s) => {
                let media = &s.bank_source_data.media_information;
                if media.source_id != 0 {
                    wems.insert(media.source_id);
                }
            }
            HIRCObjectBody::MusicTrack(t) => {
                for src in &t.sources {
                    if src.media_information.source_id != 0 {
                        wems.insert(src.media_information.source_id);
                    }
                }
            }
            _ => {}
        }
    }
    wems
}

// ---------------------------------------------------------------------------
// Mutation helpers
// ---------------------------------------------------------------------------

fn find_hirc_mut(sb: &mut Soundbank) -> Option<&mut HIRCSection> {
    sb.sections.iter_mut().find_map(|s| match &mut s.body {
        SectionBody::HIRC(h) => Some(h),
        _ => None,
    })
}

fn add_child_to(body: &mut HIRCObjectBody, child_id: u32) -> Result<()> {
    use HIRCObjectBody::*;
    let children = match body {
        ActorMixer(b) => &mut b.children,
        RandomSequenceContainer(b) => &mut b.children,
        SwitchContainer(b) => &mut b.children,
        LayerContainer(b) => &mut b.children,
        MusicSegment(b) => &mut b.music_node_params.children,
        MusicSwitchContainer(b) => &mut b.music_trans_node_params.music_node_params.children,
        MusicRandomSequenceContainer(b) => &mut b.music_trans_node_params.music_node_params.children,
        other => bail!(
            "cannot add a child to a {} — only containers/mixers have children",
            kind_name(other)
        ),
    };
    if !children.items.contains(&child_id) {
        children.items.push(child_id);
        children.items.sort_unstable();
    }
    Ok(())
}

fn kind_name(body: &HIRCObjectBody) -> &'static str {
    // Fallback inline since `body_kind` takes a HIRCObject, not HIRCObjectBody.
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

/// `HIRCObject` doesn't derive Clone (its body has internal cursors and the
/// derive isn't on every nested type), so we round-trip through serde JSON.
/// This is also the cheapest way to guarantee the cloned subtree carries no
/// stale `size`/`count` cache.
fn clone_via_json(obj: &HIRCObject) -> Result<HIRCObject> {
    let body_json = serde_json::to_string(&obj.body)?;
    let body: HIRCObjectBody = serde_json::from_str(&body_json)?;
    let id = match &obj.id {
        ObjectId::String(s) => ObjectId::String(s.clone()),
        ObjectId::Hash(h) => ObjectId::Hash(*h),
    };
    Ok(HIRCObject::new(id, body))
}

// ---------------------------------------------------------------------------
// Dictionary loading
// ---------------------------------------------------------------------------

fn load_dictionary(path: Option<&Path>) -> Result<FNVDictionary> {
    let Some(path) = path else {
        return Ok(FNVDictionary::new());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading dictionary {}", path.display()))?;
    Ok(st::parse_dictionary(&text))
}
