//! Rift core: rich internal change model.
//! The UI never operates on raw git hunks directly — everything flows
//! through `ChangeSet` with evidence attached to every conclusion.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    CSharp,
    C,
    Cpp,
    Go,
    Java,
    Unknown,
}

impl Language {
    pub fn from_path(path: &str) -> Self {
        let lower = path.to_lowercase();
        if lower.ends_with(".rs") {
            Self::Rust
        } else if lower.ends_with(".ts") || lower.ends_with(".tsx") || lower.ends_with(".mts") {
            Self::TypeScript
        } else if lower.ends_with(".js")
            || lower.ends_with(".jsx")
            || lower.ends_with(".mjs")
            || lower.ends_with(".cjs")
        {
            Self::JavaScript
        } else if lower.ends_with(".py") {
            Self::Python
        } else if lower.ends_with(".cs") {
            Self::CSharp
        } else if lower.ends_with(".go") {
            Self::Go
        } else if lower.ends_with(".java") {
            Self::Java
        } else if lower.ends_with(".c") || lower.ends_with(".h") {
            Self::C
        } else if lower.ends_with(".cpp")
            || lower.ends_with(".cc")
            || lower.ends_with(".hpp")
            || lower.ends_with(".cxx")
        {
            Self::Cpp
        } else {
            Self::Unknown
        }
    }

    pub fn supports_symbols(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Unmodified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub old_path: String,
    pub new_path: String,
    pub status: FileStatus,
    pub language: Language,
    pub is_binary: bool,
    pub is_generated: bool,
    pub is_test_file: bool,
    pub added_lines: usize,
    pub deleted_lines: usize,
    pub hunks: Vec<Hunk>,
    /// Full old/new text when available (worktree + small files). Used for parsing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_content: Option<String>,
}

impl FileChange {
    pub fn display_path(&self) -> &str {
        if self.new_path.is_empty() {
            &self.old_path
        } else {
            &self.new_path
        }
    }

    /// Formatting-only heuristic: compare whitespace-stripped line multisets.
    /// Indentation or brace reflow yields identical multisets; any token
    /// change (including comments) does not. Pure add/delete is never
    /// formatting-only.
    pub fn looks_formatting_only(&self) -> bool {
        if self.hunks.is_empty() {
            return false;
        }
        let mut del = Vec::new();
        let mut add = Vec::new();
        for h in &self.hunks {
            for l in &h.lines {
                match l.kind {
                    DiffLineKind::Context => {}
                    DiffLineKind::Addition => {
                        let t: String = l.text.chars().filter(|c| !c.is_whitespace()).collect();
                        if !t.is_empty() {
                            add.push(t);
                        }
                    }
                    DiffLineKind::Deletion => {
                        let t: String = l.text.chars().filter(|c| !c.is_whitespace()).collect();
                        if !t.is_empty() {
                            del.push(t);
                        }
                    }
                }
            }
        }
        if del.is_empty() || add.is_empty() {
            return false;
        }
        del.sort();
        add.sort();
        del == add
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Module,
    Const,
    Static,
    Test,
    TypeAlias,
    Field,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
    pub is_test: bool,
    pub is_public: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolChangeKind {
    Added,
    Removed,
    Modified,
    SignatureChanged,
    VisibilityChanged,
    Moved,
    Renamed,
    FormattingOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolChange {
    pub file: String,
    pub name: String,
    pub kind: SymbolKind,
    pub change: SymbolChangeKind,
    pub confidence: f32,
    pub old_signature: Option<String>,
    pub new_signature: Option<String>,
    pub old_lines: Option<(u32, u32)>,
    pub new_lines: Option<(u32, u32)>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Category {
    Behavior,
    Security,
    ApiBreak,
    Schema,
    Migration,
    Config,
    Dependency,
    Test,
    Mechanical,
    Refactor,
    Docs,
    Auth,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: String,
    pub summary: String,
    pub file: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
}

impl Evidence {
    pub fn new(kind: &str, summary: &str, file: &str) -> Self {
        Self {
            kind: kind.to_string(),
            summary: summary.to_string(),
            file: file.to_string(),
            old_value: None,
            new_value: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: String,
    pub title: String,
    pub category: Category,
    pub severity: Severity,
    /// 0..100 priority score (deterministic).
    pub priority: u8,
    /// 0.0..1.0 confidence.
    pub confidence: f32,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub why: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeStats {
    pub files_changed: usize,
    pub added_lines: usize,
    pub deleted_lines: usize,
    pub meaningful_changes: usize,
    pub mechanical_files: usize,
    pub test_files: usize,
    pub symbols_added: usize,
    pub symbols_removed: usize,
    pub symbols_modified: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskVerdict {
    Covered,
    Partial,
    Uncovered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTermMatch {
    pub term: String,
    pub matched_via: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskItemHit {
    pub item_id: String,
    pub title: String,
    pub matched_terms: Vec<String>,
    /// Fraction of task terms matched by this item (0.0..1.0).
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCheck {
    pub task_text: String,
    pub terms: Vec<String>,
    pub matched: Vec<TaskTermMatch>,
    pub unmatched: Vec<String>,
    pub item_hits: Vec<TaskItemHit>,
    /// matched.len() / terms.len() (1.0 when there are no terms is 0.0).
    pub coverage: f32,
    pub verdict: TaskVerdict,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeSet {
    pub repo_root: String,
    pub base_ref: String,
    pub head_ref: String,
    pub files: Vec<FileChange>,
    pub symbol_changes: Vec<SymbolChange>,
    pub review_items: Vec<ReviewItem>,
    pub stats: ChangeStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_check: Option<TaskCheck>,
}

impl ChangeSet {
    pub fn sorted_review_items(&self) -> Vec<&ReviewItem> {
        let mut v: Vec<&ReviewItem> = self.review_items.iter().collect();
        v.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.priority.cmp(&a.priority))
                .then(a.title.cmp(&b.title))
        });
        v
    }
}
