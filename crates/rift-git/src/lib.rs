//! Git engine: normalized ChangeSet extraction over libgit2 (git2 crate).
//! Pure-git facts only — no semantic conclusions here.

use anyhow::{Context, Result};
use git2::{Delta, DiffOptions, Repository, StatusOptions};
use rift_core::{DiffLine, DiffLineKind, FileChange, FileStatus, Hunk, Language};
use std::path::{Path, PathBuf};

const MAX_CONTENT_BYTES: u64 = 512 * 1024;

pub struct GitEngine {
    repo: Repository,
    pub root: PathBuf,
}

impl GitEngine {
    pub fn discover(start: &Path) -> Result<Self> {
        let repo = Repository::discover(start)
            .with_context(|| format!("not a git repository: {}", start.display()))?;
        let root = repo
            .workdir()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| repo.path().to_path_buf());
        Ok(Self { repo, root })
    }

    pub fn head_short(&self) -> String {
        self.repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(|s| s.to_string()))
            .unwrap_or_else(|| "HEAD".to_string())
    }

    /// Working tree vs HEAD: unstaged + staged + (optionally) untracked.
    pub fn worktree_changeset(&self, include_untracked: bool) -> Result<Vec<FileChange>> {
        let mut opts = DiffOptions::new();
        opts.context_lines(3)
            .interhunk_lines(0)
            .minimal(true)
            .indent_heuristic(true)
            .ignore_submodules(true)
            .include_untracked(false)
            .recurse_untracked_dirs(false)
            .show_untracked_content(false);

        let head = self.repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        let mut diff = self
            .repo
            .diff_tree_to_workdir_with_index(head.as_ref(), Some(&mut opts))
            .context("diff HEAD -> workdir failed")?;

        let mut files = diff_to_files(&mut diff, &self.root, &self.repo)?;

        if include_untracked {
            files.extend(self.untracked_files()?);
        }
        Ok(files)
    }

    pub fn staged_changeset(&self) -> Result<Vec<FileChange>> {
        let head = self.repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        let mut opts = DiffOptions::new();
        opts.context_lines(3).minimal(true).indent_heuristic(true);
        let mut diff = self
            .repo
            .diff_tree_to_index(head.as_ref(), None, Some(&mut opts))
            .context("diff HEAD -> index failed")?;
        diff_to_files(&mut diff, &self.root, &self.repo)
    }

    /// Single commit vs its first parent (or empty tree for root commit).
    pub fn commit_changeset(&self, sha: &str) -> Result<Vec<FileChange>> {
        let obj = self
            .repo
            .revparse_single(sha)
            .with_context(|| format!("unknown revision: {sha}"))?;
        let commit = obj.peel_to_commit().context("revision is not a commit")?;
        let new_tree = commit.tree().context("commit has no tree")?;
        let old_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
        let mut opts = DiffOptions::new();
        opts.context_lines(3).minimal(true).indent_heuristic(true);
        let mut diff = self
            .repo
            .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))
            .context("commit diff failed")?;
        let mut files = diff_to_files(&mut diff, &self.root, &self.repo)?;
        // Populate content from git objects for commit diffs.
        for f in files.iter_mut() {
            let _ = fill_commit_contents(&self.repo, &commit, f);
        }
        Ok(files)
    }

    /// `base..head` revision range via merge-base aware diff.
    pub fn range_changeset(&self, base: &str, head: &str) -> Result<Vec<FileChange>> {
        let base_obj = self
            .repo
            .revparse_single(base)
            .with_context(|| format!("unknown revision: {base}"))?;
        let head_obj = self
            .repo
            .revparse_single(head)
            .with_context(|| format!("unknown revision: {head}"))?;
        let base_tree = base_obj.peel_to_tree().context("base has no tree")?;
        let head_tree = head_obj.peel_to_tree().context("head has no tree")?;
        let mut opts = DiffOptions::new();
        opts.context_lines(3).minimal(true).indent_heuristic(true);
        let mut diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut opts))
            .context("range diff failed")?;
        diff_to_files(&mut diff, &self.root, &self.repo)
    }

    fn untracked_files(&self) -> Result<Vec<FileChange>> {
        let mut sopts = StatusOptions::new();
        sopts
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false);
        let statuses = self.repo.statuses(Some(&mut sopts))?;
        let mut out = Vec::new();
        for entry in statuses.iter() {
            let s = entry.status();
            if !s.is_wt_new() {
                continue;
            }
            let rel = entry.path().unwrap_or_default().to_string();
            let abs = self.root.join(&rel);
            if abs.is_dir() {
                continue;
            }
            let meta = std::fs::metadata(&abs);
            let size = meta.map(|m| m.len()).unwrap_or(0);
            if size > MAX_CONTENT_BYTES * 4 {
                out.push(FileChange {
                    old_path: String::new(),
                    new_path: rel.clone(),
                    status: FileStatus::Untracked,
                    language: Language::from_path(&rel),
                    is_binary: true,
                    is_generated: false,
                    is_test_file: is_test_path(&rel),
                    added_lines: 0,
                    deleted_lines: 0,
                    hunks: vec![],
                    old_content: None,
                    new_content: None,
                });
                continue;
            }
            let content = std::fs::read_to_string(&abs).ok();
            let is_binary = content.is_none();
            let added = content.as_ref().map(|c| c.lines().count()).unwrap_or(0);
            let hunks = content
                .as_ref()
                .map(|c| {
                    vec![Hunk {
                        old_start: 0,
                        old_lines: 0,
                        new_start: 1,
                        new_lines: added as u32,
                        header: "untracked file".to_string(),
                        lines: c
                            .lines()
                            .map(|l| DiffLine {
                                kind: DiffLineKind::Addition,
                                text: l.to_string(),
                            })
                            .collect(),
                    }]
                })
                .unwrap_or_default();
            out.push(FileChange {
                old_path: String::new(),
                new_path: rel.clone(),
                status: FileStatus::Untracked,
                language: Language::from_path(&rel),
                is_binary,
                is_generated: false,
                is_test_file: is_test_path(&rel),
                added_lines: added,
                deleted_lines: 0,
                hunks,
                old_content: None,
                new_content: content,
            });
        }
        Ok(out)
    }
}

