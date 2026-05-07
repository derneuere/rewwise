//! `bnk-viewer` — local web viewer/editor for rewwise SQLite soundbanks.
//!
//! Single-binary axum server that renders pages with Leptos SSR. There is no
//! client-side WASM; the page reloads on form submission. Run as:
//!
//!   bnk-viewer path/to/soundbank.db
//!
//! and visit <http://127.0.0.1:3939/>.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Form, Router,
};
use clap::Parser;
use leptos::ssr::render_to_string;
use leptos::*;
use serde::Deserialize;

mod app;
use app::{BankInfo, ObjectView, SearchView};

/// Cap on the number of search-result rows we render. The DB query itself
/// is fast on `cs_main` (~30k rows), but emitting 30k <tr>s as HTML is not.
const SEARCH_CAP: u64 = 500;

#[derive(Parser, Debug)]
#[command(version, about = "Local web viewer/editor for rewwise SQLite soundbanks")]
struct Args {
    /// Path to the .db file (created by `bnk2sqlite` or `bnk-edit import`).
    db: PathBuf,
    /// Port to listen on.
    #[arg(short, long, default_value_t = 3939)]
    port: u16,
}

#[derive(Clone)]
struct AppState {
    db: Arc<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if !args.db.exists() {
        anyhow::bail!("database file does not exist: {}", args.db.display());
    }
    let state = AppState {
        db: Arc::new(args.db.clone()),
    };

    let app = Router::new()
        .route("/", get(home))
        .route("/object/:target", get(object_page))
        .route("/add-child", post(add_child))
        .route("/remove-child", post(remove_child))
        .route("/edit-body", post(edit_body))
        .route("/edit-id", post(edit_id))
        .with_state(state);

