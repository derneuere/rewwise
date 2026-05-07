# Rewwise 🔊

A set of tools for working with Elden Ring, Armored Core 6 and Nightreign soundbanks — unpack, edit, and repack `.bnk` files.

This fork adds a **SQLite-backed soundbank format** (`soundbank.db`) and a small CLI editor (`bnk-edit`) that automates the "duplicate an event into another bank" workflow described in [Themyys' guide](https://docs.google.com/document/d/1lNov-a0DwnMY2yZywH3hFYzuoDfndofguvZmAnLDo-U/edit). The original `bnk2json` workflow still works unchanged.

## Why SQLite?

`soundbank.json` for `cs_main.bnk` is around 3.6 million lines — search and diff get painful, and copy-pasting whole subtrees by hand is error-prone. The new flow stores each HIRC object (Sound / Action / Event / SequenceContainer / ActorMixer / Bus / …) as its own row, with the WEM payloads in a parallel BLOB table. That gives you:

- O(1) lookup by FNV hash *or* dictionary name.
- Indexed routing queries: `SELECT * FROM hirc_objects WHERE direct_parent = ?`.
- Atomic, row-level edits — no more "missing comma broke the whole bank".
- Per-event copy commands that move the right HIRC chain *and* the WEMs in one shot.

Round-trips back through the existing Deku writer, so the resulting `.bnk` is byte-for-byte equivalent to what `bnk2json` would have produced.

## Layout

| crate            | what's inside |
|------------------|---------------|
| [`format`](format/)     | Deku parser/writer + `bnk2json` binary (unchanged behavior). |
| [`analysis`](analysis/) | Audio-routing, dictionary, FNV hash helpers. |
| [`util`](util/)         | `audio-routing` Graphviz exporter, `fnv-hash` CLI. |
| [`storage`](storage/)   | SQLite schema + `read_soundbank` / `write_soundbank` / `import_bnk` / `export_bnk`, plus the `bnk2sqlite` binary. |
| [`editor`](editor/)     | `bnk-edit` CLI: import/export/find/inspect/tree/copy-event/add-child. |
| [`viewer`](viewer/)     | `bnk-viewer` — a small [Leptos](https://leptos.dev/)-rendered web UI for browsing/editing a `.db` in the browser. |

## Building

```sh
cargo build --release
```

This produces these binaries in `target/release`:

- `bnk2json`     — the original JSON unpack/repack tool.
- `bnk2sqlite`   — same drag-and-drop UX, but reads/writes `.db` instead of `.json` + folder.
- `bnk-edit`     — surgical editor described below.
- `bnk-viewer`   — local web UI for browsing/editing a `.db`.
- `audio-routing`, `fnv-hash` — pre-existing utilities.

## Workflows

### A. Old JSON workflow (unchanged)

Drag `cs_c2110.bnk` onto `bnk2json` → produces `cs_c2110/soundbank.json` plus `*.wem` files. Edit the JSON, drag the folder back onto `bnk2json` → produces `cs_c2110.created.bnk`.

### B. New SQLite workflow

```sh
# Unpack — produces cs_c2110.db with everything inline (HIRC + WEMs).
bnk2sqlite path/to/cs_c2110.bnk

# Repack — produces cs_c2110.created.bnk
bnk2sqlite path/to/cs_c2110.db
```

`bnk2sqlite` automatically picks up a `dictionary.txt` from the working directory if present (one symbol per line, `#` comments allowed); otherwise it falls back to the bundled FNV dictionary.

### C. Duplicating an event with `bnk-edit`

This automates the [Themyys "use Maliketh's roar in your moveset" guide](https://docs.google.com/document/d/1lNov-a0DwnMY2yZywH3hFYzuoDfndofguvZmAnLDo-U/edit). The example below moves `Play_c211006013` (Maliketh's roar) and its `Stop_…` counterpart from `cs_c2110.db` into `cs_main.db`, and splices the resulting RandomSequenceContainer into the destination's existing ActorMixer:

```sh
# 1) unpack both banks
bnk2sqlite cs_c2110.bnk
bnk2sqlite cs_main.bnk

# 2) sanity-check that we can find the event
bnk-edit find cs_c2110.db --like 'Play_c2110%'

# 3) inspect the dependency tree before touching anything
bnk-edit tree cs_c2110.db Play_c211006013

# 4) dry-run the copy
bnk-edit copy-event cs_c2110.db cs_main.db Play_c211006013 \
    --register-with 381030457 --dry-run

# 5) for real (also do Stop_)
bnk-edit copy-event cs_c2110.db cs_main.db Play_c211006013 \
    --register-with 381030457
bnk-edit copy-event cs_c2110.db cs_main.db Stop_c211006013 \
    --register-with 381030457

# 6) repack
bnk2sqlite cs_main.db
# -> cs_main.created.bnk, drop in your sd\enus\ as before
```

`copy-event` walks the Event → Action → SequenceContainer → Sound chain (transitively, also through Switch/Layer containers when present), copies every node into the destination HIRC, copies every referenced WEM blob into the destination DB, and inserts the top-level container's id into the parent ActorMixer's children list. Existing IDs in the destination are skipped, never overwritten.

### D. Browsing a bank in the browser

`bnk-viewer` runs a tiny local web app rendered with [Leptos](https://leptos.dev/) (server-side; no JS bundle, just plain HTML + form posts):

```sh
bnk-viewer cs_main.db          # opens http://127.0.0.1:3939
bnk-viewer cs_main.db -p 8080  # custom port
```

It exposes:

- **`/`** — search by label substring, filter by HIRC kind. Each result links to `/object/<label-or-id>`.
- **`/object/<id-or-name>`** — shows kind, label, parent, list of outgoing references (each clickable), the body JSON pretty-printed, and an *Add child* form on container/mixer pages that POSTs to `/add-child`.

Launch it side-by-side with `bnk-edit copy-event` runs to inspect what landed where, or treat it as a read-only viewer if you don't trust yet-untested mutations.

### `bnk-edit` reference

```
bnk-edit import   <bnk> <db> [--dictionary <path>]
bnk-edit export   <db>  <bnk>
bnk-edit find     <db>  [--kind <kind>] [--like <pattern>]
bnk-edit inspect  <db>  <id-or-name>
bnk-edit tree     <db>  <id-or-name>
bnk-edit copy-event <src.db> <dst.db> <event-name> [--register-with <id-or-name>] [--dry-run]
bnk-edit add-child  <db>  <parent-id-or-name> <child-id-or-name>
```

`<id-or-name>` accepts:
- a dictionary name, e.g. `Play_c211006013`
- a decimal hash, e.g. `1834890111`
- a hex hash, e.g. `0x6D5C6A3F`

## SQLite schema

See [`storage/src/schema.sql`](storage/src/schema.sql) for the canonical version. Briefly:

```sql
CREATE TABLE meta          (key, value);
CREATE TABLE sections      (ord PRIMARY KEY, magic, body_json);   -- HIRC body_json is NULL
CREATE TABLE hirc_objects  (section_ord, ord, id_kind, id_value,
                            id_hash, label, body_kind, body_type,
                            direct_parent, override_bus, body_json);
CREATE TABLE wems          (id PRIMARY KEY, payload BLOB);
```

Indexed by `id_hash`, `label`, `body_kind`, and `direct_parent` so most editor queries hit an index. The `body_json` columns hold the same data serde produces for `bnk2json`, just split per-object — round-tripping a `.bnk` through `import_bnk` + `export_bnk` produces the same bytes as `bnk2json` would.

## File reference

#### Soundbank.json / .db
Both describe the event routing, bussing structure, looping of music, etc. Pick whichever workflow suits your editor.

#### WEMs
`.wem` files are the actual audio payload. With `bnk2json` they live as files alongside `soundbank.json`; with `bnk2sqlite` they live as BLOBs inside `soundbank.db`. Use [vgmstream](https://vgmstream.org/) to convert WEM ↔ wav. Putting custom audio into a bank still requires Wwise Studio to encode the WEM — [this video](https://www.youtube.com/watch?v=39Oeb4GvxEc) walks through it.

### Still questions on proper usage?
Check out [Themyys' guide](https://docs.google.com/document/d/1lNov-a0DwnMY2yZywH3hFYzuoDfndofguvZmAnLDo-U/edit#heading=h.7dtqo3tlss5x).

## Credits
- Wwiser project for their parsing code
- [Deku](https://github.com/sharksforarms/deku) for the parser code
- Shion and SekiroDubi for testing
- Themyys for the soundbank-editing guide that inspired the SQLite tooling
