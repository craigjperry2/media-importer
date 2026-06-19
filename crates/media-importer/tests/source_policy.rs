use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

#[test]
fn application_sql_is_loaded_from_colocated_sql_files() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let rust_files = rust_files_below(&source_root);
    let inline_sql = Regex::new(concat!(
        r#"(?is)(?:"#,
        r#"\.(?:prepare|query_row|execute|execute_batch)\(\s*"#,
        r#"|(?:const|static)\s+[A-Z_][A-Z0-9_]*\s*:\s*&str\s*=\s*"#,
        r#"|let\s+[a-z_][a-z0-9_]*(?:\s*:\s*&str)?\s*=\s*"#,
        r#")(?:br|rb|r|b)?\#*"\s*"#,
        r#"(?:SELECT|INSERT|UPDATE|DELETE|CREATE|ALTER|DROP|PRAGMA|BEGIN|COMMIT|ROLLBACK)\b"#,
    ))
    .expect("inline SQL policy regex is valid");

    let mut violations = Vec::new();
    for path in rust_files {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read production Rust source {path:?}: {error}"));
        for matched in inline_sql.find_iter(&source) {
            let line = source[..matched.start()]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            violations.push(format!("{}:{line}", path.display()));
        }
    }

    assert!(
        violations.is_empty(),
        "inline application SQL detected at {}. Move SQL to a colocated .sql file and load it with include_str!",
        violations.join(", ")
    );
}

fn rust_files_below(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read source directory {directory:?}: {error}"));
        for entry in entries {
            let entry = entry.expect("read source directory entry");
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}
