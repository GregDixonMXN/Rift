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
    #[arg(long)]
    jev: bool,

    /// Deterministic (offline) Jev judgments: same evidence kinds as --jev,
    /// computed locally with no API key and no network. For CI and calibration.
    #[arg(long)]
    jev_local: bool,

    /// CI/hook gate (batch only): exit 2 when any meaningful review item
    /// reaches this severity or higher. Levels: low, medium, high, critical.
    #[arg(long, value_name = "SEVERITY")]
    fail_on: Option<String>,

    /// Task-vs-change check (batch only): verify the diff covers this task.
    /// Inline text (`--task "extend session timeout"`) or `@path` to read
    /// the description from a file (`--task @task.md`). Prints a coverage
    /// section in --overview and embeds `task_check` in --json.
    #[arg(long, value_name = "TASK")]
    task: Option<String>,

    /// CI/hook gate on the task check (batch only, needs --task): exit 2
    /// when the task verdict is this level or worse. Levels (best to
    /// worst): covered, partial, uncovered. So `--fail-on-task partial`
    /// requires Covered, while `uncovered` only fails a total miss.
    #[arg(long, value_name = "VERDICT")]
    fail_on_task: Option<String>,

    /// LLM escalation (batch only): emit a compact evidence-only package
    /// for the riskiest items instead of the full review. Structured facts
    /// only (titles, symbols, evidence summaries) — never source — so the
    /// output is safe to pipe to any external model. Pairs with --json
    /// (package JSON) or --overview (readable summary).
    #[arg(long)]
    escalate: bool,

    /// Minimum severity to escalate. Levels: low, medium, high, critical.
    #[arg(long, default_value = "high", value_name = "SEVERITY")]
    escalate_on: String,

    /// Max items in the package (1..25, bounds LLM tokens/latency/cost).
    #[arg(long, default_value_t = 10)]
    max_escalations: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    // GUI path streams analysis progressively (window first, results land
    // live). Batch paths (--json/--overview) analyze up front.
    let batch = args.json || args.overview || args.no_gui;
    if batch {
        let cs = build_changeset(&args)?;
        if args.escalate {
            let floor = parse_severity(&args.escalate_on).map_err(|_| {
                anyhow::anyhow!(
                    "invalid --escalate-on level '{}': expected low, medium, high, or critical",
                    args.escalate_on
                )
            })?;
            let pkg = rift_analysis::build_package(&cs, floor, args.max_escalations);
            if args.json {
                println!("{}", serde_json::to_string_pretty(&pkg)?);
            } else {
                print!("{}", rift_analysis::render_summary(&pkg));
            }
            eprintln!(
                "rift: escalated {} item(s), ~{} tokens (evidence only, no source)",
                pkg.items.len(),
                pkg.approx_tokens
            );
        } else if args.json {
            println!("{}", serde_json::to_string_pretty(&cs)?);
        } else {
            print!("{}", rift_ui::render_text_overview(&cs));
        }
        if let Some(level) = args.fail_on.as_deref() {
            let threshold = parse_severity(level)?;
            if let Some(hit) = gate_trigger(&cs, threshold) {
                eprintln!(
                    "rift: gate {level} triggered by [{}] {}",
                    format!("{:?}", hit.severity).to_lowercase(),
                    hit.title
                );
                std::process::exit(2);
            }
        }
        if let Some(level) = args.fail_on_task.as_deref() {
            let threshold = parse_task_verdict(level)?;
            let check = cs.task_check.as_ref().ok_or_else(|| {
                anyhow::anyhow!("--fail-on-task needs --task (no task was checked)")
            })?;
            if task_gate_triggered(check, threshold) {
                eprintln!(
                    "rift: task gate {level} triggered — [{:?}] {}% ({} of {} terms, missing: {})",
                    check.verdict,
                    (check.coverage * 100.0).round() as u32,
                    check.matched.len(),
                    check.terms.len(),
                    if check.unmatched.is_empty() {
                        "none".to_string()
                    } else {
                        check.unmatched.join(", ")
                    }
                );
                std::process::exit(2);
            }
        }
        return Ok(());
    }
    if args.task.is_some() {
        eprintln!("rift: --task needs --overview, --json, or --no-gui; ignoring it for the GUI run");
    }
    if args.fail_on_task.is_some() {
        eprintln!("rift: --fail-on-task needs --overview, --json, or --no-gui; ignoring it for the GUI run");
    }
    if args.escalate {
        eprintln!("rift: --escalate needs --overview, --json, or --no-gui; ignoring it for the GUI run");
    }
    let engine = open_repo(&args.path)?;
    let root = engine.root.to_string_lossy().replace('\\', "/");
    let (files, base_ref, head_ref) = resolve_files(&engine, &args)?;
    // GUI first; fall back to text when headless.
    let jev_key = args
        .jev
        .then(|| std::env::var("TYPESAFE_API_KEY").ok())
        .flatten();
    if args.jev && jev_key.is_none() {
        eprintln!("rift: --jev needs TYPESAFE_API_KEY in the environment; continuing without it");
    }
    match rift_ui::run_native_progressive(root, base_ref, head_ref, files, jev_key, args.jev_local) {
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

    // Persistent symbol cache: warm runs skip re-parsing unchanged contents.
    // Best-effort — a missing or unwritable cache never fails the review.
    let cache_path = rift_parser::default_cache_path();
    let mut cache = cache_path
        .as_ref()
        .map(|p| rift_parser::SymbolCache::load(p))
        .unwrap_or_default();
    let (syms, mut items) =
        rift_analysis::analyze_with_cache(&root, &base_ref, &head_ref, &mut files, &mut cache);
    if let Some(path) = cache_path.as_ref() {
        if let Err(e) = cache.save(path) {
            eprintln!("rift: symbol cache save skipped ({e})");
        }
    }
    if args.jev_local && (args.json || args.overview || args.no_gui) {
        use rift_jev::Judge as _;
        match rift_jev::DeterministicJudge.judge(&base_ref, &head_ref, &mut items) {
            rift_jev::JevStatus::Applied { items: n } => {
                eprintln!("rift: jev-local judged {n} items (deterministic, offline)");
            }
            rift_jev::JevStatus::SkippedNoItems => {}
            other => {
                eprintln!("rift: jev-local skipped ({other:?}); review continues without it");
            }
        }
    } else if args.jev && (args.json || args.overview || args.no_gui) {
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
    }
    let mut cs = ChangeSet {
        repo_root: root,
        base_ref,
        head_ref,
        files,
        symbol_changes: syms,
        review_items: items,
        stats: Default::default(),
        task_check: None,
    };
    rift_analysis::fill_stats(&mut cs);
    if let Some(task_text) = resolve_task_text(args.task.as_deref())? {
        cs.task_check = Some(rift_analysis::check_task(&cs, &task_text));
    }
    Ok(cs)
}

