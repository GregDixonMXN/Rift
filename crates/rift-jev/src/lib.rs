//! Opt-in Jev (TypeSafe System One) enrichment for reviews.
//!
//! Design (see `docs/ARCHITECTURE.md`): Jev sits *above* the deterministic
//! model. State is built ONLY from structured review facts — item titles,
//! categories, symbol names, signatures, evidence summaries, line counts.
//! File contents and diffs never leave the machine (the function
//! signatures take `&ReviewItem`, which has no source field), so bulk
//! source cannot leak by accident. Names and short signatures do travel —
//! a severity judge cannot work on redacted rectangles — but never the
//! code they came from. One HTTP call per review (fan-out): a Noul risk
//! question and a Score severity question per behavior-grade item.
//!
//! Uncalibrated by design: answers land as `jev-risk` / `jev-severity`
//! evidence. Nothing gates, fails, or reorders on them until thresholds
//! are calibrated on labeled data.

use anyhow::{Context, Result};
use rift_core::{Category, ReviewItem};
use serde_json::{json, Map, Value};

pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const MODEL: &str = "jev-latest";

/// Max items per call: bounds tokens, latency, and cost.
pub const MAX_ITEMS: usize = 25;

/// Severity rubric levels (Score criteria). Concrete situations, ordered.
const SEVERITY_LEVELS: [&str; 3] = [
    "Cosmetic, generated-adjacent, or test-only impact",
    "Minor feature impact, easily reverted",
    "Auth/security, data-loss, or migration impact",
];

/// Items worth a judgment: everything except collapsed mechanical output,
/// riskiest first so the MAX_ITEMS cap drops the tamest items, not
/// whatever the grouping happened to emit first.
fn judged(items: &[ReviewItem]) -> Vec<&ReviewItem> {
    let mut v: Vec<&ReviewItem> = items
        .iter()
        .filter(|i| !matches!(i.category, Category::Mechanical))
        .collect();
    v.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.priority.cmp(&a.priority))
            .then(a.id.cmp(&b.id))
    });
    v.truncate(MAX_ITEMS);
    v
}

/// Sendable facts for one item. Titles, categories, symbol names, evidence
/// summaries, counts — no source, no diffs, no file contents.
pub fn item_state(item: &ReviewItem) -> Value {
    json!({
        "id": item.id,
        "title": item.title,
        "category": format!("{:?}", item.category),
        "severity": format!("{:?}", item.severity),
        "priority": item.priority,
        "confidence": item.confidence,
        "files": item.files,
        "symbols": item.symbols,
        "evidence": item.evidence.iter().map(|e| &e.summary).collect::<Vec<_>>(),
        "why": item.why,
    })
}

/// One request body for the whole review: repo context as state, per-item
/// judgments as questions (instructions carry each item's facts, since
/// question IDs are not sent to the model).
pub fn build_request(base_ref: &str, head_ref: &str, items: &[ReviewItem]) -> Value {
    let targets = judged(items);
    let mut questions = Map::new();
    for item in &targets {
        let facts = item_state(item);
        questions.insert(
            format!("{}::risk", item.id),
            json!({
                "type": "noul",
                "instructions": {
                    "change": facts,
                    "question": "Does this change carry behavioral risk deserving human review?",
                },
                "criteria": {
                    "true": "Logic change on a sensitive surface, or uncovered by tests",
                    "false": "Mechanical, generated-adjacent, or fully covered change",
                },
            }),
        );
        questions.insert(
            format!("{}::severity", item.id),
            json!({
                "type": "score",
                "instructions": {
                    "change": facts,
                    "question": "How severe would a bug in this change be?",
                },
                "criteria": SEVERITY_LEVELS,
            }),
        );
    }
    json!({
        "model": MODEL,
        "state": {
            "base": base_ref,
            "head": head_ref,
            "items_under_review": targets.len(),
        },
        "questions": questions,
    })
}

fn num(answer: &Map<String, Value>, key: &str) -> Option<f64> {
    answer.get(key)?.as_f64()
}

