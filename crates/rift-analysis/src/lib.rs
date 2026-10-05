//! Deterministic importance model + review-item grouping.
//! No LLM, no randomness: every score traces to evidence.

use rift_core::{
    Category, ChangeSet, Evidence, FileChange, FileStatus, Language, ReviewItem, Severity,
    SymbolChange, SymbolChangeKind, SymbolKind,
};
use rift_parser::{
    called_names, diff_symbols, extract_cached, imported_names, test_targets, SymbolCache,
};
use std::collections::HashMap;

pub mod task;
pub use task::{check_task, check_task_parts, render_task_check, split_ident, task_terms};
pub mod escalate;
pub use escalate::{build_package, render_summary, DEFAULT_FLOOR, DEFAULT_MAX_ITEMS};

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

pub fn analyze(
    repo_root: &str,
    _base_ref: &str,
    _head_ref: &str,
    files: &mut [FileChange],
) -> (Vec<SymbolChange>, Vec<ReviewItem>) {
    mark_generated(files);
    // Symbol work is embarrassingly parallel (pure functions of file
    // contents); rayon keeps first paint fast on large patches. Collection
    // order is deterministic, and grouping sorts its output, so results are
    // identical to the sequential run.
    use rayon::prelude::*;
    let sym_changes: Vec<SymbolChange> = files
        .par_iter()
        .map(file_symbols)
        .collect::<Vec<Vec<SymbolChange>>>()
        .into_iter()
        .flatten()
        .collect();
    // Relocation reads as relocation: link cross-file moves and reorders
    // before grouping so delete+add pairs become Moved entries.
    let sym_changes = link_moves(files, sym_changes);
    let ctx = collect_context(repo_root, files);
    let mut items = group_items(files, &sym_changes);
    apply_test_coverage(files, &sym_changes, &mut items, &ctx);
    apply_blast_radius(files, &mut items, &ctx);
    (sym_changes, items)
}

/// Cached pipeline: same conclusions as [`analyze`], but identical
/// `(language, content)` inputs parse once per cache lifetime. Sequential
/// (not rayon): the UI worker streams per-file progress anyway, and cache
/// hits dominate on repeat runs. `cache` is both read and populated —
/// callers persist it with `SymbolCache::save`.
pub fn analyze_with_cache(
    repo_root: &str,
    _base_ref: &str,
    _head_ref: &str,
    files: &mut [FileChange],
    cache: &mut SymbolCache,
) -> (Vec<SymbolChange>, Vec<ReviewItem>) {
    mark_generated(files);
    let mut sym_changes = Vec::new();
    for f in files.iter() {
        sym_changes.extend(file_symbols_cached(f, cache));
    }
    let sym_changes = link_moves_with_cache(files, sym_changes, cache);
    let ctx = collect_context_with_cache(repo_root, files, cache);
    let mut items = group_items(files, &sym_changes);
    apply_test_coverage_with_cache(files, &sym_changes, &mut items, &ctx, cache);
    apply_blast_radius_with_cache(files, &mut items, &ctx, cache);
    (sym_changes, items)
}

/// Symbol-level changes for one file. Pure function — safe to run on any
/// thread, and reused by the UI's progressive worker one file at a time.
pub fn file_symbols(f: &FileChange) -> Vec<SymbolChange> {
    file_symbols_cached(f, &mut SymbolCache::new())
}

/// Direct-parse guard for the move/coverage/blast passes: generated and
/// binary files never produced symbols, and oversize contents pay a slow
/// parse for nothing (same cap as `file_symbols_cached`; untracked files
/// may carry up to 4× the normal content budget).
fn skip_parse(f: &FileChange) -> bool {
    f.is_binary
        || f.is_generated
        || f.old_content.as_ref().map(|c| c.len()).unwrap_or(0) > rift_parser::MAX_PARSE_BYTES
        || f.new_content.as_ref().map(|c| c.len()).unwrap_or(0) > rift_parser::MAX_PARSE_BYTES
}

/// Cached variant: shares parses across old/new sides and repeat calls.
pub fn file_symbols_cached(f: &FileChange, cache: &mut SymbolCache) -> Vec<SymbolChange> {
    if f.is_binary || f.is_generated {
        return Vec::new();
    }
    // Parsing is capped like content loading: oversize files keep line
    // stats and whole-file entries, never a multi-second parse.
    if skip_parse(f) {
        return Vec::new();
    }
    let path = f.display_path();
    let old_syms = f
        .old_content
        .as_deref()
        .map(|c| extract_cached(&f.old_path, f.language, c, cache))
        .unwrap_or_default();
    let new_syms = f
        .new_content
        .as_deref()
        .map(|c| extract_cached(path, f.language, c, cache))
        .unwrap_or_default();
    // For added/deleted whole files, synthesize from one side.
    let mut d = diff_symbols(
        path,
        &old_syms,
        &new_syms,
        f.old_content.as_deref(),
        f.new_content.as_deref(),
    );
    // Whole-file add/delete with no parseable symbols still deserves one entry.
    if d.is_empty()
        && matches!(
            f.status,
            FileStatus::Added | FileStatus::Deleted | FileStatus::Untracked
        )
        && (f.added_lines + f.deleted_lines) > 0
    {
        d.push(SymbolChange {
            file: path.to_string(),
            name: path.to_string(),
            kind: rift_core::SymbolKind::Module,
            change: if matches!(f.status, FileStatus::Deleted) {
                SymbolChangeKind::Removed
            } else {
                SymbolChangeKind::Added
            },
            confidence: 0.9,
            old_signature: None,
            new_signature: None,
            old_lines: None,
            new_lines: None,
            evidence: vec!["whole file added/removed".to_string()],
        });
    }
    d
}

// ---------------------------------------------------------------------------
// Move linking
// ---------------------------------------------------------------------------

/// Evidence prefix marking a cross-file move's origin file.
const MOVED_FROM_PREFIX: &str = "moved-from:";

/// Origin file of a cross-file Moved entry, if any. Reorder-within-file
/// moves carry no origin and stay with their file's review item.
fn move_origin(s: &SymbolChange) -> Option<&str> {
    s.evidence
        .first()
        .and_then(|e| e.strip_prefix(MOVED_FROM_PREFIX))
}

/// Body text for 1-based inclusive line spans from a pre-split line
/// table. Split once per file, slice many times: symbol diffing touches
/// every symbol, so per-call `lines().collect()` goes superlinear.
fn body_slice(lines: &[&str], span: Option<(u32, u32)>) -> Option<String> {
    let (s, e) = span?;
    if lines.is_empty() {
        return None;
    }
    let s = (s.saturating_sub(1) as usize).min(lines.len());
    let e = (e as usize).min(lines.len()).max(s);
    Some(lines[s..e].join("\n"))
}

