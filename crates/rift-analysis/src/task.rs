//! Task-vs-change verification: does the diff cover the described task?
//!
//! Deterministic and local-first, like the rest of `rift-analysis`:
//! tokenize the task into content terms, match each term against the
//! structured change corpus (file paths, symbol names, review titles,
//! evidence summaries, added-line tokens), and report coverage with
//! per-item attribution. No LLM, no network, no randomness.

use rift_core::{
    ChangeSet, ReviewItem, TaskCheck, TaskItemHit, TaskTermMatch, TaskVerdict,
};
use std::collections::HashSet;

/// Minimum bigram similarity for a fuzzy term match.
const FUZZY_THRESHOLD: f32 = 0.6;
/// Max review items listed as task hits (bounds overview/JSON size).
const MAX_HITS: usize = 5;

/// Small English stopword set. Task terms that carry no signal
/// ("the", "and", "should", ...) are dropped before matching so a
/// well-written task sentence degrades to its content words.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "to", "in", "on", "for", "with", "by", "at",
    "from", "as", "is", "are", "was", "were", "be", "been", "it", "its", "this", "that",
    "these", "those", "should", "would", "could", "must", "need", "needs", "add", "adds",
    "added", "adding", "fix", "fixes", "fixed", "fixing", "update", "updates", "updated",
    "updating", "make", "makes", "use", "uses", "used", "using", "new", "also", "just",
    "please", "implement",
];

/// Split one identifier into lowercase word parts:
/// `sessionTimeout` -> ["session", "timeout"],
/// `SESSION_TIMEOUT` -> ["session", "timeout"],
/// `rate-limit` -> ["rate", "limit"].
pub fn split_ident(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, parts: &mut Vec<String>| {
        if cur.len() >= 2 {
            parts.push(std::mem::take(cur).to_lowercase());
        } else {
            cur.clear();
        }
    };
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_alphanumeric() {
            let prev_lower_next_upper = i > 0
                && chars[i - 1].is_lowercase()
                && c.is_uppercase();
            let acronym_boundary = i > 0
                && i + 1 < chars.len()
                && chars[i - 1].is_uppercase()
                && c.is_uppercase()
                && chars[i + 1].is_lowercase();
            if prev_lower_next_upper || acronym_boundary {
                flush(&mut cur, &mut parts);
            }
            cur.push(c);
        } else {
            flush(&mut cur, &mut parts);
        }
    }
    flush(&mut cur, &mut parts);
    parts
}

/// Tokenize task text into deduplicated content terms, preserving order.
pub fn task_terms(text: &str) -> Vec<String> {
    let stop: HashSet<&str> = STOPWORDS.iter().copied().collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in split_ident(text) {
        let t = raw.to_lowercase();
        if t.len() < 3 || stop.contains(t.as_str()) || !seen.insert(t.clone()) {
            continue;
        }
        out.push(t);
    }
    out
}

fn bigrams(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    if c.len() < 2 {
        return vec![s.to_string()];
    }
    c.windows(2).map(|w| w.iter().collect()).collect()
}

fn bigram_sim(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let ab = bigrams(a);
    let bb = bigrams(b);
    if ab.is_empty() || bb.is_empty() {
        return 0.0;
    }
    let mut rest = bb.clone();
    let mut hits = 0;
    for g in &ab {
        if let Some(i) = rest.iter().position(|x| x == g) {
            hits += 1;
            rest.remove(i);
        }
    }
    2.0 * hits as f32 / (ab.len() + bb.len()) as f32
}

/// One corpus entry: the normalized token plus a human-readable
/// `matched_via` label pointing at where it came from.
struct CorpusToken {
    token: String,
    via: String,
}

