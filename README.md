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
    rift --jev --overview # add Jev risk/severity judgments (needs TYPESAFE_API_KEY)
    rift --jev-local --overview # same evidence kinds, computed offline (no key)

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
    crates/rift-jev       opt-in Jev risk/severity layer (TypeSafe API)
    docs/                 architecture notes

## Install

    cargo install --path crates/rift-cli

Puts `rift` on PATH (`~/.cargo/bin`). Then from any repo:

    cd <your-repo> && rift

Rift needs a git repository to read. Point it at one (`rift <path>`) or
start tracking (`git init && git add -A && git commit -m init`).

## Hooks and CI

Gate commits on severity (batch only, exit 2 when triggered):

    rift --staged --overview --fail-on high

Install as a pre-commit hook (reviews what you're about to commit):

    cat > .git/hooks/pre-commit <<'EOF'
    #!/bin/sh
    rift --staged --overview --fail-on high || {
      echo "rift blocked this commit — review the items above." >&2
      exit 1
    }
    EOF
    chmod +x .git/hooks/pre-commit

Review a pull request as a range (needs the base locally):

    git fetch origin main && rift main..HEAD --overview
    rift --staged --json --fail-on critical | jq .stats

## Build

    cargo build
    cargo test
    cargo clippy

## License

Apache-2.0 — see `LICENSE`. The deterministic core (git, parsing,
analysis, UI) is yours to run, fork, and embed. Cloud judgment
(`--jev`, TypeSafe API) is opt-in and billed by its provider, if at all.

See `docs/ARCHITECTURE.md` for decisions and the roadmap (JEV scoring layer,
LLM escalation, blast-radius graph).