/// Whitespace-insensitive body key: relocated code matches exactly.
fn body_key(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Body key without the signature line: catches rename-in-move (the name
/// lives on line one). Empty for single-line symbols — those never match.
fn nosig_key(body: &str) -> String {
    let mut lines = body.lines();
    lines.next();
    let rest: String = lines.collect();
    let key: String = rest.chars().filter(|c| !c.is_whitespace()).collect();
    key
}

fn kind_key(kind: SymbolKind) -> String {
    format!("{kind:?}")
}

/// Link cross-file symbol moves and intra-file reorders.
///
/// Consumes Added/Removed pairs into Moved entries (same-file exact-body
/// pairs become Renamed instead). Output is sorted by (file, name) so
/// `--json` is stable across runs. Runs after per-file collection, before
/// grouping; the UI worker applies the same step.
pub fn link_moves(files: &[FileChange], syms: Vec<SymbolChange>) -> Vec<SymbolChange> {
    link_moves_with_cache(files, syms, &mut SymbolCache::new())
}

/// Cached variant: the intra-file reorder re-parse hits the shared cache.
pub fn link_moves_with_cache(
    files: &[FileChange],
    syms: Vec<SymbolChange>,
    cache: &mut SymbolCache,
) -> Vec<SymbolChange> {
    let by_path: HashMap<&str, &FileChange> = files.iter().map(|f| (f.display_path(), f)).collect();

    struct Cand {
        idx: usize,
        key: String,
        nosig: String,
        body: String,
    }
    let mut removed: Vec<Cand> = Vec::new();
    let mut added: Vec<Cand> = Vec::new();
    // Pre-split line tables per file: every candidate slices the same text.
    let mut tables: HashMap<&str, (Vec<&str>, Vec<&str>)> = HashMap::new();
    for (idx, s) in syms.iter().enumerate() {
        let Some(f) = by_path.get(s.file.as_str()) else {
            continue;
        };
        let entry = tables.entry(s.file.as_str()).or_insert_with(|| {
            (
                f.old_content
                    .as_deref()
                    .map(|c| c.lines().collect())
                    .unwrap_or_default(),
                f.new_content
                    .as_deref()
                    .map(|c| c.lines().collect())
                    .unwrap_or_default(),
            )
        });
        match s.change {
            SymbolChangeKind::Removed => {
                if let Some(body) = body_slice(&entry.0, s.old_lines) {
                    removed.push(Cand {
                        idx,
                        key: body_key(&body),
                        nosig: nosig_key(&body),
                        body,
                    });
                }
            }
            SymbolChangeKind::Added => {
                if let Some(body) = body_slice(&entry.1, s.new_lines) {
                    added.push(Cand {
                        idx,
                        key: body_key(&body),
                        nosig: nosig_key(&body),
                        body,
                    });
                }
            }
            _ => {}
        }
    }
    // Deterministic pairing order.
    removed.sort_by(|a, b| {
        (syms[a.idx].file.clone(), syms[a.idx].name.clone())
            .cmp(&(syms[b.idx].file.clone(), syms[b.idx].name.clone()))
    });
    added.sort_by(|a, b| {
        (syms[a.idx].file.clone(), syms[a.idx].name.clone())
            .cmp(&(syms[b.idx].file.clone(), syms[b.idx].name.clone()))
    });

    let mut consumed = vec![false; syms.len()];
    let mut linked: Vec<SymbolChange> = Vec::new();

    // Pass 1: exact body matches. Pair only unambiguous keys (one removed
    // × one added) so duplicated boilerplate never links wrongly.
    let mut r_groups: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (pos, c) in removed.iter().enumerate() {
        r_groups
            .entry((kind_key(syms[c.idx].kind), c.key.clone()))
            .or_default()
            .push(pos);
    }
    let mut a_groups: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (pos, c) in added.iter().enumerate() {
        a_groups
            .entry((kind_key(syms[c.idx].kind), c.key.clone()))
            .or_default()
            .push(pos);
    }
    for (kk, rs) in &r_groups {
        if rs.len() != 1 {
            continue;
        }
        let Some(aa) = a_groups.get(kk) else { continue };
        if aa.len() != 1 {
            continue;
        }
        let r = &syms[removed[rs[0]].idx];
        let a = &syms[added[aa[0]].idx];
        consumed[removed[rs[0]].idx] = true;
        consumed[added[aa[0]].idx] = true;
        linked.push(link_exact_pair(r, a));
    }

    // Pass 1b: same body modulo the signature line = rename-in-move.
    // Same 1×1 rule; empty keys (single-line symbols) never match.
    let mut r_nosig: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (pos, c) in removed.iter().enumerate() {
        if consumed[c.idx] || c.nosig.is_empty() {
            continue;
        }
        r_nosig
            .entry((kind_key(syms[c.idx].kind), c.nosig.clone()))
            .or_default()
            .push(pos);
    }
    let mut a_nosig: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (pos, c) in added.iter().enumerate() {
        if consumed[c.idx] || c.nosig.is_empty() {
            continue;
        }
        a_nosig
            .entry((kind_key(syms[c.idx].kind), c.nosig.clone()))
            .or_default()
            .push(pos);
    }
    for (kk, rs) in &r_nosig {
        if rs.len() != 1 {
            continue;
        }
        let Some(aa) = a_nosig.get(kk) else { continue };
        if aa.len() != 1 {
            continue;
        }
        let ri = removed[rs[0]].idx;
        let ai = added[aa[0]].idx;
        if consumed[ri] || consumed[ai] {
            continue;
        }
        consumed[ri] = true;
        consumed[ai] = true;
        linked.push(link_exact_pair(&syms[ri], &syms[ai]));
    }

    // Pass 2: same name + similar body in different files = moved with edits.
    // Only unambiguous (kind, name) pairs — duplicates stay unlinked.
    let mut name_counts: std::collections::BTreeMap<(String, String), (usize, usize)> =
        std::collections::BTreeMap::new();
    for c in &removed {
        if !consumed[c.idx] {
            name_counts
                .entry((kind_key(syms[c.idx].kind), syms[c.idx].name.clone()))
                .or_default()
                .0 += 1;
        }
    }
    for c in &added {
        if !consumed[c.idx] {
            name_counts
                .entry((kind_key(syms[c.idx].kind), syms[c.idx].name.clone()))
                .or_default()
                .1 += 1;
        }
    }
    for rc in &removed {
        if consumed[rc.idx] {
            continue;
        }
        let r = &syms[rc.idx];
        if name_counts.get(&(kind_key(r.kind), r.name.clone())) != Some(&(1, 1)) {
            continue;
        }
        for ac in &added {
            if consumed[ac.idx] {
                continue;
            }
            let a = &syms[ac.idx];
            if a.file == r.file || a.kind != r.kind || a.name != r.name {
                continue;
            }
            // Similarity on huge bodies is quadratic text work for a 0.70
            // guess: exact matching already had its chance in passes 1/1b.
            if rc.body.len() + ac.body.len() > 128 * 1024 {
                continue;
            }
            let ratio = similar::TextDiff::from_lines(&rc.body, &ac.body).ratio();
            if ratio >= 0.80 {
                consumed[rc.idx] = true;
                consumed[ac.idx] = true;
                let pct = (ratio * 100.0).round() as u32;
                linked.push(SymbolChange {
                    file: a.file.clone(),
                    name: a.name.clone(),
                    kind: a.kind,
                    change: SymbolChangeKind::Moved,
                    confidence: 0.70,
                    old_signature: r.old_signature.clone(),
                    new_signature: a.new_signature.clone(),
                    old_lines: r.old_lines,
                    new_lines: a.new_lines,
                    evidence: vec![
                        format!("{MOVED_FROM_PREFIX}{}", r.file),
                        format!("body {pct}% similar — moved with edits, verify the diff"),
                    ],
                });
                break;
            }
        }
    }

    // Pass 3: intra-file reorder. Only when the symbol multiset is unchanged
    // (insertions/deletions already explain order shifts) but the sequence
    // differs — then displaced symbols genuinely moved.
    for f in files {
        // Files that never produced symbols (generated, binary, oversize)
        // are skipped: re-parsing them here only buys noise, and minified
        // bundles are the slowest parses in the tree.
        if skip_parse(f) {
            continue;
        }
        let path = f.display_path();
        let (Some(old_c), Some(new_c)) = (f.old_content.as_deref(), f.new_content.as_deref())
        else {
            continue;
        };
        let old_syms = extract_cached(path, f.language, old_c, cache);
        let new_syms = extract_cached(path, f.language, new_c, cache);
        let old_keys: Vec<String> = old_syms
            .iter()
            .map(|s| format!("{:?}::{}", s.kind, s.name))
            .collect();
        let new_keys: Vec<String> = new_syms
            .iter()
            .map(|s| format!("{:?}::{}", s.kind, s.name))
            .collect();
        if old_keys.len() != new_keys.len() {
            continue;
        }
        let mut sorted_old = old_keys.clone();
        sorted_old.sort();
        let mut sorted_new = new_keys.clone();
        sorted_new.sort();
        if sorted_old != sorted_new || old_keys == new_keys {
            continue;
        }
        let mut positions: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, k) in old_keys.iter().enumerate() {
            positions.entry(k.as_str()).or_default().push(i);
        }
        let mut taken: HashMap<&str, usize> = HashMap::new();
        for (j, k) in new_keys.iter().enumerate() {
            let n = taken.entry(k.as_str()).or_insert(0);
            let i = positions[k.as_str()][*n];
            *n += 1;
            if i != j {
                let os = &old_syms[i];
                let ns = &new_syms[j];
                linked.push(SymbolChange {
                    file: path.to_string(),
                    name: ns.name.clone(),
                    kind: ns.kind,
                    change: SymbolChangeKind::Moved,
                    confidence: 0.85,
                    old_signature: Some(os.signature.clone()),
                    new_signature: Some(ns.signature.clone()),
                    old_lines: Some((os.start_line, os.end_line)),
                    new_lines: Some((ns.start_line, ns.end_line)),
                    evidence: vec![format!(
                        "reordered within {path}: position {} → {}",
                        i + 1,
                        j + 1
                    )],
                });
            }
        }
    }

    let mut out: Vec<SymbolChange> = syms
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !consumed[*i])
        .map(|(_, s)| s)
        .chain(linked)
        .collect();
    out.sort_by(|a, b| {
        (&a.file, &a.name, kind_key(a.kind)).cmp(&(&b.file, &b.name, kind_key(b.kind)))
    });
    out
}

/// Build the Moved (or same-file Renamed) entry for an exact-body pair.
fn link_exact_pair(r: &SymbolChange, a: &SymbolChange) -> SymbolChange {
    let renamed = r.name != a.name;
    if r.file == a.file {
        return SymbolChange {
            file: a.file.clone(),
            name: format!("{} → {}", r.name, a.name),
            kind: a.kind,
            change: SymbolChangeKind::Renamed,
            confidence: 0.85,
            old_signature: r.old_signature.clone(),
            new_signature: a.new_signature.clone(),
            old_lines: r.old_lines,
            new_lines: a.new_lines,
            evidence: vec!["identical body, different name — treated as rename".to_string()],
        };
    }
    let mut evidence = vec![format!("{MOVED_FROM_PREFIX}{}", r.file)];
    if renamed {
        evidence.push(format!("renamed {} → {} in the move", r.name, a.name));
    } else {
        evidence.push("body identical — pure relocation".to_string());
    }
    SymbolChange {
        file: a.file.clone(),
        name: if renamed {
            format!("{} → {}", r.name, a.name)
        } else {
            a.name.clone()
        },
        kind: a.kind,
        change: SymbolChangeKind::Moved,
        // A rename inside the move is an edit, not a pure relocation:
        // lower confidence keeps it out of the Low "pure move" bucket.
        confidence: if renamed { 0.70 } else { 0.95 },
        old_signature: r.old_signature.clone(),
        new_signature: a.new_signature.clone(),
        old_lines: r.old_lines,
        new_lines: a.new_lines,
        evidence,
    }
}

// ---------------------------------------------------------------------------
// Test coverage linking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TestInfo {
    name: String,
    file: String,
    calls: Vec<String>,
}

