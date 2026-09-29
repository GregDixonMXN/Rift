//! `rift` — the review layer between coding agents and your codebase.
//!
//!     rift [PATH] [REVS...] [--staged] [--commit SHA] [--json] [--overview]

use anyhow::{Context, Result};
use clap::Parser;
use rift_core::ChangeSet;
use std::path::PathBuf;

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
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cs = build_changeset(&args)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&cs)?);
        return Ok(());
    }
    if args.overview || args.no_gui {
        print!("{}", rift_ui::render_text_overview(&cs));
        return Ok(());
    }
    // GUI first; fall back to text when headless.
    match rift_ui::run_native(cs.clone()) {
        Ok(_) => Ok(()),
        Err(e) => {
            eprintln!("rift: GUI unavailable ({e}); printing overview instead.\n");
            print!("{}", rift_ui::render_text_overview(&cs));
            Ok(())
        }
    }
}

fn build_changeset(args: &Args) -> Result<ChangeSet> {
    let engine = rift_git::GitEngine::discover(&args.path)
        .with_context(|| format!("could not open repository at {}", args.path.display()))?;
    let root = engine.root.to_string_lossy().replace('\\', "/");

    let (files, base_ref, head_ref) = if let Some(sha) = &args.commit {
        (
            engine.commit_changeset(sha)?,
            format!("{sha}^"),
            sha.clone(),
        )
    } else if args.staged {
        (
            engine.staged_changeset()?,
            "HEAD".to_string(),
            "index (staged)".to_string(),
        )
    } else if !args.revs.is_empty() {
        parse_revs(&engine, &args.revs, &args.rev2)?
    } else {
        (
            engine.worktree_changeset(args.untracked)?,
            "HEAD".to_string(),
            format!("worktree ({})", engine.head_short()),
        )
    };

    let mut files = files;
    let (syms, items) = rift_analysis::analyze(&root, &base_ref, &head_ref, &mut files);
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