/// Strip `"..."` / `r"..."` literal contents and `//` comments so task
/// terms don't match words that only appear inside a string fixture or
/// comment (same hygiene as the importance scorer: mentioning a word is
/// not using it).
fn strip_code_literals(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == '/' {
            break;
        }
        if c == 'r' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == '#' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == '"' {
                let hashes = j - (i + 1);
                j += 1;
                while j < bytes.len() {
                    if bytes[j] == '"'
                        && bytes[j + 1..].starts_with(&vec!['#'; hashes])
                    {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
        }
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

/// Build the searchable change corpus. Sources, in priority order:
/// symbol names, file paths, review titles/why, evidence summaries,
/// added-diff tokens (literals/comments stripped, so prose doesn't
/// swamp signals).
fn build_corpus(cs: &ChangeSet) -> Vec<CorpusToken> {
    let mut corpus = Vec::new();
    let mut push_tokens = |text: &str, via: String| {
        for t in split_ident(text) {
            corpus.push(CorpusToken {
                token: t,
                via: via.clone(),
            });
        }
    };
    for s in &cs.symbol_changes {
        let via = format!("symbol {}", s.name);
        push_tokens(&s.name, via);
        if let Some(sig) = s.new_signature.as_ref().or(s.old_signature.as_ref()) {
            let via = format!("symbol {}", s.name);
            // Signatures add type/param words ("timeout", "u64") cheaply.
            push_tokens(sig, via);
        }
    }
    for f in &cs.files {
        let p = f.display_path();
        push_tokens(p, format!("file {p}"));
        for h in &f.hunks {
            for l in &h.lines {
                if !matches!(l.kind, rift_core::DiffLineKind::Addition) {
                    continue;
                }
                // Cap per-line work: first 300 chars, literals/comments
                // stripped so fixture strings don't fake coverage.
                let text: String = l.text.chars().take(300).collect();
                push_tokens(&strip_code_literals(&text), format!("diff {}", p));
            }
        }
    }
    for r in &cs.review_items {
        push_tokens(&r.title, format!("review {}", r.id));
        push_tokens(&r.why, format!("review {}", r.id));
        for e in &r.evidence {
            push_tokens(&e.summary, format!("review {}", r.id));
        }
    }
    corpus
}

/// Corpus tokens searchable for one review item (for per-item hits).
fn item_corpus(item: &ReviewItem) -> Vec<String> {
    let mut toks = Vec::new();
    for src in [&item.title, &item.why] {
        toks.extend(split_ident(src));
    }
    for f in &item.files {
        toks.extend(split_ident(f));
    }
    for s in &item.symbols {
        toks.extend(split_ident(s));
    }
    for e in &item.evidence {
        toks.extend(split_ident(&e.summary));
    }
    toks
}

fn match_score(term: &str, token: &str) -> f32 {
    if term == token {
        1.0
    } else if (token.contains(term) || term.contains(token))
        // Two-letter tokens (`in`, `of`, `to` from `for x in ...`) are
        // substrings of almost everything; require both sides to carry
        // real signal before calling it a stem match. Short terms still
        // match exactly or via fuzzy bigrams below.
        && term.len() >= 4
        && token.len() >= 4
    {
        0.8
    } else {
        let s = bigram_sim(term, token);
        if s >= FUZZY_THRESHOLD { s } else { 0.0 }
    }
}

/// Match every task term against the corpus; returns (matched, unmatched).
fn match_terms(terms: &[String], corpus: &[CorpusToken]) -> (Vec<TaskTermMatch>, Vec<String>) {
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    for term in terms {
        let mut best: Option<(&CorpusToken, f32)> = None;
        for tok in corpus {
            let s = match_score(term, &tok.token);
            if s > 0.0 && best.map(|(_, b)| s > b).unwrap_or(true) {
                best = Some((tok, s));
                if (s - 1.0).abs() < f32::EPSILON {
                    break;
                }
            }
        }
        match best {
            Some((tok, score)) => matched.push(TaskTermMatch {
                term: term.clone(),
                matched_via: tok.via.clone(),
                score,
            }),
            None => unmatched.push(term.clone()),
        }
    }
    (matched, unmatched)
}

/// Deterministic task-vs-change check. Pure function of the ChangeSet.
pub fn check_task(cs: &ChangeSet, task_text: &str) -> TaskCheck {
    let terms = task_terms(task_text);
    if terms.is_empty() {
        return TaskCheck {
            task_text: task_text.to_string(),
            terms: vec![],
            matched: vec![],
            unmatched: vec![],
            item_hits: vec![],
            coverage: 0.0,
            verdict: TaskVerdict::Uncovered,
        };
    }
    let corpus = build_corpus(cs);
    let (matched, unmatched) = match_terms(&terms, &corpus);
    let coverage = matched.len() as f32 / terms.len() as f32;

    // Per-item attribution: which review items touch which task terms.
    let matched_set: HashSet<&str> = matched.iter().map(|m| m.term.as_str()).collect();
    let mut hits: Vec<TaskItemHit> = Vec::new();
    for item in &cs.review_items {
        let toks = item_corpus(item);
        let mut hit_terms: Vec<String> = Vec::new();
        for term in &terms {
            if !matched_set.contains(term.as_str()) {
                continue;
            }
            let mut best = 0.0f32;
            for tok in &toks {
                best = best.max(match_score(term, tok));
                if (best - 1.0).abs() < f32::EPSILON {
                    break;
                }
            }
            if best > 0.0 {
                hit_terms.push(term.clone());
            }
        }
        if !hit_terms.is_empty() {
            hits.push(TaskItemHit {
                item_id: item.id.clone(),
                title: item.title.clone(),
                score: hit_terms.len() as f32 / terms.len() as f32,
                matched_terms: hit_terms,
            });
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.matched_terms.len().cmp(&a.matched_terms.len()))
            .then(a.title.cmp(&b.title))
    });
    hits.truncate(MAX_HITS);

    // Covered needs a strong majority (>=4 of 5); a single missing term
    // out of three is Partial, not Covered.
    let verdict = if coverage >= 0.8 {
        TaskVerdict::Covered
    } else if coverage >= 0.34 {
        TaskVerdict::Partial
    } else {
        TaskVerdict::Uncovered
    };
    TaskCheck {
        task_text: task_text.to_string(),
        terms,
        matched,
        unmatched,
        item_hits: hits,
        coverage,
        verdict,
    }
}

/// Render the task-check section of `--overview`. Pure formatting.
pub fn render_task_check(check: &TaskCheck) -> String {
    let mut s = String::new();
    let pct = (check.coverage * 100.0).round() as u32;
    s.push_str(&format!(
        "\nTask check [{:?}] — {}% ({} of {} terms)\n",
        check.verdict,
        pct,
        check.matched.len(),
        check.terms.len()
    ));
    s.push_str(&format!("  task: {}\n", single_line(&check.task_text)));
    if !check.matched.is_empty() {
        let ms: Vec<String> = check
            .matched
            .iter()
            .map(|m| format!("{} (via {})", m.term, m.matched_via))
            .collect();
        s.push_str(&format!("  matched: {}\n", ms.join(", ")));
    }
    if !check.unmatched.is_empty() {
        s.push_str(&format!("  missing: {}\n", check.unmatched.join(", ")));
    }
    if check.terms.is_empty() {
        s.push_str("  no content terms in task description.\n");
    }
    for h in &check.item_hits {
        s.push_str(&format!(
            "  - {} [{}]\n",
            h.title,
            h.matched_terms.join(", ")
        ));
    }
    // Deterministic guidance, not judgment: what to do next.
    match check.verdict {
        TaskVerdict::Covered => {}
        TaskVerdict::Partial => {
            s.push_str("  hint: some task terms have no matching change — check scope or wording.\n");
        }
        TaskVerdict::Uncovered => {
            s.push_str("  hint: no task term matches this diff — wrong branch, or work not started.\n");
        }
    }
    s
}

fn single_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{
        Category, ChangeSet, ChangeStats, Evidence, FileChange, FileStatus, Hunk, Language,
        ReviewItem, Severity, SymbolChange, SymbolChangeKind, SymbolKind,
    };

    #[test]
    fn terms_drop_stopwords_and_dedup() {
        let t = task_terms("Please fix the login timeout handling for login");
        assert!(t.contains(&"login".to_string()), "{t:?}");
        assert!(t.contains(&"timeout".to_string()), "{t:?}");
        assert!(t.contains(&"handling".to_string()), "{t:?}");
        assert!(!t.contains(&"please".to_string()), "{t:?}");
        assert!(!t.contains(&"the".to_string()), "{t:?}");
        assert_eq!(t.iter().filter(|x| *x == "login").count(), 1, "{t:?}");
    }

    #[test]
    fn split_ident_handles_camel_snake_kebab() {
        assert_eq!(split_ident("sessionTimeout"), vec!["session", "timeout"]);
        assert_eq!(split_ident("SESSION_TIMEOUT"), vec!["session", "timeout"]);
        assert_eq!(split_ident("rate-limit"), vec!["rate", "limit"]);
    }

    fn harness() -> ChangeSet {
        let syms = vec![SymbolChange {
            file: "src/auth/session.rs".into(),
            name: "session_timeout".into(),
            kind: SymbolKind::Function,
            change: SymbolChangeKind::Modified,
            confidence: 0.9,
            old_signature: None,
            new_signature: Some("pub fn session_timeout() -> u64".into()),
            old_lines: Some((1, 5)),
            new_lines: Some((1, 6)),
            evidence: vec![],
        }];
        let items = vec![ReviewItem {
            id: "auth-1".into(),
            title: "Auth session timeout extended".into(),
            category: Category::Auth,
            severity: Severity::High,
            priority: 70,
            confidence: 0.9,
            files: vec!["src/auth/session.rs".into()],
            symbols: vec!["session_timeout".into()],
            evidence: vec![Evidence::new("timeout-change", "timeout 900 -> 86400", "src/auth/session.rs")],
            why: "Session timeout constant changed.".into(),
        }];
        ChangeSet {
            repo_root: ".".into(),
            base_ref: "HEAD".into(),
            head_ref: "worktree".into(),
            files: vec![FileChange {
                old_path: "src/auth/session.rs".into(),
                new_path: "src/auth/session.rs".into(),
                status: FileStatus::Modified,
                language: Language::Rust,
                is_binary: false,
                is_generated: false,
                is_test_file: false,
                added_lines: 1,
                deleted_lines: 1,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    header: String::new(),
                    lines: vec![],
                }],
                old_content: None,
                new_content: None,
            }],
            symbol_changes: syms,
            review_items: items,
            stats: ChangeStats::default(),
            task_check: None,
        }
    }

    #[test]
    fn covered_when_terms_match_symbols() {
        let cs = harness();
        let c = check_task(&cs, "extend session timeout");
        assert_eq!(c.verdict, TaskVerdict::Covered, "{c:?}");
        assert!(c.unmatched.is_empty(), "{c:?}");
        assert_eq!(c.item_hits.len(), 1);
        assert_eq!(c.item_hits[0].item_id, "auth-1");
    }

    #[test]
    fn uncovered_when_nothing_matches() {
        let cs = harness();
        let c = check_task(&cs, "rewrite payment invoicing zebra");
        assert_eq!(c.verdict, TaskVerdict::Uncovered, "{c:?}");
        assert!(c.matched.is_empty(), "{c:?}");
        assert!(c.item_hits.is_empty(), "{c:?}");
    }

    #[test]
    fn partial_reports_missing_terms() {
        let cs = harness();
        let c = check_task(&cs, "session timeout invoicing");
        assert_eq!(c.verdict, TaskVerdict::Partial, "{c:?}");
        assert!(c.unmatched.contains(&"invoicing".to_string()), "{c:?}");
    }

    #[test]
    fn empty_task_is_uncovered() {
        let cs = harness();
        let c = check_task(&cs, "the and a");
        assert!(c.terms.is_empty());
        assert_eq!(c.verdict, TaskVerdict::Uncovered);
        assert_eq!(c.coverage, 0.0);
    }

    #[test]
    fn fuzzy_matches_typos() {
        let cs = harness();
        let c = check_task(&cs, "sesion timout");
        assert!(
            c.coverage > 0.0,
            "close typos should fuzzy-match: {c:?}"
        );
    }

    #[test]
    fn render_is_stable_and_mentions_verdict() {
        let cs = harness();
        let c = check_task(&cs, "session timeout invoicing");
        let out = render_task_check(&c);
        assert!(out.contains("Partial"), "{out}");
        assert!(out.contains("invoicing"), "{out}");
        assert_eq!(out, render_task_check(&c), "deterministic");
    }

    #[test]
    fn short_tokens_are_not_substring_matches() {
        assert_eq!(super::match_score("invoicing", "in"), 0.0);
        assert_eq!(super::match_score("timeout", "to"), 0.0);
        // Exact and long-stem matches still work.
        assert_eq!(super::match_score("timeout", "timeout"), 1.0);
        assert_eq!(super::match_score("rewrite", "rewrites"), 0.8);
    }

    #[test]
    fn diff_string_literals_do_not_match() {
        use rift_core::{DiffLine, DiffLineKind, FileStatus, Hunk, Language};
        let mut cs = harness();
        cs.files.push(FileChange {
            old_path: "src/fixture.rs".into(),
            new_path: "src/fixture.rs".into(),
            status: FileStatus::Modified,
            language: Language::Rust,
            is_binary: false,
            is_generated: false,
            is_test_file: false,
            added_lines: 1,
            deleted_lines: 0,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                header: String::new(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Addition,
                    text: r#"let s = "invoicing zebra payment"; // mention only"#.into(),
                }],
            }],
            old_content: None,
            new_content: None,
        });
        let c = check_task(&cs, "invoicing zebra");
        assert_eq!(c.verdict, TaskVerdict::Uncovered, "{c:?}");
    }

    #[test]
    fn check_ignores_order_of_items() {
        // Item order must not change term verdicts (hits are sorted).
        let mut cs = harness();
        cs.review_items.reverse();
        let a = check_task(&harness(), "session timeout");
        let b = check_task(&cs, "session timeout");
        assert_eq!(a.verdict, b.verdict);
        assert_eq!(a.coverage, b.coverage);
    }
}
