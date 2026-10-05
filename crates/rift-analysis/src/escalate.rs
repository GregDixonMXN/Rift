//! LLM escalation packages: compact, evidence-only review payloads.
//!
//! Same contract as the Jev layer: structured facts flow out (titles,
//! categories, symbol names, evidence summaries, counts), raw source never
//! does. `ReviewItem` has no source field, so leakage is impossible by
//! construction — the builder only copies fact fields and truncates them
//! to fixed budgets. Deterministic: same ChangeSet in, same bytes out.

use rift_core::{
    Category, ChangeSet, EscalationItem, EscalationPackage, EscalationTask,
    ESCALATION_SCHEMA_VERSION, ReviewItem, Severity,
};

/// Default severity floor: escalate High and Critical items.
pub const DEFAULT_FLOOR: Severity = Severity::High;
/// Default item cap: bounds tokens, latency, and cost of the LLM call.
pub const DEFAULT_MAX_ITEMS: usize = 10;
/// Hard cap (Jev parity): one escalation reviews at most this many items.
pub const MAX_ITEMS_LIMIT: usize = 25;

// Truncation budgets (chars). Keeps the package small enough to paste
// into any model while preserving the facts that matter.
const MAX_TITLE: usize = 200;
const MAX_WHY: usize = 500;
const MAX_EVIDENCE_SUMMARY: usize = 300;
const MAX_EVIDENCE_PER_ITEM: usize = 8;
const MAX_FILES_PER_ITEM: usize = 10;
const MAX_SYMBOLS_PER_ITEM: usize = 15;

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Rough token estimate: chars / 4 rounded up. Documented as approximate
/// — callers should budget headroom on top.
pub fn approx_tokens(s: &str) -> u32 {
    s.chars().count().div_ceil(4) as u32
}

/// Items worth escalating: everything except collapsed mechanical output,
/// at or above the severity floor, most severe first.
fn candidates(cs: &ChangeSet, floor: Severity) -> Vec<&ReviewItem> {
    let mut v: Vec<&ReviewItem> = cs
        .review_items
        .iter()
        .filter(|i| !matches!(i.category, Category::Mechanical))
        .filter(|i| i.severity >= floor)
        .collect();
    v.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.priority.cmp(&a.priority))
            .then(a.title.cmp(&b.title))
    });
    v
}

fn compact_item(item: &ReviewItem) -> EscalationItem {
    let title = truncate(&item.title, MAX_TITLE);
    let why = truncate(&item.why, MAX_WHY);
    // Pipeline order is deterministic, so keep it (most severe signals
    // already sort first upstream via item priority).
    let evidence: Vec<String> = item
        .evidence
        .iter()
        .take(MAX_EVIDENCE_PER_ITEM)
        .map(|e| truncate(&format!("{}: {}", e.kind, e.summary), MAX_EVIDENCE_SUMMARY))
        .collect();
    let files: Vec<String> = item.files.iter().take(MAX_FILES_PER_ITEM).cloned().collect();
    let symbols: Vec<String> = item
        .symbols
        .iter()
        .take(MAX_SYMBOLS_PER_ITEM)
        .cloned()
        .collect();
    let budgeted = title.chars().count()
        + why.chars().count()
        + evidence.iter().map(|e| e.chars().count()).sum::<usize>()
        + files.iter().map(|f| f.chars().count()).sum::<usize>()
        + symbols.iter().map(|s| s.chars().count()).sum::<usize>()
        + item.id.chars().count()
        + 32;
    EscalationItem {
        id: item.id.clone(),
        title,
        category: item.category,
        severity: item.severity,
        priority: item.priority,
        confidence: item.confidence,
        files,
        symbols,
        evidence,
        why,
        approx_tokens: budgeted.div_ceil(4) as u32,
    }
}

