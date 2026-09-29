//! `rift` — the review layer between coding agents and your codebase.
//!
//!     rift [PATH] [REVS...] [--staged] [--commit SHA] [--json] [--overview]

use anyhow::{Context, Result};
use clap::Parser;
use rift_core::ChangeSet;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "rift",
    version,
    about = "What actually changed? — semantic code-change review"
)]
struct Args {
    /// Repository path (default: current directory).
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Revisions: commit SHA, `base..head`, or branch names.
    #[arg(default_value_t = String::new())]
    revs: String,

    /// Second revision, if `revs` is the base (`rift main HEAD`).
    #[arg(default_value_t = String::new(), hide = true)]
    rev2: String,

    /// Diff staged changes (index vs HEAD).
    #[arg(long)]
    staged: bool,

    /// Show one commit vs its parent.
    #[arg(long)]
    commit: Option<String>,

    /// Include untracked files (default: true for worktree mode).
    #[arg(long, default_value_t = true)]
    untracked: bool,

    /// Print the ChangeSet as JSON and exit (for agents / tooling).
    #[arg(long)]
    json: bool,

    /// Print a text overview and exit (no GUI).
    #[arg(long)]
    overview: bool,

    /// Never launch the GUI (same as --overview, explicit).
    #[arg(long)]
    no_gui: bool,

    /// Accepted for forward-compat; Rift is local-first and deterministic.
    #[arg(long)]
    no_ai: bool,

    /// Ask Jev (TypeSafe cloud API) for risk/severity judgments on each
    /// behavior-grade item. Requires TYPESAFE_API_KEY. Sends structured
    /// facts only (symbols, evidence summaries, counts) — never source.
    /// Batch output only (--json/--overview/--no-gui).
    #[arg(long)]
    jev: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    // GUI path streams analysis progressively (window first, results land
    // live). Batch paths (--json/--overview) analyze up front.
    let batch = args.json || args.overview || args.no_gui;
    if batch {
        let cs = build_changeset(&args)?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&cs)?);
        } else {
            print!("{}", rift_ui::render_text_overview(&cs));
        }
        return Ok(());
    }
    let engine = open_repo(&args.path)?;
    let root = engine.root.to_string_lossy().replace('\\', "/");
    let (files, base_ref, head_ref) = resolve_files(&engine, &args)?;
    // GUI first; fall back to text when headless.
    match rift_ui::run_native_progressive(root, base_ref, head_ref, files) {
        Ok(_) => Ok(()),
        Err(e) => {
            eprintln!("rift: GUI unavailable ({e}); printing overview instead.\n");
            let cs = build_changeset(&args)?;
            print!("{}", rift_ui::render_text_overview(&cs));
            Ok(())
        }
    }
}

fn build_changeset(args: &Args) -> Result<ChangeSet> {
    let engine = open_repo(&args.path)?;
    let root = engine.root.to_string_lossy().replace('\\', "/");

    let (mut files, base_ref, head_ref) = resolve_files(&engine, args)?;

    let (syms, mut items) = rift_analysis::analyze(&root, &base_ref, &head_ref, &mut files);
    if args.jev {
        if args.json || args.overview || args.no_gui {
            match rift_jev::enrich(
                &base_ref,
                &head_ref,
                &mut items,
                std::env::var("TYPESAFE_API_KEY").ok().as_deref(),
            ) {
                rift_jev::JevStatus::Applied { items: n } => {
                    eprintln!("rift: jev judged {n} items (structured facts only, no source sent)");
                }
                rift_jev::JevStatus::SkippedNoKey => {
                    eprintln!("rift: --jev needs TYPESAFE_API_KEY in the environment; skipping");
                }
                rift_jev::JevStatus::SkippedNoItems => {}
                rift_jev::JevStatus::Failed(e) => {
                    eprintln!("rift: jev enrichment failed ({e}); review continues without it");
                }
            }
        } else {
            eprintln!("rift: --jev currently enriches batch output; add --overview or --json");
        }
    }
    let mut cs = ChangeSet {
        repo_root: root,
        base_ref,
        head_ref,
        files,
        symbol_changes: syms,
        review_items: items,
        stats: Default::default(),
    };
    rift_analysis::fill_stats(&mut cs);
    Ok(cs)
}

/// Open a repo with a human error when PATH isn't in one. First-run
/// stumble, so it says what to do instead of dumping libgit2 codes.
fn open_repo(path: &Path) -> Result<rift_git::GitEngine> {
    match rift_git::GitEngine::discover(path) {
        Ok(engine) => Ok(engine),
        Err(err) => {
            let not_repo = err.chain().any(|c| {
                c.to_string().contains("NotFound") || c.to_string().contains("not a git repository")
            });
            if not_repo {
                return Err(anyhow::anyhow!(
                    "'{}' is not inside a git repository.\n\nrift reviews git changes, so it needs a repo to read:\n  inside a project:  cd <repo> && rift\n  start tracking:    cd <dir> && git init && git add -A && git commit -m init\n  point at one:      rift <path-to-repo>",
                    path.display()
                ));
            }
            Err(err).with_context(|| format!("could not open repository at {}", path.display()))
        }
    }
}

/// Git file list + ref labels, without any symbol analysis.
fn resolve_files(
    engine: &rift_git::GitEngine,
    args: &Args,
) -> Result<(Vec<rift_core::FileChange>, String, String)> {
    if let Some(sha) = &args.commit {
        return Ok((
            engine.commit_changeset(sha)?,
            format!("{sha}^"),
            sha.clone(),
        ));
    }
    if args.staged {
        return Ok((
            engine.staged_changeset()?,
            "HEAD".to_string(),
            "index (staged)".to_string(),
        ));
    }
    if !args.revs.is_empty() {
        return parse_revs(engine, &args.revs, &args.rev2);
    }
    Ok((
        engine.worktree_changeset(args.untracked)?,
        "HEAD".to_string(),
        format!("worktree ({})", engine.head_short()),
    ))
}

fn parse_revs(
    engine: &rift_git::GitEngine,
    r1: &str,
    r2: &str,
) -> Result<(Vec<rift_core::FileChange>, String, String)> {
    // `base..head` form.
    if let Some((b, h)) = r1.split_once("..") {
        let base = b.trim();
        let head = h.trim();
        let head = if head.is_empty() { "HEAD" } else { head };
        let base = if base.is_empty() { "HEAD" } else { base };
        let files = engine.range_changeset(base, head)?;
        return Ok((files, base.to_string(), head.to_string()));
    }
    // Two positional revs: `rift main feature/x`.
    if !r2.is_empty() {
        let files = engine.range_changeset(r1, r2)?;
        return Ok((files, r1.to_string(), r2.to_string()));
    }
    // Single rev: commit vs parent.
    let files = engine.commit_changeset(r1)?;
    Ok((files, format!("{r1}^"), r1.to_string()))
}
