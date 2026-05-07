//! Leptos components for the rewwise viewer. Pure SSR — no signals or
//! interactivity beyond plain HTML form submits and links.
//!
//! Voice (per [impeccable.style/designing](https://impeccable.style/designing/)):
//! calm, clinical, terminal-density. Targets modders comparing two banks side
//! by side. Avoids hype copy, glow, and CTA-styled buttons.

use leptos::*;
// Disambiguate `Children` — leptos uses it for the children-prop closure type,
// while wwise_format uses it for HIRC container `children.items`. We only ever
// need leptos::Children in this file.
use wwise_format::{HIRCObject, HIRCObjectBody, ObjectId, SectionBody, Soundbank};
use wwise_storage::ObjectSummary;

const KINDS: &[&str] = &[
    "Event",
    "Action",
    "Sound",
    "RandomSequenceContainer",
    "SwitchContainer",
    "ActorMixer",
    "LayerContainer",
    "Bus",
    "AuxiliaryBus",
    "MusicSegment",
    "MusicTrack",
    "MusicSwitchContainer",
    "MusicRandomSequenceContainer",
    "DialogueEvent",
    "Attenuation",
    "EffectShareSet",
    "EffectCustom",
    "AudioDevice",
    "State",
    "TimeModulator",
];

const STYLE: &str = include_str!("style.css");

/// Constant header info supplied by the request handler. Keeps the bank
/// path in front of the modder when tabs are stacked.
#[derive(Clone)]
pub struct BankInfo {
    pub name: String,
    pub object_count: u64,
}

#[component]
pub fn Layout(bank: BankInfo, children: Children) -> impl IntoView {
    view! {
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width,initial-scale=1"/>
                <title>{format!("{} — rewwise", bank.name)}</title>
                // `inner_html` skips Leptos's HTML escaper — without it the
                // CSS gets mangled (`/` -> `&#x2F;`, `"` -> `&quot;`) and the
                // browser stops parsing comments / strings.
                <style inner_html=STYLE></style>
            </head>
            <body>
                <header>
                    <a href="/" class="brand">"rewwise"</a>
                    <span class="sep">"·"</span>
                    <span class="bank-path">{bank.name.clone()}</span>
                    <span class="bank-stats">{format!("{} objects", bank.object_count)}</span>
                </header>
                <main>{children()}</main>
            </body>
        </html>
    }
}

#[component]
pub fn SearchView(
    bank: BankInfo,
    results: Vec<ObjectSummary>,
    total_match: u64,
    cap: u64,
    q: String,
    kind: String,
) -> impl IntoView {
    let returned = results.len() as u64;
    let truncated = total_match > returned;
    let q_value = q.clone();
    let kind_value = kind.clone();

    let kind_options: Vec<_> = KINDS
        .iter()
        .map(|k| {
            let v = (*k).to_string();
            let selected = kind_value == v;
            view! { <option value=v.clone() selected=selected>{v}</option> }
        })
        .collect();

    let rows: Vec<_> = results
        .into_iter()
        .map(|r| {
            let id = r.id_hash;
            let target = r.label.clone().unwrap_or_else(|| id.to_string());
            let label_cell = match r.label {
                Some(label) => view! { <a href={format!("/object/{}", target)}>{label}</a> }.into_view(),
                None => view! {
                    <a href={format!("/object/{}", id)} class="muted">{format!("0x{:08x}", id)}</a>
                }.into_view(),
            };
            let parent_cell = r
                .direct_parent
                .filter(|p| *p != 0)
                .map(|p| p.to_string())
                .unwrap_or_default();
            view! {
                <tr>
                    <td class="id">{id}</td>
                    <td class="kind">{r.body_kind}</td>
                    <td class="id">{parent_cell}</td>
                    <td class="label">{label_cell}</td>
                </tr>
            }
        })
        .collect();

    let count_line = if total_match == 0 {
        view! {
            <p class="count-line">"no matches"</p>
        }.into_view()
    } else if truncated {
        view! {
            <p class="count-line">
                <strong>{returned.to_string()}</strong>" of "{total_match.to_string()}" matches"
                <span class="truncated">{format!("(truncated to {})", cap)}</span>
            </p>
        }.into_view()
    } else {
        view! {
            <p class="count-line">
                <strong>{total_match.to_string()}</strong>" "
                {if total_match == 1 { "match" } else { "matches" }}
            </p>
        }.into_view()
    };

    let table = if results_is_empty(returned) {
        view! { <></> }.into_view()
    } else {
        view! {
            <table>
                <thead>
                    <tr>
                        <th>"id"</th>
                        <th>"kind"</th>
                        <th>"parent"</th>
                        <th>"label"</th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        }.into_view()
    };

    view! {
        <Layout bank=bank>
            <form method="get" action="/" role="search">
                <label for="q">"label"</label>
                <input
                    id="q"
                    name="q"
                    placeholder="substring (e.g. Play_c2110)"
                    value=q_value
                    autofocus="true"
                />
                <label for="kind">"kind"</label>
                <select id="kind" name="kind">
                    <option value="" selected=kind.is_empty()>"any"</option>
                    {kind_options}
                </select>
                <button type="submit">"filter"</button>
            </form>
            {count_line}
            {table}
        </Layout>
    }
}