/// Build the escalation package for a reviewed ChangeSet. Pure function:
/// no I/O, no network, deterministic.
pub fn build_package(cs: &ChangeSet, floor: Severity, max_items: usize) -> EscalationPackage {
    let max_items = max_items.clamp(1, MAX_ITEMS_LIMIT);
    let items: Vec<EscalationItem> = candidates(cs, floor)
        .into_iter()
        .take(max_items)
        .map(compact_item)
        .collect();
    let task = cs.task_check.as_ref().map(|t| EscalationTask {
        text: truncate(&t.task_text, MAX_WHY),
        verdict: t.verdict,
        unmatched: t.unmatched.clone(),
    });
    let body: u32 = items.iter().map(|i| i.approx_tokens).sum();
    let envelope = approx_tokens(&format!(
        "{} {} {:?} {} {} {}",
        cs.base_ref,
        cs.head_ref,
        floor,
        cs.stats.files_changed,
        cs.stats.added_lines,
        cs.stats.deleted_lines
    )) + task
        .as_ref()
        .map(|t| approx_tokens(&t.text) + t.unmatched.iter().map(|u| approx_tokens(u)).sum::<u32>())
        .unwrap_or(0);
    EscalationPackage {
        schema_version: ESCALATION_SCHEMA_VERSION,
        base_ref: cs.base_ref.clone(),
        head_ref: cs.head_ref.clone(),
        floor,
        files_changed: cs.stats.files_changed,
        added_lines: cs.stats.added_lines,
        deleted_lines: cs.stats.deleted_lines,
        task,
        items,
        approx_tokens: body + envelope,
    }
}

/// Render the human-readable escalation summary for `--overview`.
/// Pure formatting; mirrors the JSON package one-to-one.
pub fn render_summary(pkg: &EscalationPackage) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "\nEscalation package — {} item(s), ~{} tokens, floor {:?} (schema v{})\n",
        pkg.items.len(),
        pkg.approx_tokens,
        pkg.floor,
        pkg.schema_version
    ));
    if let Some(t) = pkg.task.as_ref() {
        let gaps = if t.unmatched.is_empty() {
            "none".to_string()
        } else {
            t.unmatched.join(", ")
        };
        s.push_str(&format!(
            "  task [{:?}], gaps: {}\n",
            t.verdict, gaps
        ));
    }
    if pkg.items.is_empty() {
        s.push_str("  no items at or above the floor — nothing to escalate.\n");
        return s;
    }
    for (i, item) in pkg.items.iter().enumerate() {
        s.push_str(&format!(
            "{}. [{:?}] {} (~{} tokens)\n",
            i + 1,
            item.severity,
            item.title,
            item.approx_tokens
        ));
        if !item.why.is_empty() {
            s.push_str(&format!("    {}\n", single_line(&item.why)));
        }
        if !item.files.is_empty() {
            s.push_str(&format!("    files: {}\n", item.files.join(", ")));
        }
    }
    s.push_str("  pipe the JSON package to your model: rift --escalate --json\n");
    s
}

