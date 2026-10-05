//! Task-check scale: matching cost must grow ~linearly with diff size.
//! Corpus build is linear; term matching and per-item attribution are the
//! risks (bigram similarity per term × token pair).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rift_core::{
    Category, ChangeSet, ChangeStats, DiffLine, DiffLineKind, FileChange, FileStatus, Hunk,
    Language, ReviewItem, Severity,
};

const WORDS: &[&str] = &[
    "session", "timeout", "retry", "auth", "token", "cache", "queue", "payment", "invoice",
    "compute", "validate", "handle", "request", "config", "client", "server", "store",
];

fn added_line(file_i: usize, line_i: usize) -> DiffLine {
    let w1 = WORDS[(file_i + line_i) % WORDS.len()];
    let w2 = WORDS[(file_i * 7 + line_i * 3) % WORDS.len()];
    DiffLine {
        kind: DiffLineKind::Addition,
        text: format!("let {w1}_{line_i} = {w2}_compute(value, {file_i});"),
    }
}

fn changeset(n_files: usize, lines_per_file: usize) -> ChangeSet {
    let mut files = Vec::with_capacity(n_files);
    let mut items = Vec::with_capacity(n_files);
    for f in 0..n_files {
        let path = format!("src/mod_{f}.rs");
        let lines: Vec<DiffLine> = (0..lines_per_file).map(|l| added_line(f, l)).collect();
        files.push(FileChange {
            old_path: path.clone(),
            new_path: path.clone(),
            status: FileStatus::Modified,
            language: Language::Rust,
            is_binary: false,
            is_generated: false,
            is_test_file: false,
            added_lines: lines_per_file,
            deleted_lines: 0,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: lines_per_file as u32,
                header: String::new(),
                lines,
            }],
            old_content: None,
            new_content: None,
        });
        items.push(ReviewItem {
            id: format!("file:{path}"),
            title: format!("{path} changed"),
            category: Category::Behavior,
            severity: Severity::Medium,
            priority: 40,
            confidence: 0.8,
            files: vec![path.clone()],
            symbols: vec![format!("compute_{f}")],
            evidence: vec![],
            why: format!("symbols changed in {path}"),
        });
    }
    ChangeSet {
        repo_root: ".".into(),
        base_ref: "HEAD".into(),
        head_ref: "work".into(),
        files,
        symbol_changes: vec![],
        review_items: items,
        stats: ChangeStats::default(),
        task_check: None,
    }
}

fn bench_task_check(c: &mut Criterion) {
    let small = changeset(20, 30);
    c.bench_function("task_check_20_files", |b| {
        b.iter(|| {
            rift_analysis::check_task(
                black_box(&small),
                black_box("extend session timeout handling"),
            )
        })
    });

    let large = changeset(200, 100);
    c.bench_function("task_check_200_files", |b| {
        b.iter(|| {
            rift_analysis::check_task(
                black_box(&large),
                black_box("extend session timeout handling"),
            )
        })
    });
}

criterion_group!(benches, bench_task_check);
criterion_main!(benches);
