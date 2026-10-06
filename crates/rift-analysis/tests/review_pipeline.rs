//! End-to-end review pipeline test on a synthetic agent-generated patch.
//!
//! Builds a temp git repo with two commits:
//!   1. baseline: auth module, a function to be renamed, an indent-sensitive file
//!   2. "agent" commit: session timeout widened + validation removed,
//!      function renamed, whitespace-only reflow, lockfile added
//!
//! Asserts Rift's flagship conclusions, not just plumbing.

use rift_core::{Category, Severity, SymbolChangeKind};
use std::path::Path;

const AUTH_V1: &str = r#"pub const SESSION_TIMEOUT: u64 = 900;

fn is_valid(token: &str) -> bool {
    token.len() > 10
}

pub fn validate_token(token: &str) -> bool {
    if !is_valid(token) {
        return false;
    }
    true
}
"#;

const AUTH_V2: &str = r#"pub const SESSION_TIMEOUT: u64 = 86400;

fn is_valid(token: &str) -> bool {
    token.len() > 10
}

pub fn validate_token(token: &str) -> bool {
    true
}
"#;

const LIB_V1: &str = r#"pub fn fetch_user(id: u64) -> String {
    format!("user-{id}")
}
"#;

const LIB_V2: &str = r#"pub fn fetch_user_by_id(id: u64) -> String {
    format!("user-{id}")
}
"#;

const FMT_V1: &str = "pub fn total(xs: &[i32]) -> i32 {\n    xs.iter().sum()\n}\n";
const FMT_V2: &str = "pub fn total(xs: &[i32]) -> i32 {\n        xs.iter().sum()\n}\n";

const LOCK: &str = "[[package]]\nname = \"demo\"\nversion = \"0.1.0\"\n";

fn commit_all(repo: &git2::Repository, message: &str) {
    let mut index = repo.index().expect("index");
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("add_all");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let sig = git2::Signature::now("rift-test", "rift@test").expect("sig");
    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(h) => vec![h.peel_to_commit().expect("parent")],
        Err(_) => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .expect("commit");
}

fn write(repo_path: &Path, name: &str, content: &str) {
    let p = repo_path.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdirs");
    }
    std::fs::write(&p, content).expect("write fixture");
}

fn build_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = git2::Repository::init(dir.path()).expect("init");
    write(dir.path(), "src/auth.rs", AUTH_V1);
    write(dir.path(), "src/lib.rs", LIB_V1);
    write(dir.path(), "src/fmt.rs", FMT_V1);
    commit_all(&repo, "baseline");
    write(dir.path(), "src/auth.rs", AUTH_V2);
    write(dir.path(), "src/lib.rs", LIB_V2);
    write(dir.path(), "src/fmt.rs", FMT_V2);
    write(dir.path(), "Cargo.lock", LOCK);
    commit_all(&repo, "agent: password reset + cleanup");
    dir
}

