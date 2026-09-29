//! Symbol extraction: syn for Rust (precise), tree-sitter for JS/TS,
//! line-regex fallback for other languages. Then symbol-level diffing.

use rift_core::{Language, Symbol, SymbolChange, SymbolChangeKind, SymbolKind};
use std::collections::HashMap;
use syn::spanned::Spanned;

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

pub fn extract_symbols(path: &str, language: Language, content: &str) -> Vec<Symbol> {
    match language {
        Language::Rust => extract_rust(path, content),
        Language::TypeScript | Language::JavaScript => extract_ts(path, language, content),
        _ => extract_generic(path, content),
    }
}

fn extract_rust(path: &str, content: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let file = match syn::parse_file(content) {
        Ok(f) => f,
        Err(_) => return extract_generic(path, content),
    };
    for item in &file.items {
        collect_rust_item(path, item, None, &mut out);
    }
    out
}

fn collect_rust_item(path: &str, item: &syn::Item, impl_ctx: Option<&str>, out: &mut Vec<Symbol>) {
    use syn::Item;
    match item {
        Item::Fn(f) => {
            let name = f.sig.ident.to_string();
            let is_test = f.attrs.iter().any(|a| {
                a.path().is_ident("test")
                    || a.path()
                        .segments
                        .last()
                        .map(|s| s.ident == "test")
                        .unwrap_or(false)
            });
            let (s, e) = fn_body_lines(&f.sig, &f.block);
            out.push(Symbol {
                name: if let Some(ctx) = impl_ctx {
                    format!("{ctx}::{name}")
                } else {
                    name
                },
                kind: if is_test {
                    SymbolKind::Test
                } else if impl_ctx.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                file: path.to_string(),
                start_line: s,
                end_line: e,
                signature: fn_sig(&f.sig),
                is_test,
                is_public: matches!(f.vis, syn::Visibility::Public(_)),
            });
        }
        Item::Struct(s) => {
            let sl = ident_line(&s.ident);
            let el = struct_end(s, sl);
            out.push(Symbol {
                name: s.ident.to_string(),
                kind: SymbolKind::Struct,
                file: path.to_string(),
                start_line: sl,
                end_line: el,
                signature: format!("struct {}", s.ident),
                is_test: false,
                is_public: matches!(s.vis, syn::Visibility::Public(_)),
            });
        }
        Item::Enum(e) => {
            out.push(Symbol {
                name: e.ident.to_string(),
                kind: SymbolKind::Enum,
                file: path.to_string(),
                start_line: ident_line(&e.ident),
                end_line: brace_end(&e.brace_token.span, ident_line(&e.ident)),
                signature: format!("enum {}", e.ident),
                is_test: false,
                is_public: matches!(e.vis, syn::Visibility::Public(_)),
            });
        }
        Item::Trait(t) => {
            out.push(Symbol {
                name: t.ident.to_string(),
                kind: SymbolKind::Trait,
                file: path.to_string(),
                start_line: ident_line(&t.ident),
                end_line: brace_end(&t.brace_token.span, ident_line(&t.ident)),
                signature: format!("trait {}", t.ident),
                is_test: false,
                is_public: matches!(t.vis, syn::Visibility::Public(_)),
            });
        }
        Item::Impl(i) => {
            let ctx = type_name(&i.self_ty);
            for inner in &i.items {
                if let syn::ImplItem::Fn(m) = inner {
                    let name = m.sig.ident.to_string();
                    let is_test = m.attrs.iter().any(|a| a.path().is_ident("test"));
                    let (ms, me) = fn_body_lines(&m.sig, &m.block);
                    out.push(Symbol {
                        name: format!("{ctx}::{name}"),
                        kind: if is_test {
                            SymbolKind::Test
                        } else {
                            SymbolKind::Method
                        },
                        file: path.to_string(),
                        start_line: ms,
                        end_line: me,
                        signature: fn_sig(&m.sig),
                        is_test,
                        is_public: matches!(m.vis, syn::Visibility::Public(_)),
                    });
                }
            }
        }
        Item::Const(c) => {
            out.push(Symbol {
                name: c.ident.to_string(),
                kind: SymbolKind::Const,
                file: path.to_string(),
                start_line: ident_line(&c.ident),
                end_line: token_end(c.semi_token.span(), ident_line(&c.ident)),
                signature: format!("const {}", c.ident),
                is_test: false,
                is_public: matches!(c.vis, syn::Visibility::Public(_)),
            });
        }
        Item::Static(s) => {
            out.push(Symbol {
                name: s.ident.to_string(),
                kind: SymbolKind::Static,
                file: path.to_string(),
                start_line: ident_line(&s.ident),
                end_line: token_end(s.semi_token.span(), ident_line(&s.ident)),
                signature: format!("static {}", s.ident),
                is_test: false,
                is_public: matches!(s.vis, syn::Visibility::Public(_)),
            });
        }
        Item::Mod(m) => {
            out.push(Symbol {
                name: m.ident.to_string(),
                kind: SymbolKind::Module,
                file: path.to_string(),
                start_line: ident_line(&m.ident),
                end_line: mod_end(m, ident_line(&m.ident)),
                signature: format!("mod {}", m.ident),
                is_test: m.ident == "tests" || is_test_mod(m),
                is_public: matches!(m.vis, syn::Visibility::Public(_)),
            });
            if let Some((_, items)) = &m.content {
                for inner in items {
                    collect_rust_item(path, inner, None, out);
                }
            }
        }
        Item::Type(t) => {
            out.push(Symbol {
                name: t.ident.to_string(),
                kind: SymbolKind::TypeAlias,
                file: path.to_string(),
                start_line: ident_line(&t.ident),
                end_line: ident_line(&t.ident),
                signature: format!("type {}", t.ident),
                is_test: false,
                is_public: matches!(t.vis, syn::Visibility::Public(_)),
            });
        }
        _ => {}
    }
}