    let addr: SocketAddr = ([127, 0, 0, 1], args.port).into();
    println!("opening {}", args.db.display());
    println!("listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    kind: String,
}

async fn home(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Html<String> {
    let kind_opt = if params.kind.is_empty() {
        None
    } else {
        Some(params.kind.clone())
    };
    // Wrap user input with `%` so they don't have to type LIKE wildcards.
    let like_opt = if params.q.is_empty() {
        None
    } else {
        Some(format!("%{}%", params.q))
    };

    let results = wwise_storage::list_objects_capped(
        &state.db,
        kind_opt.as_deref(),
        like_opt.as_deref(),
        SEARCH_CAP as i64,
    )
    .unwrap_or_else(|e| {
        eprintln!("list_objects failed: {e:#}");
        Vec::new()
    });
    let total_match =
        wwise_storage::count_objects_filtered(&state.db, kind_opt.as_deref(), like_opt.as_deref())
            .unwrap_or_else(|e| {
                eprintln!("count_objects_filtered failed: {e:#}");
                results.len() as u64
            });

    let bank = bank_info(&state);
    let q = params.q;
    let kind = params.kind;
    let html = render_to_string(move || {
        view! { <SearchView bank=bank results=results total_match=total_match cap=SEARCH_CAP q=q kind=kind/> }
    })
    .to_string();
    Html(wrap_doctype(html))
}

async fn object_page(
    State(state): State<AppState>,
    AxPath(target): AxPath<String>,
) -> impl IntoResponse {
    render_object(&state, &target, None, None)
}

/// Shared renderer used by GET /object/:target and the POST handlers when
/// they need to redisplay the page with an inline error/pending edit.
fn render_object(
    state: &AppState,
    target: &str,
    pending_body: Option<String>,
    error: Option<String>,
) -> axum::response::Response {
    let sb = match wwise_storage::read_soundbank(&state.db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read_soundbank: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let resolved = resolve_target(target, &sb);
    let (status, id) = match resolved {
        Some(id) => {
            if error.is_some() {
                (StatusCode::BAD_REQUEST, id)
            } else {
                (StatusCode::OK, id)
            }
        }
        None => (StatusCode::NOT_FOUND, 0),
    };
    let children_of = if id != 0 {
        wwise_storage::list_children_of(&state.db, id).unwrap_or_default()
    } else {
        Vec::new()
    };
    let bank = bank_info(state);
    let target_owned = target.to_string();
    let html = render_to_string(move || {
        view! {
            <ObjectView
                bank=bank
                soundbank=sb
                children_of=children_of
                id=id
                target=target_owned
                pending_body=pending_body
                error=error
            />
        }
    })
    .to_string();
    (status, Html(wrap_doctype(html))).into_response()
}

#[derive(Deserialize)]
struct AddChildForm {
    parent: String,
    child: String,
}

async fn add_child(
    State(state): State<AppState>,
    Form(form): Form<AddChildForm>,
) -> impl IntoResponse {
    match do_add_child(&state.db, &form.parent, &form.child) {
        Ok(()) => Redirect::to(&format!("/object/{}", form.parent)).into_response(),
        Err(e) => render_object(&state, &form.parent, None, Some(format!("add child failed: {e:#}"))),
    }
}

#[derive(Deserialize)]
struct RemoveChildForm {
    parent: String,
    child: String,
}

async fn remove_child(
    State(state): State<AppState>,
    Form(form): Form<RemoveChildForm>,
) -> impl IntoResponse {
    match do_remove_child(&state.db, &form.parent, &form.child) {
        Ok(()) => Redirect::to(&format!("/object/{}", form.parent)).into_response(),
        Err(e) => render_object(&state, &form.parent, None, Some(format!("remove child failed: {e:#}"))),
    }
}

#[derive(Deserialize)]
struct EditBodyForm {
    target: String,
    body: String,
}

async fn edit_body(
    State(state): State<AppState>,
    Form(form): Form<EditBodyForm>,
) -> impl IntoResponse {
    match do_edit_body(&state.db, &form.target, &form.body) {
        Ok(()) => Redirect::to(&format!("/object/{}", form.target)).into_response(),
        Err(e) => render_object(
            &state,
            &form.target,
            Some(form.body),
            Some(format!("save failed: {e:#}")),
        ),
    }
}

#[derive(Deserialize)]
struct EditIdForm {
    target: String,
    new_id: String,
    /// Browser checkboxes either send `renumber=on` or omit the field.
    #[serde(default)]
    renumber: Option<String>,
}

async fn edit_id(
    State(state): State<AppState>,
    Form(form): Form<EditIdForm>,
) -> impl IntoResponse {
    let renumber = form.renumber.is_some();
    let new_id_input = form.new_id.trim().to_string();
    let from = form.target.clone();
    match do_edit_id(&state.db, &from, &new_id_input, renumber) {
        Ok(redirect_target) => Redirect::to(&format!("/object/{}", redirect_target)).into_response(),
        Err(e) => render_object(&state, &from, None, Some(format!("rename failed: {e:#}"))),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wrap_doctype(html: String) -> String {
    format!("<!DOCTYPE html>{html}")
}

fn bank_info(state: &AppState) -> BankInfo {
    let name = state
        .db
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| state.db.display().to_string());
    let object_count = wwise_storage::count_objects(&state.db).unwrap_or(0);
    BankInfo { name, object_count }
}

fn do_add_child(db: &Path, parent: &str, child: &str) -> anyhow::Result<()> {
    use wwise_format::*;
    let mut sb = wwise_storage::read_soundbank(db)?;
    let parent_id = resolve_target(parent, &sb)
        .ok_or_else(|| anyhow::anyhow!("no such parent: {parent}"))?;
    let child_id = resolve_target(child, &sb)
        .ok_or_else(|| anyhow::anyhow!("no such child: {child}"))?;
    let hirc = sb
        .sections
        .iter_mut()
        .find_map(|s| match &mut s.body {
            SectionBody::HIRC(h) => Some(h),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("no HIRC section in this soundbank"))?;
    let parent_obj = hirc
        .objects
        .iter_mut()
        .find(|o| o.id.as_hash() == parent_id)
        .ok_or_else(|| anyhow::anyhow!("parent {parent_id} not in HIRC"))?;
    add_child_to_body(&mut parent_obj.body, child_id)?;
    wwise_storage::write_soundbank(db, &sb, &Default::default())?;
    Ok(())
}

fn add_child_to_body(
    body: &mut wwise_format::HIRCObjectBody,
    child_id: u32,
) -> anyhow::Result<()> {
    let children = children_mut(body)?;
    if !children.items.contains(&child_id) {
        children.items.push(child_id);
        children.items.sort_unstable();
    }
    Ok(())
}

fn children_mut(
    body: &mut wwise_format::HIRCObjectBody,
) -> anyhow::Result<&mut wwise_format::Children> {
    use wwise_format::HIRCObjectBody::*;
    Ok(match body {
        ActorMixer(b) => &mut b.children,
        RandomSequenceContainer(b) => &mut b.children,
        SwitchContainer(b) => &mut b.children,
        LayerContainer(b) => &mut b.children,
        MusicSegment(b) => &mut b.music_node_params.children,
        MusicSwitchContainer(b) => &mut b.music_trans_node_params.music_node_params.children,
        MusicRandomSequenceContainer(b) => {
            &mut b.music_trans_node_params.music_node_params.children
        }
        _ => anyhow::bail!("this body kind has no children list"),
    })
}

fn do_remove_child(db: &Path, parent: &str, child: &str) -> anyhow::Result<()> {
    use wwise_format::*;
    let mut sb = wwise_storage::read_soundbank(db)?;
    let parent_id = resolve_target(parent, &sb)
        .ok_or_else(|| anyhow::anyhow!("no such parent: {parent}"))?;
    // Allow specifying child by id or label even if it doesn't currently
    // resolve in this bank — but try anyway.
    let child_id = parse_id(child)
        .or_else(|| resolve_target(child, &sb))
        .ok_or_else(|| anyhow::anyhow!("could not parse child id: {child}"))?;
    let hirc = sb
        .sections
        .iter_mut()
        .find_map(|s| match &mut s.body {
            SectionBody::HIRC(h) => Some(h),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("no HIRC section in this soundbank"))?;
    let parent_obj = hirc
        .objects
        .iter_mut()
        .find(|o| o.id.as_hash() == parent_id)
        .ok_or_else(|| anyhow::anyhow!("parent {parent_id} not in HIRC"))?;
    let children = children_mut(&mut parent_obj.body)?;
    let before = children.items.len();
    children.items.retain(|id| *id != child_id);
    if children.items.len() == before {
        anyhow::bail!("child {child_id} was not in the children list");
    }
    wwise_storage::write_soundbank(db, &sb, &Default::default())?;
    Ok(())
}

fn do_edit_body(db: &Path, target: &str, body_json: &str) -> anyhow::Result<()> {
    use wwise_format::*;
    // Validate as a HIRCObjectBody in isolation first so a syntax error
    // doesn't take a half-mutated soundbank to disk.
    let new_body: HIRCObjectBody = serde_json::from_str(body_json)
        .map_err(|e| anyhow::anyhow!("JSON parse: {e}"))?;

    let mut sb = wwise_storage::read_soundbank(db)?;
    let id = resolve_target(target, &sb)
        .ok_or_else(|| anyhow::anyhow!("no such object: {target}"))?;
    let hirc = sb
        .sections
        .iter_mut()
        .find_map(|s| match &mut s.body {
            SectionBody::HIRC(h) => Some(h),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("no HIRC section"))?;
    let obj = hirc
        .objects
        .iter_mut()
        .find(|o| o.id.as_hash() == id)
        .ok_or_else(|| anyhow::anyhow!("object {id} disappeared from HIRC"))?;
    obj.body = new_body;
    wwise_storage::write_soundbank(db, &sb, &Default::default())?;
    Ok(())
}

/// Parse a numeric id (decimal or `0x…`) without consulting the soundbank.
/// Used when a user pastes a raw hash into the remove-child form for an
/// orphan reference that doesn't exist as its own HIRC row.
fn parse_id(s: &str) -> Option<u32> {
    if let Some(stripped) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u32::from_str_radix(stripped, 16).ok();
    }
    s.parse::<u32>().ok()
}

/// Treat the input as a hash if it looks numeric (`0x…` or all digits),
/// otherwise as a string label. Mirrors how `bnk-edit` resolves targets.
fn parse_object_id(s: &str) -> wwise_format::ObjectId {
    use wwise_format::ObjectId;
    if let Some(h) = parse_id(s) {
        ObjectId::Hash(h)
    } else {
        ObjectId::String(s.to_string())
    }
}

/// Replace every `u32` JSON number that equals `old` with `new`. Used by
/// `do_edit_id` in renumber mode to rewrite every reference (Children.items,
/// CAkEvent.actions, CAkAction.external_id, NodeBaseParams.direct_parent_id,
/// override_bus_id, switch package nodes, …) without having to enumerate
/// each container struct's reference fields.
fn rewrite_u32(value: &mut serde_json::Value, old: u32, new: u32) {
    use serde_json::Value;
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_u64() {
                if i == old as u64 {
                    *n = serde_json::Number::from(new);
                }
            }
        }
        Value::Array(a) => {
            for item in a.iter_mut() {
                rewrite_u32(item, old, new);
            }
        }
        Value::Object(o) => {
            for (_, v) in o.iter_mut() {
                rewrite_u32(v, old, new);
            }
        }
        _ => {}
    }
}

/// Returns the path component the post-rename redirect should use. Prefer
/// the new label if any, fall back to decimal hash.
fn do_edit_id(
    db: &Path,
    target: &str,
    new_id_input: &str,
    renumber: bool,
) -> anyhow::Result<String> {
    use wwise_format::*;

    if new_id_input.is_empty() {
        anyhow::bail!("new id is empty");
    }

    let mut sb = wwise_storage::read_soundbank(db)?;
    let old_hash = resolve_target(target, &sb)
        .ok_or_else(|| anyhow::anyhow!("no such object: {target}"))?;

    let new_id = parse_object_id(new_id_input);
    let new_hash = new_id.as_hash();

    if new_hash != old_hash && !renumber {
        anyhow::bail!(
            "renaming would change the FNV hash from {old_hash} (0x{:08x}) to {new_hash} (0x{:08x}); \
             tick `renumber` to also rewrite all references",
            old_hash,
            new_hash
        );
    }
    // Refuse silent collisions — two HIRC objects sharing a hash means the
    // bank can no longer be addressed unambiguously.
    if new_hash != old_hash {
        let collision = sb.sections.iter().any(|s| match &s.body {
            SectionBody::HIRC(h) => h.objects.iter().any(|o| o.id.as_hash() == new_hash),
            _ => false,
        });
        if collision {
            anyhow::bail!(
                "hash {new_hash} (0x{:08x}) is already used by another object — refusing to merge them",
                new_hash
            );
        }
    }

    let hirc = sb
        .sections
        .iter_mut()
        .find_map(|s| match &mut s.body {
            SectionBody::HIRC(h) => Some(h),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("no HIRC section"))?;

    // Rename the object itself.
    let obj = hirc
        .objects
        .iter_mut()
        .find(|o| o.id.as_hash() == old_hash)
        .ok_or_else(|| anyhow::anyhow!("object {old_hash} disappeared from HIRC"))?;
    obj.id = new_id.clone();

    // Renumber every other reference if the hash actually changed.
    if renumber && new_hash != old_hash {
        for o in hirc.objects.iter_mut() {
            // Skip the renamed object's own id — it's already correct.
            if o.id.as_hash() == new_hash {
                continue;
            }
            let mut value = serde_json::to_value(&o.body)?;
            rewrite_u32(&mut value, old_hash, new_hash);
            o.body = serde_json::from_value(value)?;
        }
    }

    wwise_storage::write_soundbank(db, &sb, &Default::default())?;

    // Redirect target: use the label when we have one, otherwise decimal.
    Ok(match new_id {
        ObjectId::String(s) => s,
        ObjectId::Hash(h) => h.to_string(),
    })
}

fn resolve_target(target: &str, sb: &wwise_format::Soundbank) -> Option<u32> {
    use wwise_format::*;
    if let Some(stripped) = target
        .strip_prefix("0x")
        .or_else(|| target.strip_prefix("0X"))
    {
        if let Ok(v) = u32::from_str_radix(stripped, 16) {
            if has_id(sb, v) {
                return Some(v);
            }
        }
    }
    if let Ok(v) = target.parse::<u32>() {
        if has_id(sb, v) {
            return Some(v);
        }
    }
    sb.sections.iter().find_map(|s| match &s.body {
        SectionBody::HIRC(h) => h.objects.iter().find_map(|o| {
            if let ObjectId::String(name) = &o.id {
                if name == target {
                    return Some(o.id.as_hash());
                }
            }
            None
        }),
        _ => None,
    })
}

fn has_id(sb: &wwise_format::Soundbank, id: u32) -> bool {
    use wwise_format::*;
    sb.sections.iter().any(|s| match &s.body {
        SectionBody::HIRC(h) => h.objects.iter().any(|o| o.id.as_hash() == id),
        _ => false,
    })
}
