//! Task-vs-change verification: does the diff cover the described task?
//!
//! Deterministic and local-first, like the rest of `rift-analysis`:
//! tokenize the task into content terms, match each term against the
//! structured change corpus (file paths, symbol names, review titles,
//! evidence summaries, added-line tokens), and report coverage with
//! per-item attribution. No LLM, no network, no randomness.

use rift_core::{ChangeSet, ReviewItem, TaskCheck, TaskItemHit, TaskTermMatch, TaskVerdict};
use std::collections::{HashMap, HashSet};

/// Minimum bigram similarity for a fuzzy term match.
const FUZZY_THRESHOLD: f32 = 0.6;
/// Max review items listed as task hits (bounds overview/JSON size).
const MAX_HITS: usize = 5;

/// Small English stopword set. Task terms that carry no signal
/// ("the", "and", "should", ...) are dropped before matching so a
/// well-written task sentence degrades to its content words.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "but",
    "of",
    "to",
    "in",
    "on",
    "for",
    "with",
    "by",
    "at",
    "from",
    "as",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "it",
    "its",
    "this",
    "that",
    "these",
    "those",
    "should",
    "would",
    "could",
    "must",
    "need",
    "needs",
    "add",
    "adds",
    "added",
    "adding",
    "fix",
    "fixes",
    "fixed",
    "fixing",
    "update",
    "updates",
    "updated",
    "updating",
    "make",
    "makes",
    "use",
    "uses",
    "used",
    "using",
    "new",
    "also",
    "just",
    "please",
    "implement",
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
    // Peekable chars instead of a collected Vec: same boundaries (a
    // separator resets `prev`, which can never satisfy either boundary
    // condition anyway), no per-string allocation.
    let mut prev: Option<char> = None;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c.is_alphanumeric() {
            let next = it.peek().copied();
            let prev_lower_next_upper =
                matches!(prev, Some(p) if p.is_lowercase()) && c.is_uppercase();
            let acronym_boundary = matches!(prev, Some(p) if p.is_uppercase())
                && c.is_uppercase()
                && matches!(next, Some(n) if n.is_lowercase());
            if prev_lower_next_upper || acronym_boundary {
                flush(&mut cur, &mut parts);
            }
            cur.push(c);
            prev = Some(c);
        } else {
            flush(&mut cur, &mut parts);
            prev = None;
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
    // `split_ident` already lowercases; no second pass needed.
    for t in split_ident(text) {
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

/// Bigram multiset with its source length, computed once per unique
/// string and reused across every term × token comparison.
#[derive(Clone)]
struct Bigrams {
    chars: usize,
    grams: Vec<String>,
}

fn bigrams_of(s: &str) -> Bigrams {
    Bigrams {
        chars: s.chars().count(),
        grams: bigrams(s),
    }
}

/// Exact integer bound: Dice = 2·hits/(A+B) ≤ 2·min(A,B)/(A+B), so when
/// that ceiling sits below the fuzzy threshold the pair cannot match and
/// the bigram work is skipped. No floats, no rounding drift — pairs that
/// could reach 0.6 always run the full comparison.
fn dice_possible(a_chars: usize, b_chars: usize) -> bool {
    let (a, b) = (
        a_chars.saturating_sub(1).max(1),
        b_chars.saturating_sub(1).max(1),
    );
    2 * a.min(b) * 10 >= 6 * (a + b)
}

/// Multiset intersection count without cloning either side: one small
/// `used` bitmap instead of a cloned+`remove`d vec per pair. Greedy
/// first-unused matching counts the same multiset intersection either way.
fn dice(a: &Bigrams, b: &Bigrams) -> f32 {
    let mut used = vec![false; b.grams.len()];
    let mut hits = 0;
    for g in &a.grams {
        for (j, x) in b.grams.iter().enumerate() {
            if !used[j] && x == g {
                used[j] = true;
                hits += 1;
                break;
            }
        }
    }
    2.0 * hits as f32 / (a.grams.len() + b.grams.len()) as f32
}

/// One corpus entry: the normalized token plus an index into the shared
/// `vias` table. Storing the label once (not cloned per token) removes
/// the bulk of corpus-build allocation on large diffs.
struct CorpusToken {
    token: String,
    via: u32,
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
///
/// Two build-time economies with zero result change: `via` labels live
/// once in a shared table (not cloned per token), and repeated tokens
/// keep only their first occurrence (matching takes the max over a term,
/// so duplicates never change a score — only the pair count).
fn build_corpus(cs: &ChangeSet) -> (Vec<CorpusToken>, Vec<String>) {
    let mut corpus = Vec::new();
    let mut vias: Vec<String> = Vec::new();
    // Local fns (not closures) so `corpus` stays pushable at call sites.
    fn via_idx(vias: &mut Vec<String>, via: &str) -> u32 {
        match vias.iter().position(|v| v == via) {
            Some(i) => i as u32,
            None => {
                vias.push(via.to_string());
                vias.len() as u32 - 1
            }
        }
    }
    fn push_token(
        corpus: &mut Vec<CorpusToken>,
        seen: &mut HashSet<String>,
        token: String,
        idx: u32,
    ) {
        // `contains` first: `insert` would clone the token even for
        // duplicates, and repeats dominate large diffs.
        if !seen.contains(token.as_str()) {
            seen.insert(token.clone());
            corpus.push(CorpusToken { token, via: idx });
        }
    }
    fn push_tokens(
        corpus: &mut Vec<CorpusToken>,
        vias: &mut Vec<String>,
        seen: &mut HashSet<String>,
        text: &str,
        via: &str,
    ) {
        let idx = via_idx(vias, via);
        for t in split_ident(text) {
            push_token(corpus, seen, t, idx);
        }
    }
    let mut seen = HashSet::new();
    for s in &cs.symbol_changes {
        let via = format!("symbol {}", s.name);
        push_tokens(&mut corpus, &mut vias, &mut seen, &s.name, &via);
        if let Some(sig) = s.new_signature.as_ref().or(s.old_signature.as_ref()) {
            let via = format!("symbol {}", s.name);
            // Signatures add type/param words ("timeout", "u64") cheaply.
            push_tokens(&mut corpus, &mut vias, &mut seen, sig, &via);
        }
    }
    for f in &cs.files {
        let p = f.display_path();
        push_tokens(&mut corpus, &mut vias, &mut seen, p, &format!("file {p}"));
        let idx = via_idx(&mut vias, &format!("diff {}", p));
        for t in diff_tokens(f) {
            push_token(&mut corpus, &mut seen, t, idx);
        }
    }
    for r in &cs.review_items {
        push_tokens(
            &mut corpus,
            &mut vias,
            &mut seen,
            &r.title,
            &format!("review {}", r.id),
        );
        push_tokens(
            &mut corpus,
            &mut vias,
            &mut seen,
            &r.why,
            &format!("review {}", r.id),
        );
        for e in &r.evidence {
            push_tokens(
                &mut corpus,
                &mut vias,
                &mut seen,
                &e.summary,
                &format!("review {}", r.id),
            );
        }
    }
    (corpus, vias)
}

/// Stripped added-line identifiers for one file (shared by the global
/// corpus and per-item attribution so a diff-only term match still
/// attributes to the items touching that file).
fn diff_tokens(f: &rift_core::FileChange) -> Vec<String> {
    let mut toks = Vec::new();
    let mut lines = 0;
    for h in &f.hunks {
        for l in &h.lines {
            if !matches!(l.kind, rift_core::DiffLineKind::Addition) {
                continue;
            }
            lines += 1;
            if lines > 500 {
                return toks;
            }
            toks.extend(split_ident(&clean_line(&l.text)));
        }
    }
    toks
}

/// One added line, stripped and capped for tokenizing: short lines
/// without quotes or comment markers pass through borrow-only (the
/// common case — no allocation), everything else takes the slow path.
fn clean_line(text: &str) -> std::borrow::Cow<'_, str> {
    // Byte length bounds char count from above, so `len() <= 300`
    // guarantees the char cap without walking.
    if text.len() <= 300 && !text.contains('"') && !text.contains("//") {
        std::borrow::Cow::Borrowed(text)
    } else {
        let capped: String = text.chars().take(300).collect();
        std::borrow::Cow::Owned(strip_code_literals(&capped))
    }
}

/// Corpus tokens searchable for one review item (for per-item hits).
/// Deduplicated in first-occurrence order: matching takes the max per
/// term, so repeats never change a verdict — only the pair count.
fn item_corpus(item: &ReviewItem, file_tokens: &HashMap<&str, Vec<String>>) -> Vec<String> {
    let mut toks: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let push = |toks: &mut Vec<String>, seen: &mut HashSet<String>, s: String| {
        if !seen.contains(s.as_str()) {
            seen.insert(s.clone());
            toks.push(s);
        }
    };
    for src in [&item.title, &item.why] {
        for t in split_ident(src) {
            push(&mut toks, &mut seen, t);
        }
    }
    for f in &item.files {
        for t in split_ident(f) {
            push(&mut toks, &mut seen, t);
        }
    }
    for s in &item.symbols {
        for t in split_ident(s) {
            push(&mut toks, &mut seen, t);
        }
    }
    for e in &item.evidence {
        for t in split_ident(&e.summary) {
            push(&mut toks, &mut seen, t);
        }
    }
    for path in &item.files {
        if let Some(ts) = file_tokens.get(path.as_str()) {
            for t in ts {
                push(&mut toks, &mut seen, t.clone());
            }
        }
    }
    toks
}

/// Score one term against one token with both bigram sets precomputed.
/// Identical results to the old per-pair computation: exact 1.0,
/// long-enough substring 0.8, else bigram Dice at threshold — with two
/// shortcuts. Two-letter tokens (`in`, `of`, `to` from `for x in ...`)
/// are substrings of almost everything, so stem matches require both
/// sides to carry real signal; and the integer Dice bound skips pairs
/// that cannot reach the threshold before any bigram work.
fn match_score_owned(term: &str, term_g: &Bigrams, tok: &str, tok_g: &Bigrams) -> f32 {
    if term == tok {
        return 1.0;
    }
    if (tok.contains(term) || term.contains(tok)) && term.len() >= 4 && tok.len() >= 4 {
        return 0.8;
    }
    if !dice_possible(term_g.chars, tok_g.chars) {
        return 0.0;
    }
    let s = dice(term_g, tok_g);
    if s >= FUZZY_THRESHOLD {
        s
    } else {
        0.0
    }
}

/// Lazy variant for short-lived token lists: the multiset builds on
/// first fuzzy need and is reused for the remaining terms. Same scores
/// as [`match_score_owned`] — one rule, two call shapes.
fn match_score_lazy(term: &str, term_g: &Bigrams, tok: &str, slot: &mut Option<Bigrams>) -> f32 {
    if term == tok {
        return 1.0;
    }
    if (tok.contains(term) || term.contains(tok)) && term.len() >= 4 && tok.len() >= 4 {
        return 0.8;
    }
    if !dice_possible(term_g.chars, tok.chars().count()) {
        return 0.0;
    }
    let tok_g = slot.get_or_insert_with(|| bigrams_of(tok));
    let s = dice(term_g, tok_g);
    if s >= FUZZY_THRESHOLD {
        s
    } else {
        0.0
    }
}

/// Match every task term against the corpus; returns (matched, unmatched).
/// Two phases per term with identical outcomes to a full scan: exact hits
/// resolve by table lookup (attribution = first corpus occurrence, as the
/// old scan's break did), and only terms without one pay the fuzzy scan.
/// Corpus bigrams arrive precomputed (one pass over unique tokens), so the
/// hot loop hashes nothing and allocates only the tiny Dice bitmap.
fn match_terms(
    terms: &[String],
    term_grams: &[Bigrams],
    corpus: &[CorpusToken],
    corp_grams: &[Bigrams],
    vias: &[String],
) -> (Vec<TaskTermMatch>, Vec<String>) {
    let exact: HashSet<&str> = corpus.iter().map(|t| t.token.as_str()).collect();
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    for (term, term_g) in terms.iter().zip(term_grams.iter()) {
        if exact.contains(term.as_str()) {
            let via = corpus
                .iter()
                .find(|t| t.token == *term)
                .map(|t| vias[t.via as usize].clone())
                .unwrap_or_default();
            matched.push(TaskTermMatch {
                term: term.clone(),
                matched_via: via,
                score: 1.0,
            });
            continue;
        }
        let mut best: Option<(u32, f32)> = None;
        for (tok, tok_g) in corpus.iter().zip(corp_grams.iter()) {
            let s = match_score_owned(term, term_g, &tok.token, tok_g);
            if s > 0.0 && best.map(|(_, b)| s > b).unwrap_or(true) {
                best = Some((tok.via, s));
            }
        }
        match best {
            Some((via, score)) => matched.push(TaskTermMatch {
                term: term.clone(),
                matched_via: vias[via as usize].clone(),
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
    let (corpus, vias) = build_corpus(cs);
    let term_grams: Vec<Bigrams> = terms.iter().map(|t| bigrams_of(t)).collect();
    let corp_grams: Vec<Bigrams> = corpus.iter().map(|t| bigrams_of(&t.token)).collect();
    let (matched, unmatched) = match_terms(&terms, &term_grams, &corpus, &corp_grams, &vias);
    let coverage = matched.len() as f32 / terms.len() as f32;

    // Per-file diff tokens once (many items share files via moves), then
    // per-item attribution over deduplicated tokens with precomputed
    // bigrams. Comparisons reuse the corpus bigram cache, so repeated
    // identifiers across items cost nothing extra.
    let mut file_tokens: HashMap<&str, Vec<String>> = HashMap::new();
    for f in &cs.files {
        file_tokens
            .entry(f.display_path())
            .or_insert_with(|| diff_tokens(f));
    }
    let matched_set: HashSet<&str> = matched.iter().map(|m| m.term.as_str()).collect();
    let mut hits: Vec<TaskItemHit> = Vec::new();
    for item in &cs.review_items {
        let toks = item_corpus(item, &file_tokens);
        // Lazy per-token bigrams: most pairs resolve on ==/substring or
        // the integer prefilter, so building multisets eagerly would
        // allocate for tokens that never reach Dice.
        let mut gram_slots: Vec<Option<Bigrams>> = vec![None; toks.len()];
        let mut hit_terms: Vec<String> = Vec::new();
        for (term, term_g) in terms.iter().zip(term_grams.iter()) {
            if !matched_set.contains(term.as_str()) {
                continue;
            }
            let mut best = 0.0f32;
            for (tok, slot) in toks.iter().zip(gram_slots.iter_mut()) {
                let s = match_score_lazy(term, term_g, tok, slot);
                best = best.max(s);
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
    // out of three is Partial, not Covered. The Partial floor is exactly
    // 1/3 so one matched term out of three still reads as Partial.
    let verdict = if coverage >= 0.8 {
        TaskVerdict::Covered
    } else if coverage >= 1.0 / 3.0 {
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
            s.push_str(
                "  hint: some task terms have no matching change — check scope or wording.\n",
            );
        }
        TaskVerdict::Uncovered => {
            s.push_str(
                "  hint: no task term matches this diff — wrong branch, or work not started.\n",
            );
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
            evidence: vec![Evidence::new(
                "timeout-change",
                "timeout 900 -> 86400",
                "src/auth/session.rs",
            )],
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
        assert!(c.coverage > 0.0, "close typos should fuzzy-match: {c:?}");
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
        fn scored(term: &str, tok: &str) -> f32 {
            super::match_score_owned(term, &super::bigrams_of(term), tok, &super::bigrams_of(tok))
        }
        assert_eq!(scored("invoicing", "in"), 0.0);
        assert_eq!(scored("timeout", "to"), 0.0);
        // Exact and long-stem matches still work.
        assert_eq!(scored("timeout", "timeout"), 1.0);
        assert_eq!(scored("rewrite", "rewrites"), 0.8);
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
    fn one_of_three_is_partial_not_uncovered() {
        let cs = harness();
        let c = check_task(&cs, "session timeout zebra");
        assert_eq!(c.coverage, 2.0 / 3.0);
        assert_eq!(c.verdict, TaskVerdict::Partial, "{c:?}");
    }

    #[test]
    fn diff_only_term_still_attributes_to_item() {
        use rift_core::{DiffLine, DiffLineKind, Hunk};
        // Term appears only in added diff lines (no symbol/title match).
        let mut cs = harness();
        cs.symbol_changes.clear();
        if let Some(item) = cs.review_items.first_mut() {
            item.symbols.clear();
            item.title = "unrelated change".into();
            item.why = String::new();
            item.evidence.clear();
        }
        cs.files[0].hunks = vec![Hunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 1,
            header: String::new(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Addition,
                text: "invoicing_total += zebra_count;".into(),
            }],
        }];
        let c = check_task(&cs, "invoicing zebra");
        assert_eq!(c.verdict, TaskVerdict::Covered, "{c:?}");
        assert_eq!(c.item_hits.len(), 1, "diff match attributes: {c:?}");
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
