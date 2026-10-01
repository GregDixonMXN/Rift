//! First golden snapshot: full ChangeSet JSON for a synthetic agent patch.
//!
//! Run: `cargo test -p rift-analysis --test snapshot_golden`
//! Bless: `UPDATE_SNAPSHOTS=1 cargo test -p rift-analysis --test snapshot_golden`
//!
//! The snapshot pins severity/category/priority/evidence so scoring
//! regressions fail loudly. `repo_root` is normalized to `<ROOT>`.

use rift_core::ChangeSet;
use std::path::{Path, PathBuf};

const AUTH_V1: &str = r#"pub const SESSION_TIMEOUT: u64 = 900;

pub fn validate_token(token: &str) -> bool {
    if token.len() <= 10 {
        return false;
    }
    true
}
"#;

const AUTH_V2: &str = r#"pub const SESSION_TIMEOUT: u64 = 86400;

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

fn write_file(repo_path: &Path, name: &str, content: &str) {
    let p = repo_path.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdirs");
    }
    std::fs::write(&p, content).expect("write fixture");
}

fn snapshot_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join("agent_patch.json")
}

fn build_changeset() -> ChangeSet {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = git2::Repository::init(dir.path()).expect("init");
    write_file(dir.path(), "src/auth.rs", AUTH_V1);
    write_file(dir.path(), "src/lib.rs", LIB_V1);
    commit_all(&repo, "baseline");
    write_file(dir.path(), "src/auth.rs", AUTH_V2);
    write_file(dir.path(), "src/lib.rs", LIB_V2);
    commit_all(&repo, "agent patch");

    let engine = rift_git::GitEngine::discover(dir.path()).expect("discover");
    let mut files = engine.range_changeset("HEAD~1", "HEAD").expect("range");
    assert_eq!(files.len(), 2, "fixture should touch 2 files");

    let (syms, items) =
        rift_analysis::analyze("<ROOT>", "HEAD~1", "HEAD", &mut files);
    let mut cs = ChangeSet {
        repo_root: "<ROOT>".to_string(),
        base_ref: "HEAD~1".into(),
        head_ref: "HEAD".into(),
        files,
        symbol_changes: syms,
        review_items: items,
        stats: Default::default(),
    };
    rift_analysis::fill_stats(&mut cs);

    // Deterministic ordering for stable JSON.
    cs.files.sort_by(|a, b| a.display_path().cmp(b.display_path()));
    cs.symbol_changes
        .sort_by(|a, b| (&a.file, &a.name).cmp(&(&b.file, &b.name)));
    cs.review_items.sort_by(|a, b| a.id.cmp(&b.id));
    cs
}

#[test]
fn golden_agent_patch() {
    let cs = build_changeset();

    // Flagship invariants travel with the snapshot so failures explain themselves.
    assert!(
        cs.review_items
            .iter()
            .find(|r| r.files.iter().any(|f| f.ends_with("auth.rs")))
            .map(|r| r.severity >= rift_core::Severity::High)
            .unwrap_or(false),
        "auth.rs should stay HIGH+ severity"
    );

    let json = serde_json::to_string_pretty(&cs).expect("json");
    let path = snapshot_path();

    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().expect("snapshot dir")).expect("mkdir snapshots");
        std::fs::write(&path, format!("{json}\n")).expect("write snapshot");
        return;
    }

    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing snapshot {}; bless with UPDATE_SNAPSHOTS=1", path.display()));
    assert_eq!(
        json.trim(),
        expected.trim(),
        "ChangeSet snapshot drifted — review the diff; if intended, re-bless with UPDATE_SNAPSHOTS=1"
    );
}
