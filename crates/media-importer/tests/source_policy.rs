use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Violation {
    path: PathBuf,
    line: usize,
    rule: &'static str,
    repair: String,
}

impl Violation {
    fn new(path: &Path, line: usize, rule: &'static str, repair: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            line,
            rule,
            repair: repair.into(),
        }
    }
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate is inside the workspace")
        .to_path_buf()
}

fn files_below(root: &Path, extension: &str) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read directory {directory:?}: {error}"))
            .map(|entry| entry.expect("read directory entry").path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries.into_iter().rev() {
            if path.is_dir() {
                if path.file_name().is_none_or(|name| name != "target") {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|value| value == extension) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path:?}: {error}"))
}

fn line_at(source: &str, byte_offset: usize) -> usize {
    source[..byte_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn assert_no_violations(mut violations: Vec<Violation>) {
    violations.sort();
    violations.dedup();
    if violations.is_empty() {
        return;
    }
    let diagnostics = violations
        .into_iter()
        .map(|violation| {
            format!(
                "{}:{}: [{}] {}",
                violation.path.display(),
                violation.line,
                violation.rule,
                violation.repair
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    panic!("source policy violations:\n{diagnostics}");
}

fn production_rust_files() -> Vec<PathBuf> {
    files_below(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), "rs")
}

fn workspace_manifests() -> Vec<PathBuf> {
    files_below(&repository_root(), "toml")
        .into_iter()
        .filter(|path| path.file_name().is_some_and(|name| name == "Cargo.toml"))
        .collect()
}

#[test]
fn production_uses_stable_rust_2024() {
    let mut violations = Vec::new();
    let package = Regex::new(r"(?m)^\s*\[package\]\s*$").unwrap();
    let edition_2024 = Regex::new(r#"(?m)^\s*edition\s*=\s*"2024"\s*(?:#.*)?$"#).unwrap();
    for path in workspace_manifests() {
        let source = read(&path);
        if package.is_match(&source) && !edition_2024.is_match(&source) {
            violations.push(Violation::new(
                &path,
                1,
                "rust-edition-2024",
                "restore `edition = \"2024\"` for this package",
            ));
        }
    }
    let feature = Regex::new(r"#!\s*\[\s*feature\s*\(").unwrap();
    for path in production_rust_files() {
        let source = read(&path);
        for found in feature.find_iter(&source) {
            violations.push(Violation::new(
                &path,
                line_at(&source, found.start()),
                "stable-rust",
                "replace nightly-only functionality with stable Rust",
            ));
        }
    }
    assert_no_violations(violations);
}

#[test]
fn lint_allowances_are_narrow_and_documented() {
    let crate_allow = Regex::new(r"#!\s*\[\s*allow\s*\(").unwrap();
    let item_allow = Regex::new(r"#\s*\[\s*allow\s*\(").unwrap();
    let mut violations = Vec::new();
    for path in production_rust_files() {
        let source = read(&path);
        for found in crate_allow.find_iter(&source) {
            violations.push(Violation::new(&path, line_at(&source, found.start()), "crate-level-allow", "fix the lint or use the narrowest item-level allowance with a nearby explanatory comment"));
        }
        for found in item_allow.find_iter(&source) {
            if crate_allow.is_match(&source[found.start().saturating_sub(1)..found.end()]) {
                continue;
            }
            let line = line_at(&source, found.start());
            let lines = source.lines().collect::<Vec<_>>();
            let documented = line > 1
                && lines[..line - 1]
                    .iter()
                    .rev()
                    .take(2)
                    .any(|candidate| candidate.trim_start().starts_with("//"));
            if !documented {
                violations.push(Violation::new(&path, line, "undocumented-item-allow", "fix the lint or add a nearby explanatory comment to the narrowest item-level allowance"));
            }
        }
    }
    assert_no_violations(violations);
}

#[test]
fn binary_installs_color_eyre_once() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let source = read(&path);
    let install = Regex::new(r"color_eyre\s*::\s*install\s*\(\s*\)").unwrap();
    let count = install.find_iter(&source).count();
    assert_no_violations(if count == 1 {
        Vec::new()
    } else {
        vec![Violation::new(
            &path,
            1,
            "color-eyre-install",
            format!(
                "call `color_eyre::install()` exactly once in the binary entrypoint; found {count} calls"
            ),
        )]
    });
}

#[test]
fn pre_commit_keeps_filename_independent_rust_hooks() {
    let path = repository_root().join(".pre-commit-config.yaml");
    let source = read(&path);
    let required = [
        "cargo fmt --all --check",
        "cargo clippy --workspace --all-targets -- -D warnings",
        "cargo test --workspace",
    ];
    let mut violations = Vec::new();
    for command in required {
        let Some(offset) = source.find(&format!("entry: {command}")) else {
            violations.push(Violation::new(&path, 1, "required-pre-commit-hook", format!("restore a local hook with `entry: {command}`, `pass_filenames: false`, and `always_run: true`")));
            continue;
        };
        let block_end = source[offset..]
            .find("\n      - id:")
            .map_or(source.len(), |relative| offset + relative);
        let block = &source[offset..block_end];
        if !block.contains("pass_filenames: false") || !block.contains("always_run: true") {
            violations.push(Violation::new(
                &path,
                line_at(&source, offset),
                "filename-independent-pre-commit-hook",
                format!(
                    "set `pass_filenames: false` and `always_run: true` on the `{command}` hook"
                ),
            ));
        }
    }
    assert_no_violations(violations);
}

#[test]
fn stdout_is_rendered_only_by_cli_facing_modules() {
    let stdout = Regex::new(r"(?m)(?:^|[^[:alnum:]_])(?:print|println)!\s*\(").unwrap();
    let mut violations = Vec::new();
    for path in production_rust_files() {
        if path
            .file_name()
            .is_some_and(|name| name == "cli.rs" || name == "main.rs")
        {
            continue;
        }
        let source = read(&path);
        for found in stdout.find_iter(&source) {
            violations.push(Violation::new(
                &path,
                line_at(&source, found.start()),
                "stdout-outside-cli",
                "return structured results or events and render them in the CLI layer",
            ));
        }
    }
    assert_no_violations(violations);
}

#[test]
fn rusqlite_is_confined_to_catalog_modules() {
    let reference = Regex::new(r"\brusqlite\s*(?:::|\{)").unwrap();
    let mut violations = Vec::new();
    for path in production_rust_files() {
        let catalog_module = path.file_name().is_some_and(|name| name == "catalog.rs")
            || path
                .components()
                .any(|component| component.as_os_str() == "catalog");
        if catalog_module {
            continue;
        }
        let source = read(&path);
        for found in reference.find_iter(&source) {
            violations.push(Violation::new(&path, line_at(&source, found.start()), "rusqlite-outside-catalog", "add a behavior-level catalog API instead of exposing connections, transactions, PRAGMAs, migrations, or raw SQL"));
        }
    }
    assert_no_violations(violations);
}

#[test]
fn workspace_uses_rusqlite_and_no_known_orm() {
    let dependency = Regex::new(r"(?m)^\s*([A-Za-z0-9_-]+)\s*=").unwrap();
    let section =
        Regex::new(r"(?m)^\s*\[(?:workspace\.)?(?:dev-|build-)?dependencies(?:\.[^]]+)?\]\s*$")
            .unwrap();
    let forbidden = ["diesel", "sqlx", "sea-orm", "seaorm", "rbatis"];
    let mut found_rusqlite = false;
    let mut violations = Vec::new();
    for path in workspace_manifests() {
        let source = read(&path);
        for header in section.find_iter(&source) {
            let start = header.end();
            let end = source[start..]
                .find("\n[")
                .map_or(source.len(), |relative| start + relative);
            for found in dependency.captures_iter(&source[start..end]) {
                let name = found[1].to_ascii_lowercase();
                found_rusqlite |= name == "rusqlite";
                if forbidden.contains(&name.as_str()) {
                    let offset = start + found.get(0).unwrap().start();
                    violations.push(Violation::new(&path, line_at(&source, offset), "forbidden-orm-dependency", format!("remove the `{name}` dependency and use the workspace's behavior-level rusqlite catalog APIs")));
                }
            }
        }
    }
    if !found_rusqlite {
        violations.push(Violation::new(
            &repository_root().join("Cargo.toml"),
            1,
            "required-rusqlite-dependency",
            "add `rusqlite` as a workspace production dependency",
        ));
    }
    assert_no_violations(violations);
}

#[test]
fn application_sql_is_external_colocated_and_referenced() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let rust_files = production_rust_files();
    let sql_files = files_below(&source_root, "sql")
        .into_iter()
        .collect::<BTreeSet<_>>();
    let inline_sql = Regex::new(concat!(
        r#"(?is)(?:"#,
        r#"\.(?:prepare|query_row|execute|execute_batch)\(\s*"#,
        r#"|(?:const|static)\s+[A-Z_][A-Z0-9_]*\s*:\s*&str\s*=\s*"#,
        r#"|let\s+[a-z_][a-z0-9_]*(?:\s*:\s*&str)?\s*=\s*"#,
        r#")(?:br|rb|r|b)?\#*"\s*"#,
        r#"(?:SELECT|INSERT|UPDATE|DELETE|CREATE|ALTER|DROP|PRAGMA|BEGIN|COMMIT|ROLLBACK)\b"#
    ))
    .unwrap();
    let include = Regex::new(r#"include_str!\s*\(\s*"([^"]+\.sql)"\s*\)"#).unwrap();
    let mut referenced = BTreeSet::new();
    let mut violations = Vec::new();
    for path in rust_files {
        let source = read(&path);
        for found in inline_sql.find_iter(&source) {
            violations.push(Violation::new(
                &path,
                line_at(&source, found.start()),
                "inline-application-sql",
                "move SQL to a colocated `.sql` file and load it with `include_str!`",
            ));
        }
        for capture in include.captures_iter(&source) {
            let found = capture.get(0).unwrap();
            let resolved = path.parent().unwrap().join(&capture[1]);
            if !resolved.exists() {
                violations.push(Violation::new(
                    &path,
                    line_at(&source, found.start()),
                    "missing-included-sql",
                    format!(
                        "create the referenced SQL file `{}` or correct the include path",
                        resolved.display()
                    ),
                ));
                continue;
            }
            let canonical = resolved.canonicalize().expect("canonical included SQL");
            let module_dir = path
                .parent()
                .unwrap()
                .canonicalize()
                .expect("canonical module directory");
            if !canonical.starts_with(&module_dir) {
                violations.push(Violation::new(&path, line_at(&source, found.start()), "misplaced-included-sql", "move the SQL file beneath the including module's directory and update `include_str!`"));
            }
            referenced.insert(canonical);
        }
    }
    for sql in sql_files {
        let canonical = sql.canonicalize().expect("canonical production SQL");
        if !referenced.contains(&canonical) {
            violations.push(Violation::new(&sql, 1, "orphaned-production-sql", "load this production SQL file with `include_str!` from its owning module or delete it"));
        }
    }
    assert_no_violations(violations);
}

#[test]
fn writable_catalog_pragmas_are_complete() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/catalog/sql/writable_pragmas.sql");
    let normalized = read(&path)
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let required = BTreeMap::from([
        ("foreign_keys", "pragmaforeign_keys=on"),
        ("journal_mode", "pragmajournal_mode=wal"),
        ("synchronous", "pragmasynchronous=normal"),
    ]);
    let violations = required
        .into_iter()
        .filter(|(_, statement)| !normalized.contains(statement))
        .map(|(name, _)| {
            Violation::new(
                &path,
                1,
                "writable-catalog-pragma",
                format!(
                    "add `PRAGMA {name}` with the required value; case, whitespace, and statement ordering are unrestricted"
                ),
            )
        })
        .collect();
    assert_no_violations(violations);
}

#[test]
fn mount_identity_never_renders_the_raw_device_value() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scanner.rs");
    let source = read(&path);
    assert_no_violations(
        if source.contains("impl fmt::Display for MountId")
            || source.contains("write!(formatter, \"mount-")
        {
            vec![Violation::new(
                &path,
                1,
                "raw-mount-id-rendering",
                "keep MountId opaque; do not render its platform device value",
            )]
        } else {
            Vec::new()
        },
    );
}