#[test]
fn agent_patch_review() {
    let dir = build_fixture();
    let engine = rift_git::GitEngine::discover(dir.path()).expect("discover");
    let mut files = engine
        .range_changeset("HEAD~1", "HEAD")
        .expect("range diff");
    assert_eq!(files.len(), 4, "expected 4 changed files");

    let root = engine.root.to_string_lossy().to_string();
    let (syms, items) = rift_analysis::analyze(&root, "HEAD~1", "HEAD", &mut files);

    // 1. The auth change is HIGH+ with validation-removal evidence.
    let auth = items
        .iter()
        .find(|r| r.files.iter().any(|f| f.ends_with("auth.rs")))
        .expect("auth review item");
    assert!(
        auth.severity >= Severity::High,
        "auth item severity was {:?}",
        auth.severity
    );
    assert!(
        auth.evidence.iter().any(|e| e.kind == "auth-surface"),
        "missing auth-surface evidence"
    );
    assert!(
        auth.evidence.iter().any(|e| e.kind == "validation-removed"),
        "missing validation-removed evidence"
    );
    assert!(
        auth.evidence.iter().any(|e| e.summary.contains("86400"))
            || auth.symbols.iter().any(|s| s.contains("SESSION_TIMEOUT")),
        "timeout change not surfaced"
    );

    // 2. The rename is detected as a rename, not add+delete.
    assert!(
        syms.iter().any(|s| matches!(
            s.change,
            SymbolChangeKind::Renamed
        ) && s.name.contains("fetch_user")),
        "expected a Renamed symbol change, got: {:?}",
        syms.iter()
            .map(|s| format!("{:?} {}", s.change, s.name))
            .collect::<Vec<_>>()
    );

    // 3. Whitespace-only reflow and lockfile collapse into mechanical.
    let mech: Vec<_> = items
        .iter()
        .filter(|r| r.category == Category::Mechanical)
        .collect();
    let mech_files: Vec<_> = mech.iter().flat_map(|r| r.files.clone()).collect();
    assert!(
        mech_files.iter().any(|f| f.ends_with("fmt.rs")),
        "fmt.rs should be mechanical, mech files: {mech_files:?}"
    );
    assert!(
        mech_files.iter().any(|f| f.ends_with("Cargo.lock")),
        "Cargo.lock should be mechanical, mech files: {mech_files:?}"
    );

    // 4. Stats tell the north-star story: few meaningful, rest collapsed.
    let mut cs = rift_core::ChangeSet {
        repo_root: root,
        base_ref: "HEAD~1".into(),
        head_ref: "HEAD".into(),
        files,
        symbol_changes: syms,
        review_items: items,
        stats: Default::default(),
        task_check: None,
    };
    rift_analysis::fill_stats(&mut cs);
    assert!(
        cs.stats.meaningful_changes <= 3,
        "too many meaningful items: {}",
        cs.stats.meaningful_changes
    );
    assert_eq!(cs.stats.mechanical_files, 2);
}

#[test]
fn behavior_changes_survive_formatting_filters() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = git2::Repository::init(dir.path()).expect("init");
    let cases = [
        (
            "src/auth.rs",
            "pub fn authenticate() {\n    validate();\n    grant();\n}\n",
            "pub fn authenticate() {\n    grant();\n    validate();\n}\n",
            "authenticate",
        ),
        (
            "src/session.py",
            "def save_session():\n    if ready():\n        run()\n        save()\n",
            "def save_session():\n    if ready():\n        run()\n    save()\n",
            "save_session",
        ),
        (
            "src/message.rs",
            "pub fn message() -> &'static str {\n    \"a b\"\n}\n",
            "pub fn message() -> &'static str {\n    \"ab\"\n}\n",
            "message",
        ),
        (
            "src/template.rs",
            "pub fn template() -> &'static str {\n    r#\"first\n    second\"#\n}\n",
            "pub fn template() -> &'static str {\n    r#\"first\n        second\"#\n}\n",
            "template",
        ),
    ];
    for (path, old, _, _) in cases {
        write(dir.path(), path, old);
    }
    commit_all(&repo, "baseline");
    for (path, _, new, _) in cases {
        write(dir.path(), path, new);
    }
    commit_all(&repo, "behavior changes");
    let engine = rift_git::GitEngine::discover(dir.path()).expect("discover");
    let mut files = engine.range_changeset("HEAD~1", "HEAD").expect("diff");
    let root = engine.root.to_string_lossy().to_string();
    let (symbols, items) = rift_analysis::analyze(&root, "HEAD~1", "HEAD", &mut files);
    for (path, _, _, name) in cases {
        assert!(
            items
                .iter()
                .any(|item| item.category != Category::Mechanical
                    && item.files.iter().any(|file| file == path)),
            "missing meaningful item for {path}"
        );
        assert!(
            !items
                .iter()
                .any(|item| item.category == Category::Mechanical
                    && item.files.iter().any(|file| file == path)),
            "hidden change in {path}"
        );
        assert!(
            symbols.iter().any(|symbol| symbol.file == path
                && symbol.name == name
                && symbol.change == SymbolChangeKind::Modified),
            "missing modified symbol for {path}: {symbols:?}"
        );
    }
}
