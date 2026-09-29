# Rift — what actually changed?

Rift is the review layer between coding agents and your codebase. Agents can
modify dozens of files in seconds; Rift answers what behavior changed, what
matters, and what deserves human attention — before you merge.

    rift .                # review working tree vs HEAD (opens the app)
    rift --overview       # text overview, no GUI
    rift --staged         # staged changes
    rift --commit <sha>   # one commit vs its parent
    rift main..HEAD       # branch comparison
    rift --json           # machine-readable ChangeSet (for agents/tools)

No accounts, no cloud, no config. Local-first and deterministic: parsing and
static signals come before any probabilistic judgment, and every conclusion
carries evidence you can drill into.

## Layout

    crates/rift-core      ChangeSet model: files, symbols, evidence, review items
    crates/rift-git       git2-backed diff extraction (worktree/staged/commit/range)
    crates/rift-parser    syn (Rust) + tree-sitter (TS/JS/Python/Go/C#) + regex fallback
    crates/rift-analysis  deterministic importance scoring + review grouping
    crates/rift-cli       `rift` binary
    crates/rift-ui        eframe/egui native UI (Overview/Queue/Files/Diff)
    docs/                 architecture notes

## Install

    cargo install --path crates/rift-cli

Puts `rift` on PATH (`~/.cargo/bin`). Then from any repo:

    cd <your-repo> && rift

Rift needs a git repository to read. Point it at one (`rift <path>`) or
start tracking (`git init && git add -A && git commit -m init`).

## Build

    cargo build
    cargo test
    cargo clippy

See `docs/ARCHITECTURE.md` for decisions and the roadmap (JEV scoring layer,
LLM escalation, blast-radius graph).