fn is_test_mod(m: &syn::ItemMod) -> bool {
    m.attrs.iter().any(|a| {
        let s = quote::quote!(#a.path).to_string().replace(' ', "");
        s.contains("cfg(test")
    })
}

fn ident_line(i: &syn::Ident) -> u32 {
    i.span().start().line as u32
}

/// Clamp a token end-line to its item start (defensive: spans are file-local
/// but never backwards).
fn token_end(sp: proc_macro2::Span, fallback: u32) -> u32 {
    (sp.end().line as u32).max(fallback)
}

fn brace_end(span: &proc_macro2::extra::DelimSpan, fallback: u32) -> u32 {
    token_end(span.close(), fallback)
}

/// Exact (start, end) lines for a free function or method.
fn fn_body_lines(sig: &syn::Signature, block: &syn::Block) -> (u32, u32) {
    let s = sig.ident.span().start().line as u32;
    (s, brace_end(&block.brace_token.span, s))
}

fn struct_end(s: &syn::ItemStruct, fallback: u32) -> u32 {
    match &s.fields {
        syn::Fields::Named(f) => brace_end(&f.brace_token.span, fallback),
        syn::Fields::Unnamed(f) => brace_end(&f.paren_token.span, fallback),
        syn::Fields::Unit => s
            .semi_token
            .map(|t| token_end(t.span(), fallback))
            .unwrap_or(fallback),
    }
}

fn mod_end(m: &syn::ItemMod, fallback: u32) -> u32 {
    if let Some((brace, _)) = &m.content {
        brace_end(&brace.span, fallback)
    } else {
        m.semi
            .map(|t| token_end(t.span(), fallback))
            .unwrap_or(fallback)
    }
}

fn fn_sig(sig: &syn::Signature) -> String {
    let inputs: Vec<String> = sig
        .inputs
        .iter()
        .map(|a| quote::quote!(#a).to_string())
        .collect();
    let out = match &sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, t) => format!(" -> {}", quote::quote!(#t)),
    };
    format!("fn {}({}){}", sig.ident, inputs.join(", "), out)
}

fn type_name(ty: &syn::Type) -> String {
    quote::quote!(#ty).to_string().replace(' ', "")
}

// ---------------------------------------------------------------------------
// Tree-sitter JS/TS
// ---------------------------------------------------------------------------

fn ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::TypeScript => tree_sitter_typescript::language_typescript(),
        _ => tree_sitter_javascript::language(),
    }
}

fn extract_ts(path: &str, language: Language, content: &str) -> Vec<Symbol> {
    let mut parser = tree_sitter::Parser::new();
    let ts_lang = ts_language(language);
    if parser.set_language(&ts_lang).is_err() {
        return extract_generic(path, content);
    }
    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return extract_generic(path, content),
    };
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    walk_ts(tree.root_node(), path, bytes, None, &mut out);
    out
}

fn walk_ts(
    node: tree_sitter::Node,
    path: &str,
    src: &[u8],
    class_ctx: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    let kind = node.kind();
    match kind {
        "function_declaration" | "generator_function_declaration" | "function" => {
            if let Some(name) = child_name(&node, src) {
                let is_test = name.starts_with("test_") || name.contains("test");
                out.push(Symbol {
                    name: name.clone(),
                    kind: if is_test {
                        SymbolKind::Test
                    } else {
                        SymbolKind::Function
                    },
                    file: path.to_string(),
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    signature: format!("function {name}(…)"),
                    is_test,
                    is_public: !name.starts_with('_'),
                });
            }
        }
        "class_declaration" => {
            if let Some(name) = child_name(&node, src) {
                out.push(Symbol {
                    name: name.clone(),
                    kind: SymbolKind::Class,
                    file: path.to_string(),
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    signature: format!("class {name}"),
                    is_test: false,
                    is_public: !name.starts_with('_'),
                });
                // Recurse with class context for methods.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "class_body" {
                        let mut c2 = child.walk();
                        for m in child.children(&mut c2) {
                            walk_ts(m, path, src, Some(&name), out);
                        }
                    } else {
                        walk_ts(child, path, src, class_ctx, out);
                    }
                }
                return;
            }
        }
        "method_definition" => {
            if let Some(name) = child_name(&node, src) {
                let full = match class_ctx {
                    Some(c) => format!("{c}::{name}"),
                    None => name.clone(),
                };
                out.push(Symbol {
                    name: full,
                    kind: SymbolKind::Method,
                    file: path.to_string(),
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    signature: format!("{name}(…)"),
                    is_test: name.contains("test"),
                    is_public: !name.starts_with('_') && name != "constructor",
                });
            }
        }
        "interface_declaration" => {
            if let Some(name) = child_name(&node, src) {
                out.push(Symbol {
                    name: name.clone(),
                    kind: SymbolKind::Interface,
                    file: path.to_string(),
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    signature: format!("interface {name}"),
                    is_test: false,
                    is_public: true,
                });
            }
        }
        "enum_declaration" => {
            if let Some(name) = child_name(&node, src) {
                out.push(Symbol {
                    name: name.clone(),
                    kind: SymbolKind::Enum,
                    file: path.to_string(),
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                    signature: format!("enum {name}"),
                    is_test: false,
                    is_public: true,
                });
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            // const X = (...) => ... / describe/it/test blocks
            let text = node.utf8_text(src).unwrap_or_default();
            let trimmed = text.trim_start();
            let kw = trimmed.strip_prefix("export").unwrap_or(trimmed);
            let kw = kw.trim_start();
            if kw.starts_with("const ") || kw.starts_with("let ") || kw.starts_with("var ") {
                if let Some(name) = child_name(&node, src) {
                    let is_test =
                        text.contains("describe(") || text.contains("it(") || name.contains("test");
                    out.push(Symbol {
                        name: name.clone(),
                        kind: if is_test {
                            SymbolKind::Test
                        } else {
                            SymbolKind::Const
                        },
                        file: path.to_string(),
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        signature: format!("const {name}"),
                        is_test,
                        is_public: text.trim_start().starts_with("export"),
                    });
                }
            }
        }
        _ => {}
    }
    // Default recursion (non-class path).
    if kind != "class_declaration" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            // Skip descending into function bodies for speed? No — nested fns matter.
            walk_ts(child, path, src, class_ctx, out);
        }
    }
}