/// Fold one response's answers into item evidence. Unknown or malformed
/// answers are ignored — deterministic output never depends on Jev.
pub fn apply_answers(items: &mut [ReviewItem], answers: &Map<String, Value>) {
    for item in items.iter_mut() {
        let risk_key = format!("{}::risk", item.id);
        let sev_key = format!("{}::severity", item.id);
        if let Some(Value::Object(a)) = answers.get(&risk_key) {
            if a.get("type").and_then(|t| t.as_str()) == Some("noul") {
                if let Some(p) = num(a, "noul") {
                    item.evidence.push(rift_core::Evidence::new(
                        "jev-risk",
                        &format!("Jev P(risky)={p:.2}"),
                        item.files.first().map(String::as_str).unwrap_or(""),
                    ));
                }
            }
        }
        if let Some(Value::Object(a)) = answers.get(&sev_key) {
            if a.get("type").and_then(|t| t.as_str()) == Some("score") {
                if let (Some(s), Some(c)) = (num(a, "score"), num(a, "confidence")) {
                    let leg = s.round().clamp(0.0, 2.0) as usize;
                    item.evidence.push(rift_core::Evidence::new(
                        "jev-severity",
                        &format!(
                            "Jev severity {s:.1} ({}, confidence {c:.2})",
                            SEVERITY_LEVELS[leg]
                        ),
                        item.files.first().map(String::as_str).unwrap_or(""),
                    ));
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum JevStatus {
    /// Judgments applied to `n` items.
    Applied { items: usize },
    /// No API key: review proceeds unchanged.
    SkippedNoKey,
    /// Nothing worth judging: review proceeds unchanged.
    SkippedNoItems,
    /// Call failed: review proceeds unchanged, warning already noted.
    Failed(String),
}

/// One judgment source behind `rift --jev`.
///
/// The cloud judge calls the TypeSafe API; the deterministic adapter maps
/// the same structured facts to `jev-risk` / `jev-severity` evidence with
/// no network. Both write the same evidence kinds so the UI and
/// `--overview` render either without branching — summaries say which
/// source produced them.
pub trait Judge {
    fn name(&self) -> &'static str;
    fn judge(&self, base_ref: &str, head_ref: &str, items: &mut [ReviewItem]) -> JevStatus;
}

/// Cloud judge: one HTTP call per review (fan-out). Holds the API key so
/// call sites never handle secrets directly.
pub struct CloudJudge {
    key: String,
}

impl CloudJudge {
    pub fn new(key: &str) -> Option<Self> {
        let key = key.trim().to_string();
        if key.is_empty() {
            None
        } else {
            Some(Self { key })
        }
    }
}

impl Judge for CloudJudge {
    fn name(&self) -> &'static str {
        "cloud"
    }

    fn judge(&self, base_ref: &str, head_ref: &str, items: &mut [ReviewItem]) -> JevStatus {
        let targets = judged(items);
        if targets.is_empty() {
            return JevStatus::SkippedNoItems;
        }
        let body = build_request(base_ref, head_ref, items);
        let resp = match post(&body, &self.key) {
            Ok(r) => r,
            Err(e) => return JevStatus::Failed(format!("{e:#}")),
        };
        let answers = resp
            .get("answers")
            .and_then(|a| a.as_object())
            .cloned()
            .unwrap_or_default();
        // Count answers actually applied, not questions asked: an empty
        // or malformed response judges nothing.
        let n = targets
            .iter()
            .filter(|t| {
                answers.contains_key(&format!("{}::risk", t.id))
                    || answers.contains_key(&format!("{}::severity", t.id))
            })
            .count();
        apply_answers(items, &answers);
        JevStatus::Applied { items: n }
    }
}

/// Deterministic adapter: pure function of the facts the cloud judge would
/// receive (category, severity, priority, evidence kinds). No network, no
/// key, stable across runs — the offline baseline and the CI stand-in.
///
/// Risk model (documented, not tuned): starts at 0.15, +0.25 for
/// auth/security/schema/migration surfaces, +0.20 when no covering tests
/// are recorded, +0.10 per `validation-removed` / `secret-handling` /
/// `unsafe` signal (capped at 0.95). Severity mirrors the deterministic
/// `Severity` (Low→0.0, Medium→1.0, High/Critical→2.0) at fixed 0.90
/// confidence. Nothing gates on these numbers.
pub struct DeterministicJudge;

impl Judge for DeterministicJudge {
    fn name(&self) -> &'static str {
        "deterministic"
    }

    fn judge(&self, _base_ref: &str, _head_ref: &str, items: &mut [ReviewItem]) -> JevStatus {
        // Filter by the same rule as `judged` (non-mechanical), not by
        // id match: duplicate ids must not double-apply, and a mechanical
        // item sharing an id must not gain judgments. Cap identically.
        let mut targets: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, i)| !matches!(i.category, Category::Mechanical))
            .map(|(n, _)| n)
            .collect();
        targets.sort_by(|&a, &b| {
            items[b]
                .severity
                .cmp(&items[a].severity)
                .then(items[b].priority.cmp(&items[a].priority))
                .then(items[a].id.cmp(&items[b].id))
        });
        targets.truncate(MAX_ITEMS);
        if targets.is_empty() {
            return JevStatus::SkippedNoItems;
        }
        let judged_count = targets.len();
        for n in targets {
            let item = &mut items[n];
            let mut risk: f64 = 0.15;
            if matches!(
                item.category,
                Category::Auth
                    | Category::Security
                    | Category::Schema
                    | Category::Migration
                    | Category::ApiBreak
            ) {
                risk += 0.25;
            }
            let untested = item.evidence.iter().any(|e| e.kind == "untested-change");
            if untested {
                risk += 0.20;
            }
            for kind in ["validation-removed", "secret-handling", "unsafe"] {
                if item.evidence.iter().any(|e| e.kind == kind) {
                    risk += 0.10;
                }
            }
            let risk = risk.clamp(0.0, 0.95);
            let file = item.files.first().cloned().unwrap_or_default();
            item.evidence.push(rift_core::Evidence::new(
                "jev-risk",
                &format!("Local P(risky)={risk:.2} (deterministic, no cloud call)"),
                &file,
            ));
            let (score, label) = match item.severity {
                rift_core::Severity::Low => (0.0, SEVERITY_LEVELS[0]),
                rift_core::Severity::Medium => (1.0, SEVERITY_LEVELS[1]),
                rift_core::Severity::High | rift_core::Severity::Critical => {
                    (2.0, SEVERITY_LEVELS[2])
                }
            };
            item.evidence.push(rift_core::Evidence::new(
                "jev-severity",
                &format!("Local severity {score:.1} ({label}, confidence 0.90)"),
                &file,
            ));
        }
        JevStatus::Applied {
            items: judged_count,
        }
    }
}

/// Enrich items with Jev judgments. `key=None` (or empty judged set)
/// skips silently; failures never fail the review.
pub fn enrich(
    base_ref: &str,
    head_ref: &str,
    items: &mut [ReviewItem],
    key: Option<&str>,
) -> JevStatus {
    let Some(judge) = key.and_then(CloudJudge::new) else {
        return JevStatus::SkippedNoKey;
    };
    judge.judge(base_ref, head_ref, items)
}

fn post(body: &Value, key: &str) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;
    let resp = client
        .post(ENDPOINT)
        .bearer_auth(key)
        .json(body)
        .send()
        .context("calling TypeSafe API")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        anyhow::bail!("TypeSafe API {status}: {text}");
    }
    resp.json::<Value>().context("parsing TypeSafe response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{Evidence, Severity};

    fn item(id: &str, category: Category) -> ReviewItem {
        ReviewItem {
            id: id.into(),
            title: format!("{id} changed"),
            category,
            severity: Severity::Medium,
            priority: 60,
            confidence: 0.8,
            files: vec!["src/auth.rs".into()],
            symbols: vec!["login".into()],
            evidence: vec![Evidence::new("auth-surface", "auth surface", "src/auth.rs")],
            why: "behavior change".into(),
        }
    }

    #[test]
    fn state_has_facts_no_source_fields() {
        let v = item_state(&item("file:src/auth.rs", Category::Behavior));
        let s = serde_json::to_string(&v).unwrap();
        for key in ["content", "diff", "hunk", "lines\""] {
            assert!(!s.contains(key), "leak of {key} in {s}");
        }
        assert!(s.contains("login"));
        assert!(s.contains("auth surface"));
    }

    #[test]
    fn judged_prefers_riskiest_within_cap() {
        let mut items = vec![item("file:mech.rs", Category::Mechanical)];
        for i in 0..30 {
            let mut it = item(&format!("file:m{i}.rs"), Category::Behavior);
            it.severity = Severity::Low;
            it.priority = 10;
            items.push(it);
        }
        let mut hot = item("file:hot.rs", Category::Auth);
        hot.severity = Severity::Critical;
        items.push(hot);
        let req = build_request("HEAD", "work", &items);
        let q = req["questions"].as_object().unwrap();
        assert_eq!(q.len(), MAX_ITEMS * 2);
        assert!(
            q.contains_key("file:hot.rs::risk"),
            "critical survives the cap"
        );
    }

    #[test]
    fn deterministic_judge_ignores_mechanical_despite_id() {
        // A mechanical item must not gain judgments even if its id
        // collides with nothing; and duplicate ids apply once each.
        let mut mech = item("file:a.rs", Category::Mechanical);
        mech.severity = Severity::High;
        let mut items = vec![mech, item("file:b.rs", Category::Behavior)];
        let status = DeterministicJudge.judge("H", "w", &mut items);
        assert!(
            matches!(status, JevStatus::Applied { items: 1 }),
            "{status:?}"
        );
        assert!(
            !items[0].evidence.iter().any(|e| e.kind.starts_with("jev-")),
            "mechanical untouched: {:?}",
            items[0].evidence
        );
        assert!(items[1].evidence.iter().any(|e| e.kind.starts_with("jev-")));
    }

    #[test]
    fn questions_skip_mechanical_and_cap() {
        let mut items = vec![item("file:a.rs", Category::Mechanical)];
        for i in 0..30 {
            items.push(item(&format!("file:m{i}.rs"), Category::Behavior));
        }
        let req = build_request("HEAD", "work", &items);
        let q = req["questions"].as_object().unwrap();
        assert!(!q.keys().any(|k| k.starts_with("file:a.rs")));
        // 25 items × 2 questions.
        assert_eq!(q.len(), MAX_ITEMS * 2);
    }

    #[test]
    fn answers_become_evidence() {
        let mut items = vec![item("file:src/auth.rs", Category::Behavior)];
        let answers: Map<String, Value> = serde_json::from_value(json!({
            "file:src/auth.rs::risk": {"type": "noul", "noul": 0.72},
            "file:src/auth.rs::severity": {
                "type": "score", "score": 2.0, "confidence": 1.0,
                "legend": {"0": "x", "1": "y", "2": "z"},
            },
        }))
        .unwrap();
        apply_answers(&mut items, &answers);
        let kinds: Vec<&str> = items[0].evidence.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"jev-risk"), "{kinds:?}");
        assert!(kinds.contains(&"jev-severity"), "{kinds:?}");
        let risk = items[0]
            .evidence
            .iter()
            .find(|e| e.kind == "jev-risk")
            .unwrap();
        assert!(risk.summary.contains("0.72"), "{}", risk.summary);
    }

    #[test]
    fn malformed_answers_are_ignored() {
        let mut items = vec![item("file:src/auth.rs", Category::Behavior)];
        let answers: Map<String, Value> = serde_json::from_value(json!({
            "file:src/auth.rs::risk": {"type": "noul"},
            "file:src/auth.rs::severity": {"type": "score", "score": "high"},
            "other::risk": {"type": "noul", "noul": 0.5},
        }))
        .unwrap();
        apply_answers(&mut items, &answers);
        assert_eq!(items[0].evidence.len(), 1);
    }

    #[test]
    fn no_key_skips_without_touching() {
        let mut items = vec![item("file:src/auth.rs", Category::Behavior)];
        assert!(matches!(
            enrich("H", "w", &mut items, None),
            JevStatus::SkippedNoKey
        ));
        assert!(matches!(
            enrich("H", "w", &mut items, Some("")),
            JevStatus::SkippedNoKey
        ));
        assert_eq!(items[0].evidence.len(), 1);
    }

    #[test]
    fn no_judgable_items_skips() {
        let mut items = vec![item("file:a.rs", Category::Mechanical)];
        assert!(matches!(
            enrich("H", "w", &mut items, Some("key")),
            JevStatus::SkippedNoItems
        ));
    }

    #[test]
    fn deterministic_judge_needs_no_key() {
        let judge = DeterministicJudge;
        assert_eq!(judge.name(), "deterministic");
        let mut items = vec![
            item("file:src/auth.rs", Category::Auth),
            item("file:gen.rs", Category::Mechanical),
        ];
        let status = judge.judge("H", "w", &mut items);
        assert!(
            matches!(status, JevStatus::Applied { items: 1 }),
            "{status:?}"
        );
        let auth = &items[0];
        assert!(
            auth.evidence.iter().any(|e| e.kind == "jev-risk"),
            "{auth:?}"
        );
        assert!(
            auth.evidence.iter().any(|e| e.kind == "jev-severity"),
            "{auth:?}"
        );
        assert!(
            items[1]
                .evidence
                .iter()
                .all(|e| !e.kind.starts_with("jev-")),
            "mechanical item must gain no jev evidence: {items:?}"
        );
    }

    #[test]
    fn deterministic_judge_is_stable() {
        let run = || {
            let mut items = vec![item("file:src/auth.rs", Category::Auth)];
            DeterministicJudge.judge("H", "w", &mut items);
            items
                .into_iter()
                .flat_map(|i| {
                    i.evidence
                        .into_iter()
                        .filter(|e| e.kind.starts_with("jev-"))
                        .map(|e| e.summary)
                })
                .collect::<Vec<_>>()
        };
        let first = run();
        assert_eq!(first.len(), 2);
        assert_eq!(first, run());
        assert!(first.iter().all(|s| s.contains("Local")));
    }

    #[test]
    fn deterministic_judge_dispatches_as_trait_object() {
        let judge: Box<dyn Judge> = Box::new(DeterministicJudge);
        let mut items = vec![item("file:src/auth.rs", Category::Behavior)];
        assert!(matches!(
            judge.judge("H", "w", &mut items),
            JevStatus::Applied { items: 1 }
        ));
    }

    #[test]
    fn cloud_judge_rejects_empty_key() {
        assert!(CloudJudge::new("").is_none());
        assert!(CloudJudge::new("  ").is_none());
        assert!(CloudJudge::new("k").is_some());
    }
}