/// One non-diff source file: what it defines, calls, and imports.
#[derive(Debug, Clone)]
struct SourceRef {
    path: String,
    symbols: Vec<String>,
    calls: Vec<String>,
    imports: Vec<String>,
}

/// Bounded worktree context shared by coverage and blast-radius passes.
#[derive(Debug, Default)]
pub struct RepoContext {
    tests: Vec<TestInfo>,
    sources: Vec<SourceRef>,
}

/// Bounded worktree context for coverage and blast-radius passes.
/// Deterministic (sorted traversal); skips files already in the diff.
/// Resolves against the worktree even when reviewing old commits.
pub fn collect_context(root: &str, files: &[FileChange]) -> RepoContext {
    collect_context_with_cache(root, files, &mut SymbolCache::new())
}

/// Cached variant: worktree scans hit the shared cache across runs.
pub fn collect_context_with_cache(
    root: &str,
    files: &[FileChange],
    cache: &mut SymbolCache,
) -> RepoContext {
    const MAX_SCAN_FILES: usize = 200;
    const MAX_SCAN_SOURCES: usize = 400;
    const MAX_SCAN_BYTES: u64 = 256 * 1024;
    const SKIP_DIRS: [&str; 9] = [
        ".git",
        "node_modules",
        "target",
        "dist",
        "build",
        "out",
        "vendor",
        "__pycache__",
        ".venv",
    ];
    let mut ctx = RepoContext::default();
    if root.is_empty() {
        return ctx;
    }
    let root_path = std::path::Path::new(root);
    if !root_path.is_dir() {
        return ctx;
    }
    let in_diff: std::collections::HashSet<&str> = files.iter().map(|f| f.display_path()).collect();
    let mut stack = vec![root_path.to_path_buf()];
    let mut scanned_tests = 0;
    let mut scanned_sources = 0;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(p);
                }
                continue;
            }
            let rel = p
                .strip_prefix(root_path)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if rel.is_empty() || in_diff.contains(rel.as_str()) {
                continue;
            }
            let is_test = rift_git::is_test_path(&rel);
            if is_test && scanned_tests >= MAX_SCAN_FILES {
                continue;
            }
            if !is_test && scanned_sources >= MAX_SCAN_SOURCES {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.len() > MAX_SCAN_BYTES {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            let lang = Language::from_path(&rel);
            if is_test {
                scanned_tests += 1;
                let lines: Vec<&str> = content.lines().collect();
                for s in extract_cached(&rel, lang, &content, cache) {
                    if !s.is_test {
                        continue;
                    }
                    let body =
                        body_slice(&lines, Some((s.start_line, s.end_line))).unwrap_or_default();
                    ctx.tests.push(TestInfo {
                        name: s.name.clone(),
                        file: rel.clone(),
                        calls: called_names(&body),
                    });
                }
            } else {
                scanned_sources += 1;
                let table = extract_cached(&rel, lang, &content, cache);
                ctx.sources.push(SourceRef {
                    path: rel.clone(),
                    symbols: table.iter().map(|s| s.name.clone()).collect(),
                    calls: called_names(&content),
                    imports: imported_names(lang, &content),
                });
            }
        }
    }
    ctx
}

/// Link changed symbols to the tests that exercise them.
///
/// Every test symbol in the new tree contributes its call sites; a changed
/// symbol is covered when a test calls it by name or targets it by naming
/// convention. Covered file items gain `covering-tests` evidence;
/// behavior-grade items with no coverage gain an `untested-change` flag.
/// Runs after grouping; the UI worker applies the same step.
pub fn apply_test_coverage(
    files: &[FileChange],
    syms: &[SymbolChange],
    items: &mut [ReviewItem],
    ctx: &RepoContext,
) {
    apply_test_coverage_with_cache(files, syms, items, ctx, &mut SymbolCache::new())
}

