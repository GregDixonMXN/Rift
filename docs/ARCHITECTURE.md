# Rift architecture

## Pipeline

    Git (rift-git)
      -> FileChange + hunks (normalized, library types never leak)
      -> Symbols (rift-parser: syn for Rust, tree-sitter for TS/JS, regex fallback)
      -> SymbolChange matching with confidence (rename via bigram similarity)
      -> Importance scoring + grouping (rift-analysis, deterministic)
      -> ChangeSet { files, symbol_changes, review_items, stats }
      -> UI (rift-ui) or JSON/text (rift-cli)

## Decisions

- **git2 (libgit2) over gitoxide.** Rename/copy similarity detection
  (`find_similar`), worktree+index diffs, and submodule handling are mature
  today. The `GitEngine` wrapper keeps the dependency behind normalized
  `FileChange`s, so the backend can be swapped without touching analysis or UI.
- **syn for Rust, tree-sitter for TS/JS.** syn gives precise Rust signatures
  and span lines; tree-sitter gives one engine for the JS/TS family with
  cheap incremental re-parsing later. Everything else uses a line-pattern
  fallback — unsupported languages degrade to raw diff, never to failure.
- **eframe/egui for UI.** Immediate-mode native Rust, single binary, no
  browser runtime. List virtualization and progressive population keep
  100k-line diffs responsive; heavy analysis stays off the UI thread.
- **Deterministic before probabilistic.** Importance scores are hand-tuned
  functions of path signals, symbol changes, and diff-content signals, each
  emitting `Evidence`. A JEV probabilistic layer can sit *above* this model
  later (trait boundary reserved), consuming structured facts — never raw
  source — and the UI must always explain itself without it.
- **Symbol-bonus cap.** Per-symbol score contributions are capped in
  aggregate so a new 30-symbol file can't outrank an auth behavior change.
- **Keyword hygiene.** Content signals (`unsafe`, `valid`, …) match on code
  with string literals, raw strings, and line comments stripped, so tooling
  that *mentions* a keyword isn't flagged as *using* it.

## What's next (in order)

1. Harden MVP: snapshot tests, cancel-safe background analysis.
2. Milestone two: Python/C# depth, move detection, dependency graph +
   blast radius, test association, persistent content-hash cache, JEV trait
   + deterministic adapter.
3. Milestone three: task-vs-change verification, optional LLM escalation on
   compact evidence packages, PR/hook integrations.

## Perf baselines (criterion, Windows x64, debug)

- `analyze_200_files`: ~1.85 ms — 200-file review grouping is trivial.
- `extract_400_fns`: ~8.6 ms — syn extraction on a 400-function module.
- Run: `cargo bench -p rift-parser -p rift-analysis`.
