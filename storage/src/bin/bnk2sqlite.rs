//! `bnk2sqlite` — drop-in alternative to `bnk2json` that writes a SQLite DB.
//!
//! Usage mirrors `bnk2json`: drag a `.bnk` onto it to produce a `.db`, drag a
//! `.db` (or, in the future, a directory) onto it to repack to `.bnk`. Lives
//! in `wwise_storage` because it depends on the storage layer.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use wwise_storage as st;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: {} <path> [<path>...]\n\
             \n  .bnk path -> writes <stem>.db next to it (with extracted dictionary names)\n\
             \n  .db  path -> writes <stem>.created.bnk next to it",
            args.first().map(String::as_str).unwrap_or("bnk2sqlite")
        );
        std::process::exit(2);
    }

    let dictionary = load_default_dictionary();

    for arg in &args[1..] {
        let path = PathBuf::from(arg);
        if let Err(e) = process(&path, &dictionary) {
            eprintln!("{}: {e:#}", path.display());
            std::process::exit(1);
        }
    }
}

fn process(path: &Path, dictionary: &st::FNVDictionary) -> Result<(), Box<dyn std::error::Error>> {
    let md = fs::metadata(path)?;
    if !md.is_file() {
        return Err(format!("not a regular file: {}", path.display()).into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "bnk" => {
            let out = path.with_extension("db");
            st::import_bnk(path, &out, dictionary)?;
            println!("imported  {} -> {}", path.display(), out.display());
        }
        "db" => {
            let mut out = path.to_path_buf();
            // Match the `bnk2json` convention of `.created.bnk`.
            let stem = out.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.set_file_name(format!("{stem}.created.bnk"));
            st::export_bnk(path, &out)?;
            println!("exported  {} -> {}", path.display(), out.display());
        }
        other => return Err(format!("unsupported extension {other:?}").into()),
    }
    Ok(())
}

fn load_default_dictionary() -> st::FNVDictionary {
    // Reuse the dictionary that ships with the format crate.
    let cwd_dict = env::current_dir().ok().map(|p| p.join("dictionary.txt"));
    if let Some(p) = cwd_dict {
        if let Ok(text) = fs::read_to_string(&p) {
            return st::parse_dictionary(&text);
        }
    }
    st::parse_dictionary(include_str!("../../../format/src/bin/default_dictionary.txt"))
}