fn single_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{Category, ChangeStats, Evidence, ReviewItem, Severity, TaskCheck, TaskVerdict};

    fn item(id: &str, severity: Severity, category: Category) -> ReviewItem {
        ReviewItem {
            id: id.into(),
            title: format!("{id} title with some detail"),
            category,
            severity,
            priority: 50,
            confidence: 0.8,
            files: vec!["src/a.rs".into()],
            symbols: vec!["run".into()],
            evidence: vec![Evidence::new("timeout-change", "timeout 900 -> 86400", "src/a.rs")],
            why: "because reasons".into(),
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
            stats: ChangeStats {
                files_changed: 1,
                added_lines: 5,
                deleted_lines: 2,
                ..Default::default()
            },
            task_check: None,
        }
    }

    #[test]
    fn selects_floor_and_skips_mechanical() {
        let cs = cs_with(vec![
            item("crit", Severity::Critical, Category::Auth),
            item("high", Severity::High, Category::Behavior),
            item("med", Severity::Medium, Category::Behavior),
            item("mech", Severity::High, Category::Mechanical),
        ]);
        let pkg = build_package(&cs, Severity::High, 10);
        let ids: Vec<&str> = pkg.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["crit", "high"], "{ids:?}");
        assert_eq!(pkg.floor, Severity::High);
        assert_eq!(pkg.schema_version, ESCALATION_SCHEMA_VERSION);
    }

    #[test]
    fn floor_medium_admits_more() {
        let cs = cs_with(vec![
            item("high", Severity::High, Category::Behavior),
            item("med", Severity::Medium, Category::Behavior),
            item("low", Severity::Low, Category::Docs),
        ]);
        let pkg = build_package(&cs, Severity::Medium, 10);
        assert_eq!(pkg.items.len(), 2);
    }

    #[test]
    fn caps_items_deterministically() {
        let items: Vec<ReviewItem> = (0..15)
            .map(|n| item(&format!("i{n:02}"), Severity::High, Category::Behavior))
            .collect();
        let a = build_package(&cs_with(items.clone()), Severity::High, 5);
        let b = build_package(&cs_with(items), Severity::High, 5);
        assert_eq!(a.items.len(), 5);
        assert_eq!(
            a.items.iter().map(|i| &i.id).collect::<Vec<_>>(),
            b.items.iter().map(|i| &i.id).collect::<Vec<_>>(),
            "deterministic"
        );
    }

    #[test]
    fn truncates_long_fields_and_estimates_tokens() {
        let mut it = item("big", Severity::Critical, Category::Security);
        it.title = "t".repeat(500);
        it.why = "w".repeat(2000);
        it.evidence = (0..20)
            .map(|n| Evidence::new("k", &format!("{n} {}", "e".repeat(500)), "f.rs"))
            .collect();
        let pkg = build_package(&cs_with(vec![it]), Severity::High, 10);
        let e = &pkg.items[0];
        assert!(e.title.chars().count() <= MAX_TITLE + 1, "{}", e.title.len());
        assert!(e.why.chars().count() <= MAX_WHY + 1);
        assert_eq!(e.evidence.len(), MAX_EVIDENCE_PER_ITEM);
        assert!(e.evidence.iter().all(|s| s.chars().count() <= MAX_EVIDENCE_SUMMARY + 1));
        assert!(e.approx_tokens > 0);
        assert_eq!(pkg.approx_tokens, e.approx_tokens + pkg.approx_tokens - e.approx_tokens);
    }

    #[test]
    fn carries_task_gaps_when_checked() {
        let mut cs = cs_with(vec![item("high", Severity::High, Category::Behavior)]);
        cs.task_check = Some(TaskCheck {
            task_text: "session timeout invoicing".into(),
            terms: vec!["session".into(), "timeout".into(), "invoicing".into()],
            matched: vec![],
            unmatched: vec!["invoicing".into()],
            item_hits: vec![],
            coverage: 0.66,
            verdict: TaskVerdict::Partial,
        });
        let pkg = build_package(&cs, Severity::High, 10);
        let t = pkg.task.expect("task context");
        assert_eq!(t.verdict, TaskVerdict::Partial);
        assert_eq!(t.unmatched, vec!["invoicing"]);
    }

    #[test]
    fn empty_package_reports_nothing_to_escalate() {
        let cs = cs_with(vec![item("low", Severity::Low, Category::Docs)]);
        let pkg = build_package(&cs, Severity::High, 10);
        assert!(pkg.items.is_empty());
        assert!(pkg.task.is_none());
        let out = render_summary(&pkg);
        assert!(out.contains("nothing to escalate"), "{out}");
    }

    #[test]
    fn summary_lists_items_and_task_gaps() {
        let mut cs = cs_with(vec![item("high", Severity::High, Category::Auth)]);
        cs.task_check = Some(TaskCheck {
            task_text: "x".into(),
            terms: vec![],
            matched: vec![],
            unmatched: vec!["zebra".into()],
            item_hits: vec![],
            coverage: 0.0,
            verdict: TaskVerdict::Uncovered,
        });
        let pkg = build_package(&cs, Severity::High, 10);
        let out = render_summary(&pkg);
        assert!(out.contains("1 item(s)"), "{out}");
        assert!(out.contains("zebra"), "{out}");
        assert!(out.contains("--escalate --json"), "{out}");
    }

    #[test]
    fn package_serializes_to_compact_json() {
        let cs = cs_with(vec![item("high", Severity::High, Category::Auth)]);
        let pkg = build_package(&cs, Severity::High, 10);
        let v = serde_json::to_value(&pkg).expect("json");
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["items"][0]["id"], "high");
        assert!(v["items"][0].get("approx_tokens").is_some());
    }
}