/// Owner qualification stripped: `Auth::login` → `login`.
/// Call sites and import edges name the short form.
fn short_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Cached variant: changed-file test scans hit the shared cache.
pub fn apply_test_coverage_with_cache(
    files: &[FileChange],
    syms: &[SymbolChange],
    items: &mut [ReviewItem],
    ctx: &RepoContext,
    cache: &mut SymbolCache,
) {
    let mut tests: Vec<TestInfo> = Vec::new();
    for f in files {
        // Test bodies are scanned for call sites directly (not via the
        // symbol table), so oversize files still contribute recall here;
        // only binary/generated files are skipped outright.
        if f.is_binary || f.is_generated {
            continue;
        }
        let path = f.display_path();
        let Some(content) = f.new_content.as_deref() else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for s in extract_cached(path, f.language, content, cache) {
            if !(s.is_test || f.is_test_file) {
                continue;
            }
            let body = body_slice(&lines, Some((s.start_line, s.end_line))).unwrap_or_default();
            tests.push(TestInfo {
                name: s.name.clone(),
                file: path.to_string(),
                calls: called_names(&body),
            });
        }
    }
    // Unchanged worktree test files still cover changed symbols.
    tests.extend(ctx.tests.iter().cloned());
    if tests.is_empty() {
        return;
    }
    let test_names: Vec<&str> = tests.iter().map(|t| t.name.as_str()).collect();
    let test_file: HashMap<&str, bool> = files
        .iter()
        .map(|f| (f.display_path(), f.is_test_file))
        .collect();

    // (file, symbol) -> covering "name (file)" labels.
    // Symbols carry owner qualification (`Auth::login`) but call sites
    // name the short form (`login`), so matching tries both. Short-name
    // matches can over-attribute across same-named methods; that is the
    // documented price of recall here (evidence, not proof).
    let mut covering: HashMap<(String, String), Vec<String>> = HashMap::new();
    for s in syms {
        if matches!(s.kind, SymbolKind::Module) {
            continue;
        }
        // Renames read "old → new": coverage follows the new name.
        let name = s.name.rsplit(" → ").next().unwrap_or(&s.name);
        let short = short_name(name);
        if test_file.get(s.file.as_str()).copied().unwrap_or(false)
            || test_names.iter().any(|t| *t == name || *t == short)
        {
            continue; // the change itself is a test
        }
        let mut cov: Vec<String> = tests
            .iter()
            .filter(|t| {
                t.calls.iter().any(|c| c == name || c == short)
                    || test_targets(&t.name, name)
                    || test_targets(&t.name, short)
            })
            .map(|t| format!("{} ({})", t.name, t.file))
            .collect();
        cov.sort();
        cov.dedup();
        if !cov.is_empty() {
            covering.insert((s.file.clone(), name.to_string()), cov);
        }
    }

    // Move items (`move:from→to`) carry symbols too: an edited move with
    // no covering tests deserves the same flag as an edited file. Pure
    // moves are Refactor, so the `untested-change` gate below ignores them.
    for item in items
        .iter_mut()
        .filter(|i| i.id.starts_with("file:") || i.id.starts_with("move:"))
    {
        let paths = item.files.clone();
        let Some(path) = paths.first() else {
            continue;
        };
        let mut cov: Vec<String> = Vec::new();
        for sym_name in &item.symbols {
            // Renames read "old → new": coverage follows the new name.
            let name = sym_name.rsplit(" → ").next().unwrap_or(sym_name);
            for p in &paths {
                if let Some(c) = covering.get(&(p.clone(), name.to_string())) {
                    cov.extend(c.iter().cloned());
                }
            }
        }
        cov.sort();
        cov.dedup();
        if cov.is_empty() {
            if matches!(
                item.category,
                Category::Behavior
                    | Category::Security
                    | Category::ApiBreak
                    | Category::Auth
                    | Category::Schema
                    | Category::Migration
            ) {
                item.evidence.push(Evidence::new(
                    "untested-change",
                    "no test references these symbols",
                    path,
                ));
                item.why
                    .push_str(" No covering tests reference these symbols.");
            }
        } else {
            item.evidence.push(Evidence::new(
                "covering-tests",
                &format!("covered by {}", cov.join(", ")),
                path,
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Blast radius
// ---------------------------------------------------------------------------

/// Flag the non-test dependents of each changed symbol.
///
/// A reference counts when the caller imports the name (dependency edge) or
/// the name is defined exactly once repo-wide (unambiguous call). Test files
/// are excluded — they are reported by coverage instead. Runs after
/// grouping; the UI worker applies the same step.
pub fn apply_blast_radius(files: &[FileChange], items: &mut [ReviewItem], ctx: &RepoContext) {
    apply_blast_radius_with_cache(files, items, ctx, &mut SymbolCache::new())
}

/// Cached variant: changed-file definers hit the shared cache.
pub fn apply_blast_radius_with_cache(
    files: &[FileChange],
    items: &mut [ReviewItem],
    ctx: &RepoContext,
    cache: &mut SymbolCache,
) {
    // name -> defining files (worktree sources + changed files' new tree).
    // Generated files are not definers: bundles duplicate src and would
    // only dilute uniqueness (and repay the slowest parses).
    let mut defn: HashMap<String, Vec<String>> = HashMap::new();
    for s in &ctx.sources {
        for n in &s.symbols {
            defn.entry(n.clone()).or_default().push(s.path.clone());
        }
    }
    for f in files {
        if f.is_binary || f.is_generated {
            continue;
        }
        let path = f.display_path();
        // Both sides: a deleted symbol is defined only by old content —
        // without it, callers of removed code never resolve. (Cache hits
        // make the second parse cheap: contents were parsed for symbols
        // already.) Same-file duplicates collapse in the sort+dedup below.
        for content in [f.new_content.as_deref(), f.old_content.as_deref()]
            .into_iter()
            .flatten()
        {
            for s in extract_cached(path, f.language, content, cache) {
                defn.entry(s.name.clone())
                    .or_default()
                    .push(path.to_string());
            }
        }
    }
    for v in defn.values_mut() {
        v.sort();
        v.dedup();
    }
    // Names that are themselves tests are not blast subjects.
    let test_names: std::collections::HashSet<&str> =
        ctx.tests.iter().map(|t| t.name.as_str()).collect();

    for item in items
        .iter_mut()
        .filter(|i| i.id.starts_with("file:") || i.id.starts_with("move:"))
    {
        let paths = item.files.clone();
        let Some(path) = paths.first().cloned() else {
            continue;
        };
        let mut affected: Vec<String> = Vec::new();
        for sym_name in &item.symbols {
            // Renames read "old → new": dependents follow the new name.
            let name = sym_name.rsplit(" → ").next().unwrap_or(sym_name);
            let short = short_name(name);
            if test_names.contains(name) {
                continue;
            }
            let unique = defn.get(name).map(|v| v.len() == 1).unwrap_or(false);
            for src in &ctx.sources {
                if paths.contains(&src.path) || rift_git::is_test_path(&src.path) {
                    continue;
                }
                if !src.calls.iter().any(|c| c == name || c == short) {
                    continue;
                }
                if src.imports.iter().any(|i| i == name || i == short) || unique {
                    affected.push(src.path.clone());
                }
            }
        }
        affected.sort();
        affected.dedup();
        if affected.is_empty() {
            continue;
        }
        let shown: Vec<&str> = affected.iter().take(8).map(|s| s.as_str()).collect();
        let mut summary = format!(
            "referenced by {} file{}: {}",
            affected.len(),
            if affected.len() == 1 { "" } else { "s" },
            shown.join(", ")
        );
        if affected.len() > shown.len() {
            summary.push_str(&format!(" (and {} more)", affected.len() - shown.len()));
        }
        item.evidence
            .push(Evidence::new("blast-radius", &summary, &path));
    }
}

pub fn fill_stats(cs: &mut ChangeSet) {
    cs.stats = compute_stats(&cs.files, &cs.symbol_changes, &cs.review_items);
}

/// Stats without needing a full ChangeSet (used by the UI worker).
pub fn compute_stats(
    files: &[FileChange],
    syms: &[SymbolChange],
    items: &[ReviewItem],
) -> rift_core::ChangeStats {
    let mut s = rift_core::ChangeStats {
        files_changed: files.len(),
        ..Default::default()
    };
    for f in files {
        s.added_lines += f.added_lines;
        s.deleted_lines += f.deleted_lines;
        if f.is_generated || f.looks_formatting_only() {
            s.mechanical_files += 1;
        }
        if f.is_test_file {
            s.test_files += 1;
        }
    }
    for sc in syms {
        match sc.change {
            SymbolChangeKind::Added => s.symbols_added += 1,
            SymbolChangeKind::Removed => s.symbols_removed += 1,
            _ => s.symbols_modified += 1,
        }
    }
    s.meaningful_changes = items
        .iter()
        .filter(|r| !matches!(r.category, Category::Mechanical))
        .count();
    s
}

// ---------------------------------------------------------------------------
// Generated / mechanical detection
// ---------------------------------------------------------------------------

fn generated_by_name(path: &str) -> bool {
    let l = path.to_lowercase();
    // Lockfiles & vendored output (match full filename or any path segment,
    // so root-level `target/` and nested dirs both hit).
    for pat in [
        "cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "pipfile.lock",
        ".min.js",
        ".min.css",
        ".map",
        ".bundle.js",
    ] {
        if l.ends_with(pat) {
            return true;
        }
    }
    let segs: Vec<&str> = l.split('/').collect();
    for dir in [
        "target",
        "node_modules",
        "dist",
        "build",
        ".next",
        "vendor",
        "__pycache__",
        ".git",
    ] {
        if segs.contains(&dir) {
            return true;
        }
    }
    for ext in [".pb.go", ".generated.ts", ".g.cs", ".designer.cs", ".lock"] {
        if l.ends_with(ext) {
            return true;
        }
    }
    false
}

fn generated_by_content(f: &FileChange) -> bool {
    // Either side: a file that *becomes* generated is mechanical too.
    for src in [f.old_content.as_deref(), f.new_content.as_deref()]
        .into_iter()
        .flatten()
    {
        // Check first 5 lines only — cheap.
        for line in src.lines().take(5) {
            let ll = line.to_lowercase();
            if ll.contains("auto-generated")
                || ll.contains("autogenerated")
                || ll.contains("@generated")
                || (ll.contains("do not edit") && ll.contains("generat"))
            {
                return true;
            }
        }
    }
    false
}

pub fn mark_generated(files: &mut [FileChange]) {
    for f in files.iter_mut() {
        let p = f.display_path().to_string();
        f.is_generated = generated_by_name(&p) || generated_by_content(f);
    }
}

// ---------------------------------------------------------------------------
// Importance scoring
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Score {
    priority: u8,
    severity: Severity,
    category: Category,
    reasons: Vec<Evidence>,
}

fn score_file(f: &FileChange, syms: &[&SymbolChange]) -> Score {
    let path = f.display_path().to_lowercase();
    let mut points: i32 = 20;
    let mut severity = Severity::Low;
    let mut category = Category::Unknown;
    let mut reasons = Vec::new();
    let mut ev = |kind: &str, summary: &str| {
        reasons.push(Evidence::new(kind, summary, f.display_path()));
    };

    if f.is_generated {
        return Score {
            priority: 2,
            severity: Severity::Low,
            category: Category::Mechanical,
            reasons: vec![Evidence::new(
                "generated",
                "generated or lockfile output — collapsed by default",
                f.display_path(),
            )],
        };
    }
    if f.looks_formatting_only() {
        return Score {
            priority: 3,
            severity: Severity::Low,
            category: Category::Mechanical,
            reasons: vec![Evidence::new(
                "formatting-only",
                "whitespace/punctuation-only diff",
                f.display_path(),
            )],
        };
    }

    // Auth / security surface. Whole-segment matching: `author.rs` is
    // not auth surface, but `auth.rs`, `session.rs`, `user-auth.rs` are.
    // (Hyphenated compounds like `sign-in` still substring-match since
    // segments split on `-`.)
    let segments: Vec<&str> = path.split(['/', '.', '_', '-']).collect();
    let seg_has = |kw: &str| {
        if kw.contains('-') || kw.contains('/') {
            path.contains(kw)
        } else {
            segments.contains(&kw)
        }
    };
    let auth_hit = [
        "auth",
        "session",
        "token",
        "password",
        "login",
        "oauth",
        "permission",
        "secret",
        "crypto",
        "signin",
        "sign-in",
    ]
    .iter()
    .any(|k| seg_has(k));
    if auth_hit {
        points += 45;
        severity = severity.max(Severity::High);
        category = Category::Auth;
        ev(
            "auth-surface",
            "change touches authentication/authorization surface",
        );
    }
    // Schema / migration.
    let schema_hit = path.contains("migrat")
        || path.contains("schema")
        || path.ends_with(".sql")
        || path.contains("prisma/")
        || path.contains("alembic/");
    if schema_hit {
        points += 40;
        severity = severity.max(Severity::High);
        if category == Category::Unknown {
            category = Category::Schema;
        }
        ev("schema", "database schema or migration change");
    }
    // Dependency manifests.
    let dep_hit = path.ends_with("cargo.toml")
        || path.ends_with("package.json")
        || path.ends_with("pyproject.toml")
        || path.ends_with("requirements.txt")
        || path.ends_with(".csproj")
        || path.ends_with("go.mod");
    if dep_hit {
        points += 30;
        severity = severity.max(Severity::Medium);
        if category == Category::Unknown {
            category = Category::Dependency;
        }
        ev("dependency-manifest", "dependency manifest changed");
    }
    // Deploy / config.
    let cfg_hit = path.contains("dockerfile")
        || path.contains("k8s/")
        || path.contains("deploy/")
        || path.contains(".github/workflows/")
        || path.ends_with(".env")
        || path.contains("appsettings");
    if cfg_hit {
        points += 25;
        severity = severity.max(Severity::Medium);
        if category == Category::Unknown {
            category = Category::Config;
        }
        ev(
            "deploy-config",
            "deployment or runtime configuration changed",
        );
    }
    // Test files: informative but low priority (unless existing tests modified).
    // Test/docs win over path-keyword categories: `tests/auth_test.rs`
    // is a test change that happens to touch auth words, not an auth
    // change. Severity/evidence from keyword hits are kept.
    if f.is_test_file {
        points -= 10;
        category = Category::Test;
        ev(
            "test-file",
            "test file — verifies behavior, rarely the risk itself",
        );
    }
    // Docs-only.
    if path.ends_with(".md") || path.ends_with(".rst") || path.contains("/docs/") {
        points -= 12;
        category = Category::Docs;
        ev("docs", "documentation change");
    }
    // Symbol-level signals. Per-symbol bonuses are capped in aggregate so a
    // brand-new file with 30 added symbols doesn't outrank an auth change.
    let mut symbol_bonus: i32 = 0;
    for s in syms {
        match s.change {
            SymbolChangeKind::Removed => {
                symbol_bonus += 25;
                severity = severity.max(Severity::High);
                if category == Category::Unknown {
                    category = Category::ApiBreak;
                }
                ev("symbol-removed", &format!("symbol removed: {}", s.name));
            }
            SymbolChangeKind::SignatureChanged => {
                symbol_bonus += 22;
                severity = severity.max(Severity::Medium);
                if category == Category::Unknown {
                    category = Category::Behavior;
                }
                ev(
                    "signature-changed",
                    &format!("signature changed: {}", s.name),
                );
            }
            SymbolChangeKind::VisibilityChanged => {
                symbol_bonus += 20;
                severity = severity.max(Severity::Medium);
                if category == Category::Unknown {
                    category = Category::ApiBreak;
                }
                ev(
                    "visibility-changed",
                    &format!("visibility changed: {}", s.name),
                );
            }
            SymbolChangeKind::Renamed | SymbolChangeKind::Moved => {
                symbol_bonus += 5;
                ev("rename-move", &format!("renamed/moved: {}", s.name));
            }
            SymbolChangeKind::Modified => {
                symbol_bonus += 12;
                if category == Category::Unknown {
                    category = Category::Behavior;
                }
            }
            SymbolChangeKind::Added => {
                symbol_bonus += 8;
                if category == Category::Unknown {
                    category = Category::Behavior;
                }
            }
            SymbolChangeKind::FormattingOnly => {
                points -= 5;
            }
        }
    }
    points += symbol_bonus.clamp(0, 25);
    // Content signals on added/deleted lines, with string literals and
    // line comments stripped: mentioning `valid` in a comment is not
    // validation logic (same hygiene as the `unsafe` check below).
    // NOTE: `==`, not `matches!` — a variable in a `matches!` pattern
    // binds instead of comparing and would mix additions into deletions.
    let code_text = |kind: rift_core::DiffLineKind| {
        f.hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| l.kind == kind)
            .map(|l| strip_string_literals(&l.text).to_lowercase())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let added_text = code_text(rift_core::DiffLineKind::Addition);
    let deleted_text = code_text(rift_core::DiffLineKind::Deletion);
    let has_unsafe = f
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| matches!(l.kind, rift_core::DiffLineKind::Addition))
        .map(|l| strip_string_literals(&l.text).trim().to_lowercase())
        .any(|t| {
            t.starts_with("unsafe")
                || t.contains("unsafe fn")
                || t.contains("unsafe {")
                || t.contains("unsafe impl")
                || t.contains("unsafe trait")
                || t.contains("unsafe extern")
        });
    if has_unsafe && f.language == rift_core::Language::Rust {
        points += 20;
        severity = severity.max(Severity::High);
        if category == Category::Unknown {
            category = Category::Security;
        }
        ev("unsafe", "new `unsafe` code");
    }
    if deleted_text.contains("valid") && !added_text.contains("valid") {
        points += 18;
        severity = severity.max(Severity::High);
        if category == Category::Unknown {
            category = Category::Security;
        }
        ev(
            "validation-removed",
            "deleted lines mention validation with no replacement",
        );
    }
    if (deleted_text.contains("if ") || deleted_text.contains("match "))
        && (added_text.contains("if ") || added_text.contains("match "))
    {
        points += 10;
        ev("conditional-changed", "branch conditions modified");
    }
    // Whole-segment path match: `keyboard.rs` and `monkey.rs` are not
    // secret handling (`key` as a substring over-matches).
    if (seg_has("secret") || seg_has("credential") || seg_has("key"))
        && (added_text.contains("secret")
            || added_text.contains("token")
            || added_text.contains("password"))
    {
        points += 25;
        severity = severity.max(Severity::Critical);
        category = Category::Security;
        ev("secret-handling", "secret/credential handling touched");
    }

    let priority = points.clamp(0, 100) as u8;
    if category == Category::Unknown {
        category = if priority >= 55 {
            Category::Behavior
        } else if f.is_test_file {
            Category::Test
        } else {
            Category::Refactor
        };
    }
    if priority >= 80 {
        severity = severity.max(Severity::Critical);
    } else if priority >= 55 {
        severity = severity.max(Severity::Medium);
    }
    Score {
        priority,
        severity,
        category,
        reasons,
    }
}

// ---------------------------------------------------------------------------
// Grouping into review items
// ---------------------------------------------------------------------------

pub fn group_items(files: &[FileChange], syms: &[SymbolChange]) -> Vec<ReviewItem> {
    let mut items = Vec::new();
    let sym_by_file: HashMap<&str, Vec<&SymbolChange>> = {
        let mut m: HashMap<&str, Vec<&SymbolChange>> = HashMap::new();
        for s in syms {
            m.entry(s.file.as_str()).or_default().push(s);
        }
        m
    };

    // 1. Mechanical bucket.
    let mech: Vec<&FileChange> = files
        .iter()
        .filter(|f| f.is_generated || f.looks_formatting_only())
        .collect();
    if !mech.is_empty() {
        let total: usize = mech.iter().map(|f| f.added_lines + f.deleted_lines).sum();
        items.push(ReviewItem {
            id: "mechanical".to_string(),
            title: format!(
                "Generated / mechanical changes — {} file{} collapsed",
                mech.len(),
                if mech.len() == 1 { "" } else { "s" }
            ),
            category: Category::Mechanical,
            severity: Severity::Low,
            priority: 2,
            confidence: 0.9,
            files: mech.iter().map(|f| f.display_path().to_string()).collect(),
            symbols: vec![],
            evidence: vec![Evidence::new(
                "mechanical-bucket",
                &format!("{total} changed lines across generated/formatting-only files"),
                "",
            )],
            why: "These files match generated-output or whitespace-only heuristics. Expand to verify, but they rarely need line-by-line review.".to_string(),
        });
    }

    // 2. Test bucket (added test files with no other signal).
    let test_added: Vec<&FileChange> = files
        .iter()
        .filter(|f| {
            f.is_test_file
                && !f.is_generated
                && !f.looks_formatting_only()
                && matches!(
                    f.status,
                    FileStatus::Added | FileStatus::Untracked | FileStatus::Modified
                )
        })
        .collect();
    // Only bucket additive test changes; removed/broken tests get per-file items.
    // Files with zero symbol changes are excluded: `all()` is vacuously
    // true on the empty set, which previously bucketed empty/unparseable
    // test files as "0 added".
    let pure_test_adds: Vec<&FileChange> = test_added
        .iter()
        .filter(|f| {
            let mut has_any = false;
            syms.iter().filter(|s| s.file == f.display_path()).all(|s| {
                has_any = true;
                matches!(
                    s.change,
                    SymbolChangeKind::Added | SymbolChangeKind::Modified
                )
            }) && has_any
        })
        .copied()
        .collect();
    if !pure_test_adds.is_empty() {
        let n_added: usize = pure_test_adds
            .iter()
            .map(|f| {
                syms.iter()
                    .filter(|s| {
                        s.file == f.display_path() && matches!(s.change, SymbolChangeKind::Added)
                    })
                    .count()
            })
            .sum();
        let modified: Vec<(&str, &str)> = pure_test_adds
            .iter()
            .flat_map(|f| {
                syms.iter()
                    .filter(|s| {
                        s.file == f.display_path() && matches!(s.change, SymbolChangeKind::Modified)
                    })
                    .map(|s| (s.name.as_str(), s.file.as_str()))
            })
            .collect();
        let mut title = if n_added > 0 {
            format!(
                "Tests — {n_added} new test symbol{}",
                if n_added == 1 { "" } else { "s" }
            )
        } else {
            "Tests".to_string()
        };
        if !modified.is_empty() {
            title.push_str(&format!(", {} modified", modified.len()));
        }
        title.push_str(&format!(
            " across {} file{}",
            pure_test_adds.len(),
            if pure_test_adds.len() == 1 { "" } else { "s" }
        ));
        let mut evidence = vec![Evidence::new(
            "test-additions",
            &format!("{n_added} added test symbols"),
            "",
        )];
        for (name, file) in modified.iter().take(10) {
            evidence.push(Evidence::new("test-modified", name, file));
        }
        items.push(ReviewItem {
            id: "tests".to_string(),
            title,
            category: Category::Test,
            severity: Severity::Low,
            priority: 15,
            confidence: 0.85,
            files: pure_test_adds
                .iter()
                .map(|f| f.display_path().to_string())
                .collect(),
            symbols: vec![],
            evidence,
            why: "New tests document intended behavior. Modified tests changed what they assert — check the diffs if their targets are in the queue above.".to_string(),
        });
    }
    let bucketed_tests: Vec<&str> = pure_test_adds.iter().map(|f| f.display_path()).collect();

    // 2b. Cross-file moves: one low-priority item per (from, to) pair.
    // Reorder-within-file moves have no origin and stay with their file.
    let mut move_pairs: std::collections::BTreeMap<(String, String), Vec<&SymbolChange>> =
        std::collections::BTreeMap::new();
    for s in syms
        .iter()
        .filter(|s| matches!(s.change, SymbolChangeKind::Moved))
    {
        if let Some(origin) = move_origin(s) {
            move_pairs
                .entry((origin.to_string(), s.file.clone()))
                .or_default()
                .push(s);
        }
    }
    let mut move_covered: Vec<&str> = Vec::new();
    for ((from, to), members) in &move_pairs {
        let edited = members.iter().any(|s| s.confidence < 0.8);
        let min_conf: f32 = members.iter().map(|s| s.confidence).fold(1.0, f32::min);
        let title = if members.len() == 1 {
            format!("{} moved {} → {}", members[0].name, from, to)
        } else {
            format!("{} symbols moved {} → {}", members.len(), from, to)
        };
        items.push(ReviewItem {
            id: format!("move:{from}→{to}"),
            title,
            category: Category::Refactor,
            severity: if edited {
                Severity::Medium
            } else {
                Severity::Low
            },
            priority: if edited { 25 } else { 8 },
            confidence: min_conf,
            files: vec![from.clone(), to.clone()],
            symbols: members.iter().map(|s| s.name.clone()).collect(),
            evidence: members
                .iter()
                .map(|s| {
                    let detail = s
                        .evidence
                        .iter()
                        .skip(1)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ");
                    Evidence::new(
                        "symbol-move",
                        &format!(
                            "{} ({:?}, {:.0}% confidence): {detail}",
                            s.name,
                            s.kind,
                            s.confidence * 100.0
                        ),
                        to,
                    )
                })
                .collect(),
            why: if edited {
                "Symbols relocated across files with small body edits. The move itself is safe; review the edits flagged on each symbol.".to_string()
            } else {
                "Pure relocation — bodies identical. Safe to skim; no behavior change.".to_string()
            },
        });
        move_covered.push(from.as_str());
        move_covered.push(to.as_str());
    }

    // 3. Per-file items for everything else meaningful.
    for f in files.iter().filter(|f| {
        !f.is_generated && !f.looks_formatting_only() && !bucketed_tests.contains(&f.display_path())
    }) {
        let path = f.display_path();
        let fsyms: Vec<&SymbolChange> = sym_by_file
            .get(path)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .copied()
            // Pair-covered moves live in their move item, not here.
            .filter(|s| !(matches!(s.change, SymbolChangeKind::Moved) && move_origin(s).is_some()))
            .collect();
        // Skip test files whose changes are all bucketed (additions/modifications).
        if f.is_test_file
            && !fsyms.is_empty()
            && fsyms.iter().all(|s| {
                matches!(
                    s.change,
                    SymbolChangeKind::Added | SymbolChangeKind::Modified
                )
            })
        {
            continue;
        }
        // Files fully explained by move pairs are covered by the pair item.
        if fsyms.is_empty() && move_covered.contains(&path) {
            continue;
        }
        // Skip files with zero signal (e.g. binary with no parse).
        if fsyms.is_empty() && f.added_lines + f.deleted_lines == 0 {
            continue;
        }
        let sc = score_file(f, &fsyms);
        let title = file_title(f, &fsyms);
        let why = file_why(f, &fsyms, &sc);
        items.push(ReviewItem {
            id: format!("file:{}", path),
            title,
            category: sc.category,
            severity: sc.severity,
            priority: sc.priority,
            confidence: 0.8,
            files: vec![path.to_string()],
            symbols: fsyms.iter().map(|s| s.name.clone()).collect(),
            evidence: sc.reasons,
            why,
        });
    }

    // Sort: severity desc, then priority desc.
    items.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.priority.cmp(&a.priority))
            .then(a.title.cmp(&b.title))
    });
    items
}

fn file_title(f: &FileChange, syms: &[&SymbolChange]) -> String {
    let p = f.display_path();
    if syms.len() == 1 {
        let s = syms[0];
        let verb = match s.change {
            SymbolChangeKind::Added => "added",
            SymbolChangeKind::Removed => "removed",
            SymbolChangeKind::SignatureChanged => "signature changed",
            SymbolChangeKind::VisibilityChanged => "visibility changed",
            SymbolChangeKind::Renamed => "renamed",
            SymbolChangeKind::Moved => "moved",
            SymbolChangeKind::Modified => "modified",
            SymbolChangeKind::FormattingOnly => "reformatted",
        };
        return format!("{p} — {} {verb}", s.name);
    }
    if !syms.is_empty() {
        return format!("{p} — {} symbols changed", syms.len());
    }
    match f.status {
        FileStatus::Added | FileStatus::Untracked => format!("{p} — new file"),
        FileStatus::Deleted => format!("{p} — deleted"),
        FileStatus::Renamed => format!("{} → {} — renamed", f.old_path, f.new_path),
        _ => format!("{p} — +{} −{}", f.added_lines, f.deleted_lines),
    }
}

fn file_why(f: &FileChange, syms: &[&SymbolChange], sc: &Score) -> String {
    let _ = sc;
    if syms.is_empty() {
        return format!(
            "No parseable symbols changed in this file ({}). Review the raw diff.",
            f.display_path()
        );
    }
    let mut parts = Vec::new();
    for s in syms.iter().take(5) {
        parts.push(format!("{:?} {} ({:?})", s.kind, s.name, s.change));
    }
    if syms.len() > 5 {
        parts.push(format!("…and {} more", syms.len() - 5));
    }
    parts.join("; ")
}

/// Remove `"..."` / raw-string literal contents and `//` comments so keyword
/// heuristics don't fire on code that merely *mentions* a keyword inside a
/// string, test fixture, or comment (e.g. an evidence label like
/// "new `unsafe` code" must not count as new unsafe code).
fn strip_string_literals(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Line comment (outside any literal): drop the rest.
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == '/' {
            break;
        }
        // Raw string r"..." / r#"..."# / r##"..."##.
        if c == 'r' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == '#' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == '"' {
                let hashes = j - (i + 1);
                j += 1;
                while j < bytes.len() {
                    if bytes[j] == '"' && bytes[j + 1..].starts_with(&vec!['#'; hashes]) {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        // Ordinary string.
        if c == '"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == '\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{FileStatus, Language};

    fn fc(path: &str, added: &str, deleted: &str) -> FileChange {
        FileChange {
            old_path: path.to_string(),
            new_path: path.to_string(),
            status: FileStatus::Modified,
            language: Language::from_path(path),
            is_binary: false,
            is_generated: false,
            is_test_file: false,
            added_lines: 1,
            deleted_lines: 1,
            hunks: vec![rift_core::Hunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                lines: vec![
                    rift_core::DiffLine {
                        kind: rift_core::DiffLineKind::Deletion,
                        text: deleted.to_string(),
                    },
                    rift_core::DiffLine {
                        kind: rift_core::DiffLineKind::Addition,
                        text: added.to_string(),
                    },
                ],
            }],
            old_content: None,
            new_content: None,
        }
    }

    #[test]
    fn auth_scores_high() {
        let f = fc("src/auth/session.rs", "x", "y");
        let s = score_file(&f, &[]);
        assert!(s.priority >= 55);
        assert_eq!(s.category, Category::Auth);
    }

    #[test]
    fn author_is_not_auth_but_auth_paths_are() {
        let auth = score_file(&fc("src/auth/session.rs", "x", "y"), &[]);
        assert_eq!(auth.category, Category::Auth);
        // `author` merely contains the substring.
        let author = score_file(&fc("src/author.rs", "x", "y"), &[]);
        assert_ne!(author.category, Category::Auth, "{author:?}");
        // Test and docs files keep their own lens even on auth words.
        let mut test_fc = fc("tests/auth_test.rs", "x", "y");
        test_fc.is_test_file = true;
        let t = score_file(&test_fc, &[]);
        assert_eq!(t.category, Category::Test, "{t:?}");
        assert_eq!(t.severity, Severity::High, "severity evidence kept: {t:?}");
        let docs = score_file(&fc("docs/auth.md", "x", "y"), &[]);
        assert_eq!(docs.category, Category::Docs, "{docs:?}");
    }

    #[test]
    fn keyboard_is_not_secret_handling() {
        let f = fc("src/keyboard.rs", "let token = 1;", "let token = 0;");
        let s = score_file(&f, &[]);
        assert_ne!(s.category, Category::Security, "{s:?}");
        let f = fc("src/api_key.rs", "let token = 1;", "let token = 0;");
        let s = score_file(&f, &[]);
        assert_eq!(s.category, Category::Security, "{s:?}");
    }

    #[test]
    fn comment_mentions_are_not_content_signals() {
        // `// validate` in a deleted comment is not removed validation.
        let f = fc("src/a.rs", "ok();", "// validate input");
        let s = score_file(&f, &[]);
        assert!(
            !s.reasons.iter().any(|e| e.kind == "validation-removed"),
            "{s:?}"
        );
    }

    #[test]
    fn unsafe_and_visibility_get_security_api_categories() {
        let f = fc("src/a.rs", "unsafe { f(); }", "f();");
        let s = score_file(&f, &[]);
        assert_eq!(s.category, Category::Security, "{s:?}");
        let vis = SymbolChange {
            file: "src/a.rs".into(),
            name: "f".into(),
            kind: SymbolKind::Function,
            change: SymbolChangeKind::VisibilityChanged,
            confidence: 0.95,
            old_signature: None,
            new_signature: None,
            old_lines: None,
            new_lines: None,
            evidence: vec![],
        };
        let s = score_file(&fc("src/a.rs", "x", "y"), &[&vis]);
        assert_eq!(s.category, Category::ApiBreak, "{s:?}");
    }

    #[test]
    fn method_coverage_matches_short_call_names() {
        // `Auth::login` changed; a test calling `login()` must cover it.
        let syms = vec![SymbolChange {
            file: "src/auth.rs".into(),
            name: "Auth::login".into(),
            kind: SymbolKind::Method,
            change: SymbolChangeKind::Modified,
            confidence: 0.9,
            old_signature: None,
            new_signature: None,
            old_lines: None,
            new_lines: None,
            evidence: vec![],
        }];
        let mut test_fc = fc("tests/auth_test.rs", "+", "-");
        test_fc.is_test_file = true;
        test_fc.new_content = Some("fn test_login() {\n    login(user);\n}\n".into());
        let files = vec![
            {
                let mut f = fc("src/auth.rs", "+", "-");
                f.new_content = Some("class Auth:\n    def login(self):\n        pass\n".into());
                f
            },
            test_fc,
        ];
        let ctx = collect_context("", &files);
        let mut items = group_items(&files, &syms);
        apply_test_coverage(&files, &syms, &mut items, &ctx);
        let item = items
            .iter()
            .find(|i| i.id.starts_with("file:"))
            .expect("item");
        assert!(
            item.evidence.iter().any(|e| e.kind == "covering-tests"),
            "method covered by short-name call: {item:?}"
        );
    }

    /// The UI worker's sequential recipe must match batch analyze() exactly:
    /// same flags, same symbols, same items, same stats.
    #[test]
    fn worker_sequence_matches_batch() {
        fn content_fc(path: &str, old: &str, new: &str) -> FileChange {
            let mut f = fc(path, "+", "-");
            f.old_content = Some(old.to_string());
            f.new_content = Some(new.to_string());
            f
        }
        let mk = || {
            vec![
                content_fc(
                    "src/auth.rs",
                    "pub const SESSION_TIMEOUT: u64 = 900;\n",
                    "pub const SESSION_TIMEOUT: u64 = 86400;\n",
                ),
                content_fc(
                    "src/util.rs",
                    "pub fn a() -> i32 {\n1\n}\n",
                    "pub fn a() -> i32 {\n1\n}\n",
                ),
            ]
        };
        let mut batch_files = mk();
        let (batch_syms, batch_items) = analyze("r", "b", "h", &mut batch_files);
        let batch_stats = compute_stats(&batch_files, &batch_syms, &batch_items);

        // Worker recipe: mark -> per-file symbols -> link moves -> group -> stats.
        let mut worker_files = mk();
        mark_generated(&mut worker_files);
        let mut worker_syms = Vec::new();
        for f in worker_files.iter() {
            worker_syms.extend(file_symbols(f));
        }
        let worker_syms = link_moves(&worker_files, worker_syms);
        let ctx = collect_context("", &worker_files);
        let mut worker_items = group_items(&worker_files, &worker_syms);
        apply_test_coverage(&worker_files, &worker_syms, &mut worker_items, &ctx);
        apply_blast_radius(&worker_files, &mut worker_items, &ctx);
        let worker_stats = compute_stats(&worker_files, &worker_syms, &worker_items);

        fn js<T: serde::Serialize>(v: &T) -> String {
            serde_json::to_string(v).unwrap()
        }
        assert_eq!(js(&batch_syms), js(&worker_syms));
        assert_eq!(js(&batch_items), js(&worker_items));
        assert_eq!(js(&batch_stats), js(&worker_stats));
        // And the timeout change is actually caught (guards vacuous equality).
        assert_eq!(batch_syms.len(), 1);
        assert_eq!(batch_syms[0].name, "SESSION_TIMEOUT");
    }

    fn moved_fc(path: &str, old: Option<&str>, new: Option<&str>) -> FileChange {
        FileChange {
            old_path: path.to_string(),
            new_path: path.to_string(),
            status: FileStatus::Modified,
            language: Language::from_path(path),
            is_binary: false,
            is_generated: false,
            is_test_file: false,
            added_lines: 5,
            deleted_lines: 5,
            hunks: vec![],
            old_content: old.map(str::to_string),
            new_content: new.map(str::to_string),
        }
    }

    const HELPER: &str = "pub fn helper() -> i32 {\n    42\n}\n";

    #[test]
    fn cross_file_move_links() {
        let old_a = format!("{HELPER}\npub fn keep() {{}}\n");
        let new_b = format!("pub fn other() {{}}\n\n{HELPER}");
        let mut files = vec![
            moved_fc("src/a.rs", Some(old_a.as_str()), Some("pub fn keep() {}\n")),
            moved_fc(
                "src/b.rs",
                Some("pub fn other() {}\n"),
                Some(new_b.as_str()),
            ),
        ];
        let (syms, items) = analyze("r", "b", "h", &mut files);
        assert!(
            syms.iter().all(|s| !matches!(
                s.change,
                SymbolChangeKind::Added | SymbolChangeKind::Removed
            )),
            "{syms:?}"
        );
        let m: Vec<_> = syms
            .iter()
            .filter(|s| matches!(s.change, SymbolChangeKind::Moved))
            .collect();
        assert_eq!(m.len(), 1, "{syms:?}");
        assert_eq!(m[0].name, "helper");
        assert_eq!(m[0].file, "src/b.rs");
        assert!((m[0].confidence - 0.95).abs() < 1e-6);
        // One pair item; both files fully explained by it.
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, "move:src/a.rs→src/b.rs");
        assert_eq!(items[0].category, Category::Refactor);
        assert_eq!(items[0].severity, Severity::Low);
    }

    #[test]
    fn move_with_rename_links() {
        let renamed = HELPER.replace("helper", "helper2");
        let mut files = vec![
            moved_fc("src/a.rs", Some(HELPER), Some("")),
            moved_fc("src/b.rs", Some(""), Some(&renamed)),
        ];
        let (syms, _) = analyze("r", "b", "h", &mut files);
        let m: Vec<_> = syms
            .iter()
            .filter(|s| matches!(s.change, SymbolChangeKind::Moved))
            .collect();
        assert_eq!(m.len(), 1, "{syms:?}");
        assert_eq!(m[0].name, "helper → helper2");
    }

    #[test]
    fn intra_file_reorder_reports_moves() {
        let old = "pub fn one() {\n    1\n}\n\npub fn two() {\n    2\n}\n";
        let new = "pub fn two() {\n    2\n}\n\npub fn one() {\n    1\n}\n";
        let mut files = vec![moved_fc("src/r.rs", Some(old), Some(new))];
        let (syms, items) = analyze("r", "b", "h", &mut files);
        assert_eq!(syms.len(), 2, "{syms:?}");
        assert!(syms
            .iter()
            .all(|s| matches!(s.change, SymbolChangeKind::Moved)));
        // Reorder moves stay with their file (no cross-file origin).
        assert!(items.iter().any(|i| i.id == "file:src/r.rs"), "{items:?}");
    }

    #[test]
    fn ambiguous_duplicate_bodies_do_not_link() {
        // Same boilerplate removed in two files, added in one: no safe link.
        let boiler = "pub fn boiler() {}\n";
        let mut files = vec![
            moved_fc("src/a.rs", Some(boiler), Some("")),
            moved_fc("src/c.rs", Some(boiler), Some("")),
            moved_fc("src/b.rs", Some(""), Some(boiler)),
        ];
        let (syms, _) = analyze("r", "b", "h", &mut files);
        assert!(
            syms.iter()
                .all(|s| !matches!(s.change, SymbolChangeKind::Moved)),
            "{syms:?}"
        );
    }

    #[test]
    fn covering_test_links_by_call() {
        let mut files = vec![
            moved_fc(
                "src/auth.rs",
                Some("pub fn login(user: &str) -> bool {\n    false\n}\n"),
                Some("pub fn login(user: &str) -> bool {\n    check(user)\n}\n"),
            ),
            moved_fc(
                "tests/auth_test.rs",
                Some("#[test]\nfn test_login() {\n    assert!(login(\"a\"));\n}\n"),
                Some("#[test]\nfn test_login() {\n    assert!(login(\"a\"));\n}\n"),
            ),
        ];
        files[1].is_test_file = true;
        files[1].added_lines = 0;
        files[1].deleted_lines = 0;
        let (_, items) = analyze("r", "b", "h", &mut files);
        let item = items
            .iter()
            .find(|i| i.id == "file:src/auth.rs")
            .expect("auth item");
        let ev = item
            .evidence
            .iter()
            .find(|e| e.kind == "covering-tests")
            .expect("covering evidence");
        assert!(ev.summary.contains("test_login"), "{}", ev.summary);
        assert!(
            item.evidence.iter().all(|e| e.kind != "untested-change"),
            "{item:?}"
        );
    }

    #[test]
    fn behavior_change_without_tests_flagged() {
        let mut files = vec![
            moved_fc(
                "src/auth.rs",
                Some("pub fn login(user: &str) -> bool {\n    false\n}\n"),
                Some("pub fn login(user: &str) -> bool {\n    check(user)\n}\n"),
            ),
            moved_fc(
                "tests/other_test.rs",
                Some("#[test]\nfn test_other() {\n    assert!(other());\n}\n"),
                Some("#[test]\nfn test_other() {\n    assert!(other());\n}\n"),
            ),
        ];
        files[1].is_test_file = true;
        files[1].added_lines = 0;
        files[1].deleted_lines = 0;
        let (_, items) = analyze("r", "b", "h", &mut files);
        let item = items
            .iter()
            .find(|i| i.id == "file:src/auth.rs")
            .expect("auth item");
        assert!(
            item.evidence.iter().any(|e| e.kind == "untested-change"),
            "{item:?}"
        );
        assert!(item.why.contains("No covering tests"), "{}", item.why);
    }

    #[test]
    fn tests_bucket_counts_added_and_modified() {
        let mut added = moved_fc(
            "tests/new_test.rs",
            None,
            Some("#[test]\nfn test_a() {\n    assert!(true);\n}\n\n#[test]\nfn test_b() {\n    assert!(true);\n}\n"),
        );
        added.status = FileStatus::Added;
        added.is_test_file = true;
        let mut changed = moved_fc(
            "tests/old_test.rs",
            Some("#[test]\nfn test_c() {\n    assert!(true);\n}\n"),
            Some("#[test]\nfn test_c() {\n    assert!(false);\n}\n"),
        );
        changed.is_test_file = true;
        let mut files = vec![added, changed];
        let (_, items) = analyze("r", "b", "h", &mut files);
        let bucket = items.iter().find(|i| i.id == "tests").expect("bucket");
        assert!(
            bucket.title.contains("2 new test symbol"),
            "{}",
            bucket.title
        );
        assert!(bucket.title.contains("1 modified"), "{}", bucket.title);
        assert!(
            bucket
                .evidence
                .iter()
                .any(|e| e.kind == "test-modified" && e.summary == "test_c"),
            "{bucket:?}"
        );
        // Both files bucketed — no per-file items for them.
        assert!(
            items.iter().all(|i| !i.id.starts_with("file:tests/")),
            "{items:?}"
        );
    }

    #[test]
    fn unchanged_worktree_tests_cover() {
        // Covering test lives outside the diff: resolved via worktree scan.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(
            dir.path().join("tests/auth_test.rs"),
            "#[test]\nfn test_login() {\n    assert!(login(\"a\"));\n}\n",
        )
        .unwrap();
        let mut files = vec![moved_fc(
            "src/auth.rs",
            Some("pub fn login(user: &str) -> bool {\n    false\n}\n"),
            Some("pub fn login(user: &str) -> bool {\n    check(user)\n}\n"),
        )];
        mark_generated(&mut files);
        let syms: Vec<SymbolChange> = files.iter().flat_map(file_symbols).collect();
        let syms = link_moves(&files, syms);
        let mut items = group_items(&files, &syms);
        let ctx = collect_context(dir.path().to_str().unwrap(), &files);
        apply_test_coverage(&files, &syms, &mut items, &ctx);
        apply_blast_radius(&files, &mut items, &ctx);
        let item = items
            .iter()
            .find(|i| i.id == "file:src/auth.rs")
            .expect("auth item");
        let ev = item
            .evidence
            .iter()
            .find(|e| e.kind == "covering-tests")
            .expect("covering evidence");
        assert!(ev.summary.contains("test_login"), "{}", ev.summary);
        assert!(ev.summary.contains("tests/auth_test.rs"), "{}", ev.summary);
    }

    #[test]
    fn blast_radius_lists_unique_callers() {
        // routes.rs outside the diff calls changed login; only definer.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/routes.rs"),
            "use crate::auth::login;\n\npub fn handle() {\n    login(\"x\");\n}\n",
        )
        .unwrap();
        let mut files = vec![moved_fc(
            "src/auth.rs",
            Some("pub fn login(user: &str) -> bool {\n    false\n}\n"),
            Some("pub fn login(user: &str) -> bool {\n    check(user)\n}\n"),
        )];
        mark_generated(&mut files);
        let syms: Vec<SymbolChange> = files.iter().flat_map(file_symbols).collect();
        let syms = link_moves(&files, syms);
        let mut items = group_items(&files, &syms);
        let ctx = collect_context(dir.path().to_str().unwrap(), &files);
        apply_blast_radius(&files, &mut items, &ctx);
        let item = items
            .iter()
            .find(|i| i.id == "file:src/auth.rs")
            .expect("auth item");
        let ev = item
            .evidence
            .iter()
            .find(|e| e.kind == "blast-radius")
            .expect("blast evidence");
        assert!(ev.summary.contains("src/routes.rs"), "{}", ev.summary);
    }

    #[test]
    fn blast_radius_finds_callers_of_deleted_symbols() {
        // login is removed: only old content defines it, but the
        // unchanged caller with an import edge must still resolve.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/routes.rs"),
            "use crate::auth::login;\n\npub fn handle() {\n    login(\"x\");\n}\n",
        )
        .unwrap();
        let mut files = vec![moved_fc(
            "src/auth.rs",
            Some("pub fn login(user: &str) -> bool {\n    false\n}\n"),
            Some("pub fn other() {}\n"),
        )];
        mark_generated(&mut files);
        let syms: Vec<SymbolChange> = files.iter().flat_map(file_symbols).collect();
        assert!(syms.iter().any(|s| s.name == "login"), "{syms:?}");
        let syms = link_moves(&files, syms);
        let mut items = group_items(&files, &syms);
        let ctx = collect_context(dir.path().to_str().unwrap(), &files);
        apply_blast_radius(&files, &mut items, &ctx);
        let item = items
            .iter()
            .find(|i| i.id == "file:src/auth.rs")
            .expect("auth item");
        let ev = item
            .evidence
            .iter()
            .find(|e| e.kind == "blast-radius")
            .expect("blast evidence for deleted login: {item:?}");
        assert!(ev.summary.contains("src/routes.rs"), "{}", ev.summary);
    }

    #[test]
    fn blast_radius_needs_edge_for_ambiguous_names() {
        // Two definers of helper: caller without an import edge is skipped,
        // caller with `use` edge counts.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/other.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(
            dir.path().join("src/plain.rs"),
            "pub fn run() {\n    helper();\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/wired.rs"),
            "use crate::a::helper;\n\npub fn run() {\n    helper();\n}\n",
        )
        .unwrap();
        let mut files = vec![moved_fc(
            "src/a.rs",
            Some("pub fn helper() {\n    1\n}\n"),
            Some("pub fn helper() {\n    2\n}\n"),
        )];
        mark_generated(&mut files);
        let syms: Vec<SymbolChange> = files.iter().flat_map(file_symbols).collect();
        let syms = link_moves(&files, syms);
        let mut items = group_items(&files, &syms);
        let ctx = collect_context(dir.path().to_str().unwrap(), &files);
        apply_blast_radius(&files, &mut items, &ctx);
        let item = items
            .iter()
            .find(|i| i.id == "file:src/a.rs")
            .expect("a item");
        let ev = item
            .evidence
            .iter()
            .find(|e| e.kind == "blast-radius")
            .expect("blast evidence");
        assert!(ev.summary.contains("src/wired.rs"), "{}", ev.summary);
        assert!(!ev.summary.contains("src/plain.rs"), "{}", ev.summary);
    }

    #[test]
    fn lockfile_is_mechanical() {
        let mut files = vec![fc("Cargo.lock", "x", "y")];
        mark_generated(&mut files);
        assert!(files[0].is_generated);
    }

    #[test]
    fn string_mentions_are_not_signals() {
        assert_eq!(
            strip_string_literals(r#"ev("unsafe", "new `unsafe` code")"#),
            "ev(, )"
        );
        assert_eq!(strip_string_literals("// unsafe fn commented"), "");
        assert_eq!(
            strip_string_literals("let x = 1; // unsafe here"),
            "let x = 1; "
        );
        // A file whose only "unsafe" is inside string literals scores low.
        let f = fc("src/plain.rs", r#"let s = "unsafe fn nope";"#, "let s = 1;");
        let s = score_file(&f, &[]);
        assert!(s.priority < 55, "priority was {}", s.priority);
    }

    #[test]
    fn oversize_content_skips_parsing() {
        // 600KB of functions: parsing is capped, no symbols out.
        let mut big = String::new();
        while big.len() < 600 * 1024 {
            big.push_str("pub fn f() {}\n");
        }
        let f = moved_fc("src/big.rs", Some(&big), Some(&big));
        assert!(file_symbols(&f).is_empty());
    }

    #[test]
    fn reorder_skips_generated_files() {
        // Same symbols, different order, but generated: no Moved entries.
        let old = "pub fn a() {}\npub fn b() {}\n";
        let new = "pub fn b() {}\npub fn a() {}\n";
        let mut f = moved_fc("src/gen.rs", Some(old), Some(new));
        f.is_generated = true;
        let syms = file_symbols(&f);
        assert!(syms.is_empty());
        let linked = link_moves(std::slice::from_ref(&f), syms);
        assert!(
            linked
                .iter()
                .all(|s| !matches!(s.change, SymbolChangeKind::Moved)),
            "{linked:?}"
        );
    }

    #[test]
    fn cached_pipeline_matches_uncached() {
        // Same fixture through both pipelines: conclusions must agree.
        let old = "pub fn login(x: &str) -> bool {\n    x.len() > 3\n}\n";
        let new = "pub fn login(x: &str) -> bool {\n    check(x)\n}\n";
        let mut a = vec![moved_fc("src/auth.rs", Some(old), Some(new))];
        let mut b = vec![moved_fc("src/auth.rs", Some(old), Some(new))];
        let (syms_a, items_a) = analyze("", "H", "w", &mut a);
        let mut cache = rift_parser::SymbolCache::new();
        let (syms_b, items_b) = analyze_with_cache("", "H", "w", &mut b, &mut cache);
        assert!(cache.hits > 0, "expected at least one cache hit");
        let key = |s: &SymbolChange| (s.file.clone(), s.name.clone(), format!("{:?}", s.change));
        let mut ka: Vec<_> = syms_a.iter().map(key).collect();
        let mut kb: Vec<_> = syms_b.iter().map(key).collect();
        ka.sort();
        kb.sort();
        assert_eq!(ka, kb, "symbol conclusions diverged");
        let ia: Vec<_> = items_a.iter().map(|i| (i.id.clone(), i.priority)).collect();
        let ib: Vec<_> = items_b.iter().map(|i| (i.id.clone(), i.priority)).collect();
        assert_eq!(ia, ib, "review items diverged");
    }
}