fn delta_to_status(d: Delta) -> FileStatus {
    match d {
        Delta::Added => FileStatus::Added,
        Delta::Deleted => FileStatus::Deleted,
        Delta::Modified => FileStatus::Modified,
        Delta::Renamed => FileStatus::Renamed,
        Delta::Copied => FileStatus::Copied,
        Delta::Typechange => FileStatus::TypeChanged,
        _ => FileStatus::Modified,
    }
}

struct Pending {
    old_path: String,
    new_path: String,
    status: FileStatus,
    is_binary: bool,
    hunks: Vec<Hunk>,
    added: usize,
    deleted: usize,
    cur: Option<Hunk>,
}

fn flush_pending(cur: &mut Option<Pending>, ordered: &mut Vec<Pending>) {
    if let Some(mut p) = cur.take() {
        if let Some(h) = p.cur.take() {
            p.hunks.push(h);
        }
        ordered.push(p);
    }
}

fn diff_to_files(diff: &mut git2::Diff, root: &Path, repo: &Repository) -> Result<Vec<FileChange>> {
    // Rename/copy detection (similarity-based, replaces DiffOptions flags).
    {
        let mut find_opts = git2::DiffFindOptions::new();
        find_opts.renames(true).copies(true).rename_limit(1000);
        let _ = diff.find_similar(Some(&mut find_opts));
    }
    // Walk hunks/lines via structured foreach callbacks (file order from deltas).
    // Cells: foreach takes all callbacks at once, so shared state goes behind
    // interior mutability instead of stacked `&mut` borrows.
    let ordered = std::cell::RefCell::new(Vec::<Pending>::new());
    let cur = std::cell::RefCell::new(None::<Pending>);
    // Re-derive from deltas for file headers.
    let mut delta_infos: Vec<(String, String, FileStatus)> = Vec::new();
    let mut i = 0;
    while let Some(delta) = diff.get_delta(i) {
        i += 1;
        let of = delta
            .old_file()
            .path()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let nf = delta
            .new_file()
            .path()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        delta_infos.push((of, nf, delta_to_status(delta.status())));
    }
    let mut di = 0usize;
    diff.foreach(
        &mut |_delta, _| {
            if di < delta_infos.len() {
                let (ref of, ref nf, st) = delta_infos[di];
                flush_pending(&mut cur.borrow_mut(), &mut ordered.borrow_mut());
                *cur.borrow_mut() = Some(Pending {
                    old_path: of.clone(),
                    new_path: nf.clone(),
                    status: st,
                    is_binary: false,
                    hunks: vec![],
                    added: 0,
                    deleted: 0,
                    cur: None,
                });
                di += 1;
            }
            true
        },
        None,
        Some(&mut |_delta, hunk| {
            let mut guard = cur.borrow_mut();
            if let Some(c) = guard.as_mut() {
                if let Some(h) = c.cur.take() {
                    c.hunks.push(h);
                }
                let header = String::from_utf8_lossy(hunk.header()).to_string();
                c.cur = Some(Hunk {
                    old_start: hunk.old_start(),
                    old_lines: hunk.old_lines(),
                    new_start: hunk.new_start(),
                    new_lines: hunk.new_lines(),
                    header,
                    lines: vec![],
                });
            }
            true
        }),
        Some(&mut |_delta, _hunk, line| {
            let mut guard = cur.borrow_mut();
            if let Some(c) = guard.as_mut() {
                let origin = line.origin();
                let text = String::from_utf8_lossy(line.content()).to_string();
                let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
                match origin {
                    '+' => {
                        c.added += 1;
                        if let Some(h) = c.cur.as_mut() {
                            h.lines.push(DiffLine {
                                kind: DiffLineKind::Addition,
                                text,
                            });
                        }
                    }
                    '-' => {
                        c.deleted += 1;
                        if let Some(h) = c.cur.as_mut() {
                            h.lines.push(DiffLine {
                                kind: DiffLineKind::Deletion,
                                text,
                            });
                        }
                    }
                    'F' | 'H' => {}
                    _ => {
                        if let Some(h) = c.cur.as_mut() {
                            // ' ' context or other markers
                            if origin == 'B' {
                                c.is_binary = true;
                            } else {
                                h.lines.push(DiffLine {
                                    kind: DiffLineKind::Context,
                                    text,
                                });
                            }
                        }
                        if origin == 'B' {
                            c.is_binary = true;
                        }
                    }
                }
            }
            true
        }),
    )
    .ok();
    flush_pending(&mut cur.borrow_mut(), &mut ordered.borrow_mut());
    // Fallback: if foreach produced nothing but deltas exist (binary-only),
    // keep delta shells.
    if ordered.borrow().is_empty() {
        for (of, nf, st) in delta_infos {
            ordered.borrow_mut().push(Pending {
                old_path: of,
                new_path: nf,
                status: st,
                is_binary: true,
                hunks: vec![],
                added: 0,
                deleted: 0,
                cur: None,
            });
        }
    }

    let mut out = Vec::with_capacity(ordered.borrow().len());
    for p in ordered.into_inner() {
        let display = if !p.new_path.is_empty() {
            p.new_path.clone()
        } else {
            p.old_path.clone()
        };
        let language = Language::from_path(&display);
        let mut fc = FileChange {
            old_path: p.old_path,
            new_path: p.new_path,
            status: p.status,
            language,
            is_binary: p.is_binary,
            is_generated: false,
            is_test_file: is_test_path(&display),
            added_lines: p.added,
            deleted_lines: p.deleted,
            hunks: p.hunks,
            old_content: None,
            new_content: None,
        };
        fill_worktree_contents(repo, root, &mut fc);
        out.push(fc);
    }
    Ok(out)
}