fn results_is_empty(returned: u64) -> bool { returned == 0 }

#[component]
pub fn ObjectView(
    bank: BankInfo,
    soundbank: Soundbank,
    children_of: Vec<ObjectSummary>,
    id: u32,
    target: String,
    /// If the user just submitted a bad edit, the raw text they typed comes
    /// back here so they don't lose it.
    #[prop(default = None)] pending_body: Option<String>,
    /// Validation message to render above the textarea.
    #[prop(default = None)] error: Option<String>,
) -> impl IntoView {
    let Some(obj) = find_object(&soundbank, id) else {
        let display = if target.is_empty() { id.to_string() } else { target.clone() };
        return view! {
            <Layout bank=bank>
                <p class="crumb">
                    <a href="/">"objects"</a>
                    <span class="sep">"/"</span>
                    <span class="muted">{display.clone()}</span>
                </p>
                <p class="empty">"No HIRC object matches " <code>{display}</code> "."</p>
            </Layout>
        }
        .into_view();
    };

    let kind = body_kind_str(&obj.body);
    let label_opt = match &obj.id {
        ObjectId::String(s) => Some(s.clone()),
        ObjectId::Hash(_) => None,
    };
    let label_for_display = label_opt
        .clone()
        .unwrap_or_else(|| format!("0x{:08x}", id));

    let live_body_json = serde_json::to_string_pretty(&obj.body).unwrap_or_default();
    // If we have a pending edit (failed save), show that instead so the user
    // doesn't lose their work.
    let body_json = pending_body.clone().unwrap_or_else(|| live_body_json.clone());
    let body_lines = body_json.lines().count();
    let body_bytes = body_json.len();
    // For HTML boolean attributes Leptos's `open=bool` emits `open=""` which
    // the browser treats as truthy regardless. `Option<&str>` lets us omit
    // the attribute entirely when we don't want it open.
    let editor_open: Option<&'static str> =
        if pending_body.is_some() || error.is_some() { Some("") } else { None };

    let refs = references_of(&obj.body);

    let parent = match &obj.body {
        HIRCObjectBody::Sound(b) => Some(b.node_base_params.direct_parent_id),
        HIRCObjectBody::RandomSequenceContainer(b) => Some(b.node_base_params.direct_parent_id),
        HIRCObjectBody::SwitchContainer(b) => Some(b.node_base_params.direct_parent_id),
        HIRCObjectBody::ActorMixer(b) => Some(b.node_base_params.direct_parent_id),
        HIRCObjectBody::LayerContainer(b) => Some(b.node_base_params.direct_parent_id),
        HIRCObjectBody::MusicSegment(b) => Some(b.music_node_params.node_base_params.direct_parent_id),
        HIRCObjectBody::MusicTrack(b) => Some(b.node_base_params.direct_parent_id),
        HIRCObjectBody::MusicSwitchContainer(b) => Some(
            b.music_trans_node_params
                .music_node_params
                .node_base_params
                .direct_parent_id,
        ),
        HIRCObjectBody::MusicRandomSequenceContainer(b) => Some(
            b.music_trans_node_params
                .music_node_params
                .node_base_params
                .direct_parent_id,
        ),
        _ => None,
    }
    .filter(|p| *p != 0);

    let can_add_child = matches!(
        &obj.body,
        HIRCObjectBody::ActorMixer(_)
            | HIRCObjectBody::RandomSequenceContainer(_)
            | HIRCObjectBody::SwitchContainer(_)
            | HIRCObjectBody::LayerContainer(_)
            | HIRCObjectBody::MusicSegment(_)
            | HIRCObjectBody::MusicSwitchContainer(_)
            | HIRCObjectBody::MusicRandomSequenceContainer(_)
    );

    let target_for_form = label_opt.clone().unwrap_or_else(|| id.to_string());

    // True when the references list comes from a Children.items array — only
    // those entries support inline removal. Action targets, event action ids,
    // etc. aren't backed by a `children.items` slot in the parent.
    let refs_are_children = matches!(
        &obj.body,
        HIRCObjectBody::ActorMixer(_)
            | HIRCObjectBody::RandomSequenceContainer(_)
            | HIRCObjectBody::SwitchContainer(_)
            | HIRCObjectBody::LayerContainer(_)
            | HIRCObjectBody::MusicSegment(_)
            | HIRCObjectBody::MusicSwitchContainer(_)
            | HIRCObjectBody::MusicRandomSequenceContainer(_)
    );

    // Outgoing reference rows (event.actions, action.external_id, container.children).
    let ref_rows: Vec<_> = refs
        .iter()
        .map(|&rid| reference_row(&soundbank, rid, refs_are_children, &target_for_form))
        .collect();
    let refs_section = if ref_rows.is_empty() {
        view! { <></> }.into_view()
    } else {
        view! {
            <h3>{format!("references ({})", refs.len())}</h3>
            <table class="refs"><tbody>{ref_rows}</tbody></table>
        }
        .into_view()
    };

    // Reverse: things in this bank whose direct_parent equals our id. Useful
    // for "what does this mixer hold?" without traversing children.items.
    let child_rows: Vec<_> = children_of
        .iter()
        .map(|s| {
            let target = s.label.clone().unwrap_or_else(|| s.id_hash.to_string());
            let label_cell = match s.label.clone() {
                Some(l) => view! { <a href={format!("/object/{}", target)}>{l}</a> }.into_view(),
                None => view! {
                    <a href={format!("/object/{}", s.id_hash)} class="muted">{format!("0x{:08x}", s.id_hash)}</a>
                }.into_view(),
            };
            view! {
                <tr>
                    <td class="id">{s.id_hash}</td>
                    <td class="kind">{s.body_kind.clone()}</td>
                    <td class="label">{label_cell}</td>
                </tr>
            }
        })
        .collect();
    let children_section = if child_rows.is_empty() {
        view! { <></> }.into_view()
    } else {
        view! {
            <h3>{format!("attached children ({})", children_of.len())}</h3>
            <table class="refs"><tbody>{child_rows}</tbody></table>
        }
        .into_view()
    };

    let parent_view = parent.map(|pid| {
        let target = lookup_label(&soundbank, pid).unwrap_or_else(|| pid.to_string());
        let display_label = lookup_label(&soundbank, pid).unwrap_or_else(|| format!("0x{:08x}", pid));
        let display_kind = lookup_kind(&soundbank, pid).unwrap_or("external");
        view! {
            <>
                <dt>"parent"</dt>
                <dd>
                    <span class="muted">{display_kind}" "</span>
                    <a href={format!("/object/{}", target)}>{display_label}</a>
                    " "
                    <span class="dim">{format!("({})", pid)}</span>
                </dd>
            </>
        }
    });

    let add_child_form = can_add_child.then(|| {
        view! {
            <section class="add-child">
                <h3>"add child"</h3>
                <p class="help">
                    "The child must already exist in this bank. Use "
                    <code>"bnk-edit copy-event"</code>
                    " first to bring an event subgraph in from another bank."
                </p>
                <form method="post" action="/add-child" class="row">
                    <input type="hidden" name="parent" value=target_for_form.clone()/>
                    <input
                        type="text"
                        name="child"
                        placeholder="child label or id"
                        required="true"
                    />
                    <button type="submit">"add"</button>
                </form>
            </section>
        }
    });

    view! {
        <Layout bank=bank>
            <p class="crumb">
                <a href="/">"objects"</a>
                <span class="sep">"/"</span>
                <span class="muted">{kind}</span>
                <span class="sep">"/"</span>
                <span>{label_for_display}</span>
            </p>

            <h2>
                {match label_opt.as_ref() {
                    Some(l) => view! { <span>{l.clone()}</span> }.into_view(),
                    None => view! { <span class="muted">{format!("0x{:08x}", id)}</span> }.into_view(),
                }}
            </h2>

            <dl class="meta">
                <dt>"kind"</dt><dd>{kind}</dd>
                <dt>"id"</dt><dd><code>{id}</code> " " <span class="dim">{format!("(0x{:08x})", id)}</span></dd>
                {parent_view}
            </dl>

            <details>
                <summary>"identity"</summary>
                <form method="post" action="/edit-id" class="body-edit-form">
                    <input type="hidden" name="target" value=target_for_form.clone()/>
                    <div class="row">
                        <input
                            type="text"
                            name="new_id"
                            value=label_opt.clone().unwrap_or_else(|| id.to_string())
                            placeholder="label, decimal hash, or 0x… hash"
                        />
                        <button type="submit">"rename"</button>
                    </div>
                    <label class="renumber-row">
                        <input type="checkbox" name="renumber" value="on"/>
                        " renumber — also rewrite every reference to "
                        <code>{id}</code>
                        " in this bank"
                    </label>
                    <p class="help">
                        "Without "<em>"renumber"</em>", the new id must FNV-hash to the same value as the
                         current one — useful for upgrading an unknown "
                        <code>"0x…"</code>
                        " hash to a discovered dictionary name. "
                        <em>"Renumber"</em>" rewrites every "
                        <code>"u32"</code>
                        " in the HIRC matching the old hash, so children, parents, "
                        <code>"external_id"</code>
                        ", and event "
                        <code>"actions[]"</code>
                        " stay linked."
                    </p>
                </form>
            </details>

            {refs_section}
            {children_section}
            {add_child_form}

            <details open=editor_open>
                <summary>
                    {format!("body json ({} lines, {})", body_lines, format_bytes(body_bytes))}
                </summary>
                <form method="post" action="/edit-body" class="body-edit-form">
                    <input type="hidden" name="target" value=target_for_form.clone()/>
                    {error.map(|e| view! { <div class="error">{e}</div> })}
                    <textarea name="body" class="body-edit" spellcheck="false" rows="24">
                        {body_json}
                    </textarea>
                    <div class="toolbar">
                        <span class="help">
                            "Edits replace the entire HIRC body. JSON shape must match the body kind."
                        </span>
                        <button type="submit">"save"</button>
                    </div>
                </form>
            </details>
        </Layout>
    }
    .into_view()
}

