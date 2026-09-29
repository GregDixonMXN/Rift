//! Symbol extraction + matching throughput on a generated Rust module.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rift_core::Language;
use rift_parser::{diff_symbols, extract_symbols};

fn big_module(n_fns: usize) -> String {
    let mut s = String::from("pub const VERSION: u64 = 7;\n\n");
    for i in 0..n_fns {
        s.push_str(&format!(
            "pub fn handler_{i}(req: &str, flag: bool) -> String {{\n    let _ = (req, flag);\n    format!(\"ok-{i}\")\n}}\n\n"
        ));
    }
    s
}

fn bench_extract(c: &mut Criterion) {
    let src = big_module(400);
    c.bench_function("extract_400_fns", |b| {
        b.iter(|| extract_symbols(black_box("bench.rs"), Language::Rust, black_box(&src)))
    });
}

fn bench_diff(c: &mut Criterion) {
    let old = big_module(400);
    let mut new = big_module(400);
    // Simulate an agent edit: change one signature, add one function.
    new = new.replacen(
        "pub fn handler_7(req: &str, flag: bool) -> String {",
        "pub fn handler_7(req: &str, flag: bool, extra: u32) -> String {",
        1,
    );
    new.push_str("pub fn handler_new() {}\n");
    let o = extract_symbols("bench.rs", Language::Rust, &old);
    let n = extract_symbols("bench.rs", Language::Rust, &new);
    c.bench_function("diff_400_fns", |b| {
        b.iter(|| {
            diff_symbols(
                black_box("bench.rs"),
                black_box(&o),
                black_box(&n),
                Some(black_box(&old)),
                Some(black_box(&new)),
            )
        })
    });
}

criterion_group!(benches, bench_extract, bench_diff);
criterion_main!(benches);