fn fill_worktree_contents(repo: &Repository, root: &Path, fc: &mut FileChange) {
    if fc.is_binary {
        return;
    }
    // New content: worktree file (if not deleted).
    if !matches!(fc.status, FileStatus::Deleted) && !fc.new_path.is_empty() {
        let abs = root.join(&fc.new_path);
        if let Ok(m) = std::fs::metadata(&abs) {
            if m.len() <= MAX_CONTENT_BYTES {
                fc.new_content = std::fs::read_to_string(&abs).ok();
            }
        }
    }
    // Old content: HEAD blob (if not added/untracked).
    if !matches!(fc.status, FileStatus::Added | FileStatus::Untracked) && !fc.old_path.is_empty() {
        if let Ok(head) = repo.head() {
            if let Ok(tree) = head.peel_to_tree() {
                if let Ok(entry) = tree.get_path(Path::new(&fc.old_path)) {
                    if let Ok(obj) = entry.to_object(repo) {
                        if let Some(blob) = obj.as_blob() {
                            if blob.size() <= MAX_CONTENT_BYTES as usize && !blob.is_binary() {
                                fc.old_content = String::from_utf8(blob.content().to_vec()).ok();
                            } else if blob.is_binary() {
                                fc.is_binary = true;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn fill_commit_contents(
    repo: &Repository,
    commit: &git2::Commit,
    fc: &mut FileChange,
) -> Result<()> {
    if fc.is_binary {
        return Ok(());
    }
    let new_tree = commit.tree().ok();
    let old_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    if !matches!(fc.status, FileStatus::Deleted) {
        if let Some(t) = new_tree.as_ref() {
            if !fc.new_path.is_empty() {
                if let Ok(e) = t.get_path(Path::new(&fc.new_path)) {
                    if let Ok(o) = e.to_object(repo) {
                        if let Some(b) = o.as_blob() {
                            if b.size() <= MAX_CONTENT_BYTES as usize && !b.is_binary() {
                                fc.new_content = String::from_utf8(b.content().to_vec()).ok();
                            }
                        }
                    }
                }
            }
        }
    }
    if !matches!(fc.status, FileStatus::Added) {
        if let Some(t) = old_tree.as_ref() {
            if !fc.old_path.is_empty() {
                if let Ok(e) = t.get_path(Path::new(&fc.old_path)) {
                    if let Ok(o) = e.to_object(repo) {
                        if let Some(b) = o.as_blob() {
                            if b.size() <= MAX_CONTENT_BYTES as usize && !b.is_binary() {
                                fc.old_content = String::from_utf8(b.content().to_vec()).ok();
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn is_test_path(p: &str) -> bool {
    let l = p.to_lowercase();
    l.contains("/test")
        || l.contains("/tests/")
        || l.contains("test_")
        || l.ends_with("_test.rs")
        || l.ends_with("_test.ts")
        || l.ends_with("_test.py")
        || l.ends_with(".test.ts")
        || l.ends_with(".test.js")
        || l.ends_with(".spec.ts")
        || l.ends_with(".spec.js")
        || l.starts_with("test/")
        || l.starts_with("tests/")
}