fn reference_row(
    sb: &Soundbank,
    rid: u32,
    removable: bool,
    parent_target: &str,
) -> impl IntoView {
    let target = lookup_label(sb, rid).unwrap_or_else(|| rid.to_string());
    let display_kind = lookup_kind(sb, rid).unwrap_or("external");
    let label_cell = match lookup_label(sb, rid) {
        Some(label) => view! { <a href={format!("/object/{}", target)}>{label}</a> }.into_view(),
        None => view! {
            <a href={format!("/object/{}", rid)} class="muted">{format!("0x{:08x}", rid)}</a>
        }.into_view(),
    };
    let remove_cell = if removable {
        let pt = parent_target.to_string();
        view! {
            <td class="action">
                <form method="post" action="/remove-child" class="inline">
                    <input type="hidden" name="parent" value=pt/>
                    <input type="hidden" name="child" value=rid.to_string()/>
                    <button type="submit" class="icon" title="remove this child">"×"</button>
                </form>
            </td>
        }
        .into_view()
    } else {
        view! { <td class="action"></td> }.into_view()
    };
    view! {
        <tr>
            <td class="id">{rid}</td>
            <td class="kind">{display_kind}</td>
            <td class="label">{label_cell}</td>
            {remove_cell}
        </tr>
    }
}

fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{} B", n)
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_object(sb: &Soundbank, id: u32) -> Option<&HIRCObject> {
    sb.sections.iter().find_map(|s| match &s.body {
        SectionBody::HIRC(h) => h.objects.iter().find(|o| o.id.as_hash() == id),
        _ => None,
    })
}

fn lookup_label(sb: &Soundbank, id: u32) -> Option<String> {
    find_object(sb, id).and_then(|o| match &o.id {
        ObjectId::String(s) => Some(s.clone()),
        ObjectId::Hash(_) => None,
    })
}

fn lookup_kind(sb: &Soundbank, id: u32) -> Option<&'static str> {
    find_object(sb, id).map(|o| body_kind_str(&o.body))
}

fn body_kind_str(body: &HIRCObjectBody) -> &'static str {
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
        _ => {}
    }
    out
}
