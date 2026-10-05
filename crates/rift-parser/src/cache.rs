//! Persistent content-hash symbol cache.
//!
//! Parsing is pure: same `(language, content)` always yields the same
//! symbols. The cache keys on `sha256(language_tag + content)` so unchanged
//! files — diff sides, worktree context scans, repeat runs — parse once.
//!
//! Disk format (JSON, versioned): `{version: 1, entries: {hash: [Symbol]}}`.
//! Bounded in memory (`MAX_ENTRIES`); eviction drops the lexicographically
//! smallest keys first so behavior is deterministic across runs.

use rift_core::{Language, Symbol};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const CACHE_VERSION: u32 = 1;
/// Upper bound on cached contents. Past this, the smallest keys are evicted
/// in bulk — deterministic, no timestamps to persist.
pub const MAX_ENTRIES: usize = 5000;

fn language_tag(lang: Language) -> &'static str {
    match lang {
        Language::Rust => "rs",
        Language::TypeScript => "ts",
        Language::JavaScript => "js",
        Language::Python => "py",
        Language::CSharp => "cs",
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::Go => "go",
        Language::Java => "java",
        Language::Unknown => "unknown",
    }
}

/// Stable content-hash key. Hex-encoded sha256 over the language tag,
/// a NUL separator, and the raw content bytes.
pub fn content_key(language: Language, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(language_tag(language).as_bytes());
    h.update([0u8]);
    h.update(content.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Default on-disk location: `$XDG_CACHE_HOME/rift/symbols-v1.json`
/// falling back to `~/.cache/rift/symbols-v1.json`. Returns `None` when
/// no home directory can be determined.
pub fn default_cache_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return Some(dir.join("rift").join("symbols-v1.json"));
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".cache").join("rift").join("symbols-v1.json"))
}

#[derive(Debug, Default)]
pub struct SymbolCache {
    map: HashMap<String, Vec<Symbol>>,
    pub hits: u64,
    pub misses: u64,
}

impl SymbolCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    pub fn get(&self, key: &str) -> Option<&Vec<Symbol>> {
        self.map.get(key)
    }

    pub fn insert(&mut self, key: String, symbols: Vec<Symbol>) {
        if !self.map.contains_key(&key) && self.map.len() >= MAX_ENTRIES {
            // Deterministic eviction: drop the smallest 1/8 of keys.
            let drop = (MAX_ENTRIES / 8).max(1);
            let mut keys: Vec<String> = self.map.keys().cloned().collect();
            keys.sort();
            for k in keys.into_iter().take(drop) {
                self.map.remove(&k);
            }
        }
        self.map.insert(key, symbols);
    }

    /// Cached extraction: hash, hit-clone, or parse + store.
    pub fn extract(
        &mut self,
        path: &str,
        language: Language,
        content: &str,
        parse: impl FnOnce() -> Vec<Symbol>,
    ) -> Vec<Symbol> {
        let key = content_key(language, content);
        if let Some(hit) = self.map.get(&key) {
            self.hits += 1;
            // Rewrite the file path: same content may appear under many paths
            // (copies, context scans); symbols must point at the requester.
            return hit
                .iter()
                .map(|s| {
                    let mut owned = s.clone();
                    owned.file = path.to_string();
                    owned
                })
                .collect();
        }
        self.misses += 1;
        let symbols = parse();
        self.insert(key, symbols.clone());
        symbols
    }

    /// Load from disk. Missing/corrupt files yield an empty cache —
    /// a cache must never fail a review.
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Self::default();
        };
        if value.get("version").and_then(|v| v.as_u64()) != Some(CACHE_VERSION as u64) {
            return Self::default();
        }
        let mut cache = Self::default();
        if let Some(entries) = value.get("entries").and_then(|e| e.as_object()) {
            for (k, v) in entries {
                if cache.map.len() >= MAX_ENTRIES {
                    break;
                }
                if let Ok(syms) = serde_json::from_value::<Vec<Symbol>>(v.clone()) {
                    cache.map.insert(k.clone(), syms);
                }
            }
        }
        cache
    }

    /// Save to disk. Creates parent dirs; failures are reported to the
    /// caller (call sites treat them as best-effort warnings).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entries: HashMap<&String, &Vec<Symbol>> = self.map.iter().collect();
        let value = serde_json::json!({
            "version": CACHE_VERSION,
            "entries": entries,
        });
        // Compact, not pretty: the cache is machine state rewritten every
        // run (up to 5000 entries) — whitespace is pure write/read cost.
        // Loading accepts either shape, so old pretty files still parse.
        let bytes = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
        // Write atomically: temp file + rename so a killed run never
        // leaves a truncated cache behind.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, file: &str) -> Symbol {
        Symbol {
            name: name.into(),
            kind: rift_core::SymbolKind::Function,
            file: file.into(),
            start_line: 1,
            end_line: 3,
            signature: format!("fn {name}()"),
            is_test: false,
            is_public: true,
        }
    }

    #[test]
    fn keys_separate_language_and_content() {
        let a = content_key(Language::Rust, "fn f() {}");
        let b = content_key(Language::Python, "fn f() {}");
        let c = content_key(Language::Rust, "fn g() {}");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert_eq!(content_key(Language::Rust, "fn f() {}"), a);
    }

    #[test]
    fn extract_hits_rewrite_path() {
        let mut cache = SymbolCache::new();
        let first = cache.extract("a.rs", Language::Rust, "fn f() {}", || {
            vec![sym("f", "a.rs")]
        });
        assert_eq!(cache.misses, 1);
        let second = cache.extract("copy.rs", Language::Rust, "fn f() {}", || {
            panic!("must hit")
        });
        assert_eq!(cache.hits, 1);
        assert_eq!(first.len(), second.len());
        assert!(second.iter().all(|s| s.file == "copy.rs"));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("symbols-v1.json");
        let mut cache = SymbolCache::new();
        cache.insert("abc".into(), vec![sym("f", "a.rs")]);
        cache.save(&path).expect("save");
        let loaded = SymbolCache::load(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get("abc").unwrap()[0].name, "f");
    }

    #[test]
    fn load_missing_or_bad_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(SymbolCache::load(&dir.path().join("nope.json")).is_empty());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"not json{{{").expect("write");
        assert!(SymbolCache::load(&bad).is_empty());
        let wrong = dir.path().join("wrong.json");
        std::fs::write(&wrong, r#"{"version":999,"entries":{}}"#).expect("write");
        assert!(SymbolCache::load(&wrong).is_empty());
    }

    #[test]
    fn cap_evicts_deterministically() {
        let mut cache = SymbolCache::new();
        for i in 0..MAX_ENTRIES + 100 {
            cache.insert(format!("k{i:06}"), vec![sym("f", "a.rs")]);
        }
        assert!(cache.len() <= MAX_ENTRIES);
    }
}
