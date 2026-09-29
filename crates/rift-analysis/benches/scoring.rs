//! Review-grouping throughput on a synthetic 200-file ChangeSet.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rift_core::{DiffLine, DiffLineKind, FileChange, FileStatus, Hunk, Language};

fn synthetic_file(i: usize) -> FileChange {
    let path = if i.is_multiple_of(20) {
        format!("src/auth/session_{i}.rs")
    } else if i.is_multiple_of(7) {
        format!("src/tests/t_{i}.rs")
    } else {
        format!("src/mod_{i}.rs")
    };
    FileChange {
        old_path: path.clone(),
        new_path: path.clone(),
        status: FileStatus::Modified,
        language: Language::Rust,
        is_binary: false,
        is_generated: false,
        is_test_file: path.contains("tests"),
        added_lines: 5,
        deleted_lines: 3,
        hunks: vec![Hunk {
            old_start: 10,
            old_lines: 3,
            new_start: 10,
            new_lines: 5,
            header: String::new(),
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Deletion,
                    text: "if !is_valid(x) {".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Addition,
                    text: "if check(x) {".into(),
                },
            ],
        }],
        old_content: Some("pub fn f() {}\n".into()),
        new_content: Some("pub fn f() {}\n".into()),
    }
}

fn bench_analyze(c: &mut Criterion) {
    c.bench_function("analyze_200_files", |b| {
        b.iter(|| {
            let mut files: Vec<FileChange> = (0..200).map(synthetic_file).collect();
            rift_analysis::analyze(
                black_box("."),
                black_box("HEAD"),
                black_box("work"),
                black_box(&mut files),
            )
        })
    });
}

criterion_group!(benches, bench_analyze);
criterion_main!(benches);
