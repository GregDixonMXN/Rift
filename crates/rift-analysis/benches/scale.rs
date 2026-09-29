//! Scale bounds: worst-case single files (minified JS, many-function
//! Python) and a 2000-file changeset. Times must grow ~linearly with
//! input, never superlinearly.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rift_core::{FileChange, FileStatus, Hunk, Language};

fn change(path: &str, lang: Language, old: String, new: String) -> FileChange {
    FileChange {
        old_path: path.into(),
        new_path: path.into(),
        status: FileStatus::Modified,
        language: lang,
        is_binary: false,
        is_generated: false,
        is_test_file: false,
        added_lines: 10,
        deleted_lines: 10,
        hunks: vec![Hunk {
            old_start: 1,
            old_lines: 10,
            new_start: 1,
            new_lines: 10,
            header: String::new(),
            lines: vec![],
        }],
        old_content: Some(old),
        new_content: Some(new),
    }
}

/// 512KB of single-line minified JS: tree-sitter's worst case.
fn minified_js() -> (String, String) {
    let mut old = String::from("var a0=0;");
    while old.len() < 512 * 1024 {
        let n = old.len();
        old.push_str(&format!("function f{n}(x){{return x+{n};}}var v{n}={n};"));
    }
    let mut new = old.clone();
    new.push_str("function added(x){return x;}");
    (old, new)
}

/// 512KB of small Python functions (~3500 symbols).
fn many_fn_py() -> (String, String) {
    let mut old = String::new();
    for i in 0..3500 {
        old.push_str(&format!("def f{i}(x):\n    return x + {i}\n\n"));
    }
    let mut new = old.clone();
    new = new.replacen("return x + 42", "return check(x) + 42", 1);
    (old, new)
}

fn bench_large_files(c: &mut Criterion) {
    let (jo, jn) = minified_js();
    c.bench_function("symbols_minified_js_512k", |b| {
        b.iter(|| {
            let mut files = vec![change(
                "b.min.js",
                Language::JavaScript,
                jo.clone(),
                jn.clone(),
            )];
            rift_analysis::analyze(
                black_box(""),
                black_box("HEAD"),
                black_box("work"),
                black_box(&mut files),
            )
        })
    });

    let (po, pn) = many_fn_py();
    c.bench_function("symbols_many_fn_py_512k", |b| {
        b.iter(|| {
            let mut files = vec![change("m.py", Language::Python, po.clone(), pn.clone())];
            rift_analysis::analyze(
                black_box(""),
                black_box("HEAD"),
                black_box("work"),
                black_box(&mut files),
            )
        })
    });
}

fn bench_many_files(c: &mut Criterion) {
    c.bench_function("analyze_2000_files", |b| {
        b.iter(|| {
            let mut files: Vec<FileChange> = (0..2000)
                .map(|i| {
                    change(
                        &format!("src/mod_{i}.rs"),
                        Language::Rust,
                        "pub fn f() {}\n".into(),
                        "pub fn f() {\n    check()\n}\n".into(),
                    )
                })
                .collect();
            rift_analysis::analyze(
                black_box(""),
                black_box("HEAD"),
                black_box("work"),
                black_box(&mut files),
            )
        })
    });
}

criterion_group!(benches, bench_large_files, bench_many_files);
criterion_main!(benches);