/// Resolve `--task` text: `@path` reads the description from a file,
/// anything else is used inline. `None` when the flag is absent.
fn resolve_task_text(flag: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = flag else {
        return Ok(None);
    };
    if let Some(path) = raw.strip_prefix('@') {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read task file '{path}'"))?;
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            anyhow::bail!("task file '{path}' is empty");
        }
        Ok(Some(trimmed))
    } else {
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            anyhow::bail!("--task needs non-empty text (or @path to a task file)");
        }
        Ok(Some(trimmed))
    }
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

/// Parse a `--fail-on` level. Case-insensitive; errors list the valid levels.
fn parse_severity(level: &str) -> Result<rift_core::Severity> {
    use rift_core::Severity;
    match level.to_lowercase().as_str() {
        "low" => Ok(Severity::Low),
        "medium" | "med" => Ok(Severity::Medium),
        "high" => Ok(Severity::High),
        "critical" | "crit" => Ok(Severity::Critical),
        other => anyhow::bail!("invalid --fail-on level '{other}': expected low, medium, high, or critical"),
    }
}

/// Parse a `--fail-on-task` level. Case-insensitive; errors list the valid
/// levels (best to worst: covered, partial, uncovered).
fn parse_task_verdict(level: &str) -> Result<rift_core::TaskVerdict> {
    use rift_core::TaskVerdict;
    match level.to_lowercase().as_str() {
        "covered" | "cover" => Ok(TaskVerdict::Covered),
        "partial" => Ok(TaskVerdict::Partial),
        "uncovered" | "uncover" => Ok(TaskVerdict::Uncovered),
        other => anyhow::bail!(
            "invalid --fail-on-task level '{other}': expected covered, partial, or uncovered"
        ),
    }
}

/// Quality rank for verdicts (higher is better). The gate fires when the
/// check's rank is at or below the threshold's rank — i.e. the verdict is
/// the threshold level or worse.
fn verdict_rank(v: rift_core::TaskVerdict) -> u8 {
    use rift_core::TaskVerdict;
    match v {
        TaskVerdict::Covered => 2,
        TaskVerdict::Partial => 1,
        TaskVerdict::Uncovered => 0,
    }
}

/// Whether the task gate fires: verdict at or below `threshold` quality.
fn task_gate_triggered(check: &rift_core::TaskCheck, threshold: rift_core::TaskVerdict) -> bool {
    verdict_rank(check.verdict) <= verdict_rank(threshold)
}