fn child_name(node: &tree_sitter::Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "property_identifier" {
            if let Ok(s) = child.utf8_text(src) {
                return Some(s.to_string());
            }
        }
    }
    // Fallback: `name` field.
    if let Some(n) = node.child_by_field_name("name") {
        if let Ok(s) = n.utf8_text(src) {
            return Some(s.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Generic regex fallback (Python/C#/Go/Java/C/C++ and unparseable files)
// ---------------------------------------------------------------------------

fn extract_generic(path: &str, content: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let ln = idx as u32 + 1;
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with('#') || t.starts_with('*') {
            continue;
        }
        if let Some(sym) = generic_line(path, t, ln) {
            out.push(sym);
        }
    }
    out
}

fn generic_line(path: &str, t: &str, ln: u32) -> Option<Symbol> {
    let mk = |name: String, kind: SymbolKind, sig: String| Symbol {
        name: name.clone(),
        kind,
        file: path.to_string(),
        start_line: ln,
        end_line: ln,
        signature: sig,
        is_test: name.contains("test") || t.contains("[Test") || t.contains("@Test"),
        is_public: t.contains("pub ") || t.contains("public "),
    };
    // Python
    if let Some(rest) = t.strip_prefix("def ") {
        let name: String = rest
            .chars()
            .take_while(|c| *c != '(' && *c != ':')
            .collect();
        let name = name.trim().to_string();
        if !name.is_empty() {
            let is_test = name.starts_with("test_");
            return Some(Symbol {
                name: name.clone(),
                kind: if is_test {
                    SymbolKind::Test
                } else {
                    SymbolKind::Function
                },
                file: path.to_string(),
                start_line: ln,
                end_line: ln,
                signature: format!("def {name}(…)"),
                is_test,
                is_public: !name.starts_with('_'),
            });
        }
    }
    if let Some(rest) = t.strip_prefix("class ") {
        let name: String = rest
            .chars()
            .take_while(|c| *c != '(' && *c != ':' && !c.is_whitespace())
            .collect();
        if !name.is_empty() {
            return Some(mk(name.clone(), SymbolKind::Class, format!("class {name}")));
        }
    }
    // C# / Java / Go / C++ function-ish: `... Name(...) {`
    if (t.contains('(') && t.contains(')'))
        && (t.contains("public ")
            || t.contains("private ")
            || t.contains("func ")
            || t.contains("void ")
            || t.ends_with('{'))
    {
        // Grab token before '('.
        if let Some(paren) = t.find('(') {
            let before = t[..paren].trim();
            if let Some(name) = before.split_whitespace().last() {
                let name = name.trim_matches(|c| c == '*' || c == '&').to_string();
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    let skip = ["if", "for", "while", "switch", "catch", "return"];
                    if !skip.contains(&name.as_str()) {
                        return Some(mk(name.clone(), SymbolKind::Function, t.to_string()));
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Symbol diffing
// ---------------------------------------------------------------------------

/// Match old/new symbol tables for one file and emit SymbolChanges.
/// Matching key: (kind, name). Signature comparison + body-line overlap
/// determine Modified vs SignatureChanged. Unmatched pairs with similar
/// names are reported as Renamed at lower confidence.
pub fn diff_symbols(
    file: &str,
    old_syms: &[Symbol],
    new_syms: &[Symbol],
    old_body: Option<&str>,
    new_body: Option<&str>,
) -> Vec<SymbolChange> {
    let mut out = Vec::new();
    let mut old_map: HashMap<(String, String), &Symbol> = HashMap::new();
    for s in old_syms {
        old_map.insert((format!("{:?}", s.kind), s.name.clone()), s);
    }
    let mut new_map: HashMap<(String, String), &Symbol> = HashMap::new();
    for s in new_syms {
        new_map.insert((format!("{:?}", s.kind), s.name.clone()), s);
    }
    for (k, ns) in &new_map {
        match old_map.get(k) {
            None => {
                // Possible rename? Look for same-kind symbol with similar name
                // that vanished.
                let rename = old_map.iter().find(|(ok, _)| {
                    ok.0 == k.0 && !new_map.contains_key(*ok) && str_sim(&ok.1, &k.1) > 0.6
                });
                if let Some((_, os)) = rename {
                    out.push(SymbolChange {
                        file: file.to_string(),
                        name: format!("{} → {}", os.name, ns.name),
                        kind: ns.kind,
                        change: SymbolChangeKind::Renamed,
                        confidence: 0.7,
                        old_signature: Some(os.signature.clone()),
                        new_signature: Some(ns.signature.clone()),
                        old_lines: Some((os.start_line, os.end_line)),
                        new_lines: Some((ns.start_line, ns.end_line)),
                        evidence: vec![format!(
                            "same-kind symbol with similar name ({} vs {})",
                            os.name, ns.name
                        )],
                    });
                } else {
                    out.push(SymbolChange {
                        file: file.to_string(),
                        name: ns.name.clone(),
                        kind: ns.kind,
                        change: SymbolChangeKind::Added,
                        confidence: 0.98,
                        old_signature: None,
                        new_signature: Some(ns.signature.clone()),
                        old_lines: None,
                        new_lines: Some((ns.start_line, ns.end_line)),
                        evidence: vec!["no matching symbol in base version".to_string()],
                    });
                }
            }
            Some(os) => {
                if os.signature != ns.signature {
                    out.push(SymbolChange {
                        file: file.to_string(),
                        name: ns.name.clone(),
                        kind: ns.kind,
                        change: SymbolChangeKind::SignatureChanged,
                        confidence: 0.95,
                        old_signature: Some(os.signature.clone()),
                        new_signature: Some(ns.signature.clone()),
                        old_lines: Some((os.start_line, os.end_line)),
                        new_lines: Some((ns.start_line, ns.end_line)),
                        evidence: vec![format!(
                            "signature: '{}' → '{}'",
                            os.signature, ns.signature
                        )],
                    });
                } else if bodies_differ(old_body, new_body, os, ns) {
                    out.push(SymbolChange {
                        file: file.to_string(),
                        name: ns.name.clone(),
                        kind: ns.kind,
                        change: SymbolChangeKind::Modified,
                        confidence: 0.9,
                        old_signature: Some(os.signature.clone()),
                        new_signature: Some(ns.signature.clone()),
                        old_lines: Some((os.start_line, os.end_line)),
                        new_lines: Some((ns.start_line, ns.end_line)),
                        evidence: vec!["body lines differ".to_string()],
                    });
                } else if os.is_public != ns.is_public {
                    out.push(SymbolChange {
                        file: file.to_string(),
                        name: ns.name.clone(),
                        kind: ns.kind,
                        change: SymbolChangeKind::VisibilityChanged,
                        confidence: 0.95,
                        old_signature: Some(os.signature.clone()),
                        new_signature: Some(ns.signature.clone()),
                        old_lines: Some((os.start_line, os.end_line)),
                        new_lines: Some((ns.start_line, ns.end_line)),
                        evidence: vec!["visibility flag changed".to_string()],
                    });
                }
            }
        }
    }
    for (k, os) in &old_map {
        if !new_map.contains_key(k) {
            // Already reported as rename target?
            let renamed = out.iter().any(|c| {
                matches!(c.change, SymbolChangeKind::Renamed)
                    && c.name.starts_with(&format!("{} →", os.name))
            });
            if !renamed {
                out.push(SymbolChange {
                    file: file.to_string(),
                    name: os.name.clone(),
                    kind: os.kind,
                    change: SymbolChangeKind::Removed,
                    confidence: 0.95,
                    old_signature: Some(os.signature.clone()),
                    new_signature: None,
                    old_lines: Some((os.start_line, os.end_line)),
                    new_lines: None,
                    evidence: vec!["symbol absent from new version".to_string()],
                });
            }
        }
    }
    out.sort_by(|a, b| a.file.cmp(&b.file).then(a.name.cmp(&b.name)));
    out
}

/// Compare symbol bodies when line ranges are known; fall back to
/// signature-only (no false Modified) when ranges are unknown.
fn bodies_differ(old_body: Option<&str>, new_body: Option<&str>, os: &Symbol, ns: &Symbol) -> bool {
    if os.start_line == 0 || ns.start_line == 0 {
        return false;
    }
    let (ob, nb) = match (old_body, new_body) {
        (Some(a), Some(b)) => (a, b),
        _ => return false,
    };
    let old_lines: Vec<&str> = ob.lines().collect();
    let new_lines: Vec<&str> = nb.lines().collect();
    let slice = |lines: &[&str], s: u32, e: u32| -> String {
        if s == 0 {
            return String::new();
        }
        let e = e.max(s);
        let a = (s.saturating_sub(1)) as usize;
        let b = (e as usize).min(lines.len());
        if a >= lines.len() || a >= b {
            return String::new();
        }
        lines[a..b].join("\n")
    };
    let o = slice(&old_lines, os.start_line, os.end_line);
    let n = slice(&new_lines, ns.start_line, ns.end_line);
    if o.is_empty() || n.is_empty() {
        return false;
    }
    normalize(&o) != normalize(&n)
}

fn normalize(s: &str) -> String {
    s.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn str_sim(a: &str, b: &str) -> f32 {
    // Jaro-ish cheap similarity: char-bigram overlap.
    if a == b {
        return 1.0;
    }
    let bigrams = |s: &str| -> Vec<String> {
        let c: Vec<char> = s.chars().collect();
        if c.len() < 2 {
            return vec![s.to_string()];
        }
        c.windows(2).map(|w| w.iter().collect()).collect()
    };
    let ab = bigrams(&a.to_lowercase());
    let bb = bigrams(&b.to_lowercase());
    if ab.is_empty() || bb.is_empty() {
        return 0.0;
    }
    let mut hits = 0;
    let mut rest = bb.clone();
    for g in &ab {
        if let Some(i) = rest.iter().position(|x| x == g) {
            hits += 1;
            rest.remove(i);
        }
    }
    2.0 * hits as f32 / (ab.len() + bb.len()) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_fn_signature_change() {
        let old = "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }";
        let new = "pub fn greet(name: &str, loud: bool) -> String { format!(\"hi {name}\") }";
        let o = extract_symbols("a.rs", Language::Rust, old);
        let n = extract_symbols("a.rs", Language::Rust, new);
        let d = diff_symbols("a.rs", &o, &n, Some(old), Some(new));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].change, SymbolChangeKind::SignatureChanged);
    }

    #[test]
    fn ts_function_added() {
        let old = "export function a() { return 1; }";
        let new = "export function a() { return 1; }\nexport function b() { return 2; }";
        let o = extract_symbols("a.ts", Language::TypeScript, old);
        let n = extract_symbols("a.ts", Language::TypeScript, new);
        let d = diff_symbols("a.ts", &o, &n, Some(old), Some(new));
        assert!(d
            .iter()
            .any(|c| c.name == "b" && matches!(c.change, SymbolChangeKind::Added)));
    }

    #[test]
    fn python_def_detected() {
        let src = "def test_login():\n    pass\n";
        let s = extract_symbols("t.py", Language::Python, src);
        assert!(s.iter().any(|x| x.is_test));
    }

    #[test]
    fn adjacent_unchanged_fn_is_quiet() {
        let old = "pub fn a() -> i32 {\n    1\n}\n\npub fn b() -> i32 {\n    2\n}\n";
        let new = "pub fn a() -> i32 {\n    1\n}\n\npub fn b() -> i32 {\n    3\n}\n";
        let o = extract_symbols("x.rs", Language::Rust, old);
        let n = extract_symbols("x.rs", Language::Rust, new);
        let ao = o.iter().find(|s| s.name == "a").expect("fn a");
        assert_eq!((ao.start_line, ao.end_line), (1, 3));
        let d = diff_symbols("x.rs", &o, &n, Some(old), Some(new));
        assert_eq!(d.len(), 1, "only b changed: {d:?}");
        assert_eq!(d[0].name, "b");
    }
}