/// Highest-priority meaningful item at or above `threshold`, if any.
/// Mechanical items never gate (they are collapsed output, not risk); an
/// empty or all-mechanical review always passes.
fn gate_trigger(cs: &ChangeSet, threshold: rift_core::Severity) -> Option<&rift_core::ReviewItem> {
    cs.review_items
        .iter()
        .filter(|r| !matches!(r.category, rift_core::Category::Mechanical))
        .filter(|r| r.severity >= threshold)
        .max_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then(a.priority.cmp(&b.priority))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{Category, ReviewItem, Severity};

    fn item(title: &str, category: Category, severity: Severity) -> ReviewItem {
        ReviewItem {
            id: title.into(),
            title: title.into(),
            category,
            severity,
            priority: 50,
            confidence: 0.8,
            files: vec!["a.rs".into()],
            symbols: vec![],
            evidence: vec![],
            why: String::new(),
        }
    }

    fn cs_with(items: Vec<ReviewItem>) -> ChangeSet {
        ChangeSet {
            repo_root: ".".into(),
            base_ref: "H".into(),
            head_ref: "w".into(),
            files: vec![],
            symbol_changes: vec![],
            review_items: items,
            stats: Default::default(),
            task_check: None,
        }
    }

    #[test]
    fn parses_levels_case_insensitively() {
        assert_eq!(parse_severity("low").unwrap(), Severity::Low);
        assert_eq!(parse_severity("MEDIUM").unwrap(), Severity::Medium);
        assert_eq!(parse_severity("High").unwrap(), Severity::High);
        assert_eq!(parse_severity("crit").unwrap(), Severity::Critical);
        assert!(parse_severity("extreme").is_err());
    }

    #[test]
    fn gate_fires_on_highest_qualifying_item() {
        let cs = cs_with(vec![
            item("docs", Category::Docs, Severity::Low),
            item("auth", Category::Auth, Severity::High),
            item("dep", Category::Dependency, Severity::Critical),
        ]);
        assert_eq!(gate_trigger(&cs, Severity::High).unwrap().title, "dep");
        assert_eq!(gate_trigger(&cs, Severity::Critical).unwrap().title, "dep");
        assert!(gate_trigger(&cs, Severity::Critical).is_some());
    }

    #[test]
    fn gate_ignores_mechanical_and_passes_clean() {
        let mech = cs_with(vec![item("lock", Category::Mechanical, Severity::Low)]);
        assert!(gate_trigger(&mech, Severity::Low).is_none());
        let clean = cs_with(vec![]);
        assert!(gate_trigger(&clean, Severity::Low).is_none());
        let low = cs_with(vec![item("docs", Category::Docs, Severity::Low)]);
        assert!(gate_trigger(&low, Severity::Medium).is_none());
        assert!(gate_trigger(&low, Severity::Low).is_some());
    }
    #[test]
    fn task_text_inline_is_trimmed() {
        assert_eq!(
            resolve_task_text(Some("  extend timeout  ")).unwrap(),
            Some("extend timeout".to_string())
        );
        assert!(resolve_task_text(None).unwrap().is_none());
        assert!(resolve_task_text(Some("   ")).is_err());
    }

    #[test]
    fn task_text_at_path_reads_file() {
        let dir = std::env::temp_dir().join(format!("rift-task-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("task.md");
        std::fs::write(&path, "  session timeout work\n").unwrap();
        let flag = format!("@{}", path.display());
        assert_eq!(
            resolve_task_text(Some(&flag)).unwrap(),
            Some("session timeout work".to_string())
        );
        assert!(resolve_task_text(Some("@/nonexistent-rift-task-xyz.md")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_task_verdicts_case_insensitively() {
        use rift_core::TaskVerdict;
        assert_eq!(parse_task_verdict("covered").unwrap(), TaskVerdict::Covered);
        assert_eq!(parse_task_verdict("PARTIAL").unwrap(), TaskVerdict::Partial);
        assert_eq!(
            parse_task_verdict("uncovered").unwrap(),
            TaskVerdict::Uncovered
        );
        assert!(parse_task_verdict("extreme").is_err());
    }

    #[test]
    fn task_gate_fires_at_or_below_threshold() {
        use rift_core::{TaskCheck, TaskVerdict};
        let check = |verdict| TaskCheck {
            task_text: "t".into(),
            terms: vec!["t".into()],
            matched: vec![],
            unmatched: vec!["t".into()],
            item_hits: vec![],
            coverage: 0.0,
            verdict,
        };
        // Threshold partial: Partial and Uncovered fail, Covered passes.
        assert!(task_gate_triggered(
            &check(TaskVerdict::Uncovered),
            TaskVerdict::Partial
        ));
        assert!(task_gate_triggered(
            &check(TaskVerdict::Partial),
            TaskVerdict::Partial
        ));
        assert!(!task_gate_triggered(
            &check(TaskVerdict::Covered),
            TaskVerdict::Partial
        ));
        // Threshold uncovered: only a total miss fails.
        assert!(task_gate_triggered(
            &check(TaskVerdict::Uncovered),
            TaskVerdict::Uncovered
        ));
        assert!(!task_gate_triggered(
            &check(TaskVerdict::Partial),
            TaskVerdict::Uncovered
        ));
    }

}
