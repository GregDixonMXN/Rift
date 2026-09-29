//! Symbol extraction: syn for Rust (precise), tree-sitter for JS/TS,
//! Python, Go, and C#, line-regex fallback for other languages.
//! Then symbol-level diffing.

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
        Language::Python | Language::Go | Language::CSharp => extract_lang(path, language, content),
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
// Tree-sitter for Python, Go, and C#.
//
// One shared walker, since all three grammars expose the same shape:
// named declaration nodes with a `name` field and exact source ranges.
// Language-specific rules (test detection, visibility) branch on `lang.
// ---------------------------------------------------------------------------

fn lang_grammar(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Python => tree_sitter_python::language(),
        Language::Go => tree_sitter_go::language(),
        _ => tree_sitter_c_sharp::language(),
    }
}

fn extract_lang(path: &str, language: Language, content: &str) -> Vec<Symbol> {
    let mut parser = tree_sitter::Parser::new();
    let grammar = lang_grammar(language);
    if parser.set_language(&grammar).is_err() {
        return extract_generic(path, content);
    }
    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return extract_generic(path, content),
    };
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    walk_lang(tree.root_node(), path, language, bytes, None, &[], &mut out);
    out
}

/// Attribute/decorator names that mark a test, across xUnit/NUnit/MSTest.
fn is_test_attr(name: &str) -> bool {
    name.contains("Test") || name.contains("Fact") || name.contains("Theory")
}

/// Decorator/attribute names attached to a declaration node: C# keeps
/// `attribute_list` inside the node, Python wraps the definition in
/// `decorated_definition` (inherited via `attrs`).
fn node_attrs(node: &tree_sitter::Node, src: &[u8], inherited: &[String]) -> Vec<String> {
    let mut out: Vec<String> = inherited.to_vec();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_list" {
            let mut c2 = child.walk();
            for a in child.children(&mut c2) {
                if a.kind() != "attribute" {
                    continue;
                }
                let text = a
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(src).ok())
                    .unwrap_or_else(|| a.utf8_text(src).unwrap_or_default());
                out.push(text.trim_start_matches('@').to_string());
            }
        } else if child.kind() == "decorator" {
            let text = child.utf8_text(src).unwrap_or_default();
            let first = text
                .trim_start_matches('@')
                .split(['(', '.', ' '])
                .next()
                .unwrap_or_default();
            out.push(first.to_string());
            // Dotted paths (`pytest.mark.slow`): every segment can match.
            for seg in text.trim_start_matches('@').split(['.', '(']) {
                let seg = seg.trim();
                if !seg.is_empty() && seg != first {
                    out.push(seg.to_string());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `name` field text of a declaration node.
fn field_name(node: &tree_sitter::Node, src: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(src).ok())
        .map(|s| s.to_string())
}

/// Go method receiver `(s *Server)` → `Server`: last identifier wins.
fn go_receiver_type(node: &tree_sitter::Node, src: &[u8]) -> Option<String> {
    let recv = node.child_by_field_name("receiver")?;
    let text = recv.utf8_text(src).ok()?;
    text.rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// C# accessibility: a direct `modifier` child reading `public`.
fn cs_is_public(node: &tree_sitter::Node, src: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifier" && child.utf8_text(src).unwrap_or_default() == "public" {
            return true;
        }
    }
    false
}

fn cs_is_const(node: &tree_sitter::Node, src: &[u8]) -> bool {
    // Only `const` counts: `readonly` is still instance state.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifier" && child.utf8_text(src).unwrap_or_default() == "const" {
            return true;
        }
    }
    false
}

/// Display fields for one extracted symbol; ranges come from the node.
struct LangSym {
    name: String,
    kind: SymbolKind,
    signature: String,
    is_test: bool,
    is_public: bool,
}

fn push_lang_symbol(out: &mut Vec<Symbol>, path: &str, node: &tree_sitter::Node, sym: LangSym) {
    out.push(Symbol {
        name: sym.name,
        kind: if sym.is_test {
            SymbolKind::Test
        } else {
            sym.kind
        },
        file: path.to_string(),
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
        signature: sym.signature,
        is_test: sym.is_test,
        is_public: sym.is_public,
    });
}

fn walk_lang(
    node: tree_sitter::Node,
    path: &str,
    lang: Language,
    src: &[u8],
    class_ctx: Option<&str>,
    attrs: &[String],
    out: &mut Vec<Symbol>,
) {
    let kind = node.kind();
    // Children of class/struct bodies inherit the owner for `Owner::method`.
    // Decorated definitions pass their decorator names down.
    let mut next_ctx = class_ctx;
    let mut next_attrs = attrs;
    let owned_attrs: Vec<String>;
    let owned_ctx: String;

    match (lang, kind) {
        // ---- Python ----
        (Language::Python, "function_definition") => {
            if let Some(name) = field_name(&node, src) {
                let attrs = node_attrs(&node, src, attrs);
                let test = name.starts_with("test_")
                    || name.starts_with("Test")
                    || attrs.iter().any(|a| is_test_attr(a));
                let full = match class_ctx {
                    Some(c) => format!("{c}::{name}"),
                    None => name.clone(),
                };
                let kind = if class_ctx.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: full,
                        kind,
                        signature: format!("def {name}(…)"),
                        is_test: test,
                        is_public: !name.starts_with('_'),
                    },
                );
            }
        }
        (Language::Python, "class_definition") => {
            if let Some(name) = field_name(&node, src) {
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: name.clone(),
                        kind: SymbolKind::Class,
                        signature: format!("class {name}"),
                        is_test: false,
                        is_public: !name.starts_with('_'),
                    },
                );
                owned_ctx = name;
                next_ctx = Some(&owned_ctx);
                // ctx/attrs shadow dance: return early via the shared tail
                // by recursing manually here.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_lang(child, path, lang, src, next_ctx, attrs, out);
                }
                return;
            }
        }
        (Language::Python, "decorated_definition") => {
            owned_attrs = node_attrs(&node, src, attrs);
            next_attrs = &owned_attrs;
        }
        (Language::Python, "assignment") => {
            // Module-level `NAME = ...` constants only: walk up past any
            // expression wrappers; bail inside functions or classes.
            let mut p = node.parent();
            let mut top = false;
            while let Some(n) = p {
                match n.kind() {
                    "module" => {
                        top = true;
                        break;
                    }
                    "function_definition" | "class_definition" | "lambda" => break,
                    _ => p = n.parent(),
                }
            }
            if top {
                if let Some(left) = node.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        if let Ok(name) = left.utf8_text(src) {
                            push_lang_symbol(
                                out,
                                path,
                                &node,
                                LangSym {
                                    name: name.to_string(),
                                    kind: SymbolKind::Const,
                                    signature: format!("{name} = …"),
                                    is_test: false,
                                    is_public: !name.starts_with('_'),
                                },
                            );
                        }
                    }
                }
            }
        }
        // ---- Go ----
        (Language::Go, "function_declaration") => {
            if let Some(name) = field_name(&node, src) {
                let test_file = path.ends_with("_test.go");
                let test = test_file
                    || ["Test", "Example", "Benchmark", "Fuzz"]
                        .iter()
                        .any(|p| name.starts_with(p));
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: name.clone(),
                        kind: SymbolKind::Function,
                        signature: format!("func {name}(…)"),
                        is_test: test,
                        is_public: name
                            .chars()
                            .next()
                            .map(|c| c.is_uppercase())
                            .unwrap_or(false),
                    },
                );
            }
        }
        (Language::Go, "method_declaration") => {
            if let Some(name) = field_name(&node, src) {
                let full = match go_receiver_type(&node, src) {
                    Some(r) => format!("{r}::{name}"),
                    None => name.clone(),
                };
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: full,
                        kind: SymbolKind::Method,
                        signature: format!("func {name}(…)"),
                        is_test: path.ends_with("_test.go"),
                        is_public: name
                            .chars()
                            .next()
                            .map(|c| c.is_uppercase())
                            .unwrap_or(false),
                    },
                );
            }
        }
        (Language::Go, "type_declaration") => {
            // One declaration can hold several `type_spec` children.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "type_spec" {
                    continue;
                }
                if let Some(name) = field_name(&child, src) {
                    let kind = child
                        .child_by_field_name("type")
                        .map(|t| match t.kind() {
                            "struct_type" => SymbolKind::Struct,
                            "interface_type" => SymbolKind::Interface,
                            _ => SymbolKind::TypeAlias,
                        })
                        .unwrap_or(SymbolKind::TypeAlias);
                    let sig_kw = match kind {
                        SymbolKind::Struct => "struct",
                        SymbolKind::Interface => "interface",
                        _ => "type",
                    };
                    let test = false;
                    push_lang_symbol(
                        out,
                        path,
                        &child,
                        LangSym {
                            name: name.clone(),
                            kind,
                            signature: format!("type {name} {sig_kw}"),
                            is_test: test,
                            is_public: name
                                .chars()
                                .next()
                                .map(|c| c.is_uppercase())
                                .unwrap_or(false),
                        },
                    );
                }
            }
        }
        // ---- C# ----
        (Language::CSharp, "class_declaration") => {
            if let Some(name) = field_name(&node, src) {
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: name.clone(),
                        kind: SymbolKind::Class,
                        signature: format!("class {name}"),
                        is_test: false,
                        is_public: cs_is_public(&node, src),
                    },
                );
                owned_ctx = name;
                next_ctx = Some(&owned_ctx);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_lang(child, path, lang, src, next_ctx, attrs, out);
                }
                return;
            }
        }
        (Language::CSharp, "struct_declaration") => {
            if let Some(name) = field_name(&node, src) {
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: name.clone(),
                        kind: SymbolKind::Struct,
                        signature: format!("struct {name}"),
                        is_test: false,
                        is_public: cs_is_public(&node, src),
                    },
                );
                owned_ctx = name;
                next_ctx = Some(&owned_ctx);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_lang(child, path, lang, src, next_ctx, attrs, out);
                }
                return;
            }
        }
        (Language::CSharp, "interface_declaration") => {
            if let Some(name) = field_name(&node, src) {
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: name.clone(),
                        kind: SymbolKind::Interface,
                        signature: format!("interface {name}"),
                        is_test: false,
                        is_public: cs_is_public(&node, src),
                    },
                );
                owned_ctx = name;
                next_ctx = Some(&owned_ctx);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_lang(child, path, lang, src, next_ctx, attrs, out);
                }
                return;
            }
        }
        (Language::CSharp, "enum_declaration") => {
            if let Some(name) = field_name(&node, src) {
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: name.clone(),
                        kind: SymbolKind::Enum,
                        signature: format!("enum {name}"),
                        is_test: false,
                        is_public: cs_is_public(&node, src),
                    },
                );
            }
        }
        (Language::CSharp, "method_declaration") => {
            if let Some(name) = field_name(&node, src) {
                let attrs = node_attrs(&node, src, attrs);
                let full = match class_ctx {
                    Some(c) => format!("{c}::{name}"),
                    None => name.clone(),
                };
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: full,
                        kind: SymbolKind::Method,
                        signature: format!("{name}(…)"),
                        is_test: attrs.iter().any(|a| is_test_attr(a)),
                        is_public: cs_is_public(&node, src),
                    },
                );
            }
        }
        (Language::CSharp, "constructor_declaration") => {
            // `Foo::new`: unique per class, stable across edits.
            let full = match class_ctx {
                Some(c) => format!("{c}::new"),
                None => "new".to_string(),
            };
            let sig = format!("{full}(…)");
            push_lang_symbol(
                out,
                path,
                &node,
                LangSym {
                    name: full,
                    kind: SymbolKind::Method,
                    signature: sig,
                    is_test: false,
                    is_public: cs_is_public(&node, src),
                },
            );
        }
        (Language::CSharp, "property_declaration") => {
            if let Some(name) = field_name(&node, src) {
                let full = match class_ctx {
                    Some(c) => format!("{c}::{name}"),
                    None => name.clone(),
                };
                push_lang_symbol(
                    out,
                    path,
                    &node,
                    LangSym {
                        name: full,
                        kind: SymbolKind::Field,
                        signature: format!("property {name}"),
                        is_test: false,
                        is_public: cs_is_public(&node, src),
                    },
                );
            }
        }
        (Language::CSharp, "field_declaration") => {
            let is_const = cs_is_const(&node, src);
            let public = cs_is_public(&node, src);
            // Declarators nest inside `variable_declaration`.
            let mut declarators = Vec::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    declarators.push(child);
                } else if child.kind() == "variable_declaration" {
                    let mut c2 = child.walk();
                    for d in child.children(&mut c2) {
                        if d.kind() == "variable_declarator" {
                            declarators.push(d);
                        }
                    }
                }
            }
            for child in declarators {
                if let Some(name) = field_name(&child, src) {
                    let full = match class_ctx {
                        Some(c) => format!("{c}::{name}"),
                        None => name.clone(),
                    };
                    push_lang_symbol(
                        out,
                        path,
                        &child,
                        LangSym {
                            name: full,
                            kind: if is_const {
                                SymbolKind::Const
                            } else {
                                SymbolKind::Field
                            },
                            signature: format!("field {name}"),
                            is_test: false,
                            is_public: public,
                        },
                    );
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_lang(child, path, lang, src, next_ctx, next_attrs, out);
    }
}

// ---------------------------------------------------------------------------
// Generic regex fallback (Java/C/C++ and unparseable files)
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

/// Call-site names inside a body: identifiers immediately followed by `(`.
/// Used to link tests to the symbols they exercise. Keywords and obvious
/// non-calls are filtered; the result is sorted and deduplicated.
pub fn called_names(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            if chars.get(i) == Some(&'(') && !is_call_keyword(&name) && !name.starts_with('_') {
                out.push(name);
            }
            continue;
        }
        i += 1;
    }
    out.sort();
    out.dedup();
    out
}

fn is_call_keyword(name: &str) -> bool {
    matches!(
        name,
        "if" | "else"
            | "for"
            | "while"
            | "loop"
            | "match"
            | "return"
            | "use"
            | "mod"
            | "fn"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "const"
            | "static"
            | "let"
            | "mut"
            | "ref"
            | "move"
            | "where"
            | "async"
            | "await"
            | "crate"
            | "self"
            | "Self"
            | "super"
            | "extern"
            | "macro"
            | "function"
            | "class"
            | "extends"
            | "new"
            | "typeof"
            | "instanceof"
            | "in"
            | "of"
            | "do"
            | "switch"
            | "case"
            | "try"
            | "catch"
            | "finally"
            | "throw"
            | "def"
            | "lambda"
            | "import"
            | "from"
            | "with"
            | "as"
            | "assert"
            | "sizeof"
            | "true"
            | "false"
            | "print"
            | "println"
    )
}

/// Imported names per file, for dependency edges. Best-effort per language
/// family: answers "does this file import `name`?" without full resolution.
/// Sorted and deduplicated.
pub fn imported_names(lang: Language, content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        match lang {
            Language::Rust => {
                if let Some(rest) = t.strip_prefix("use ") {
                    let rest = rest.trim_end_matches(';');
                    for id in ident_list(rest) {
                        if !matches!(id.as_str(), "crate" | "self" | "super" | "Self") {
                            out.push(id);
                        }
                    }
                }
            }
            Language::JavaScript | Language::TypeScript => {
                if t.starts_with("import ") || t.contains("require(") {
                    for id in ident_list(t) {
                        if !matches!(
                            id.as_str(),
                            "import"
                                | "from"
                                | "export"
                                | "as"
                                | "default"
                                | "require"
                                | "const"
                                | "let"
                                | "var"
                        ) {
                            out.push(id);
                        }
                    }
                }
            }
            Language::Python if t.starts_with("import ") || t.starts_with("from ") => {
                for id in ident_list(t) {
                    if !matches!(id.as_str(), "import" | "from" | "as") {
                        out.push(id);
                    }
                }
            }
            Language::Python => {}
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Bare identifiers in a snippet (dotted paths split: `a.b` → a, b).
fn ident_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Does a test name target a symbol? `test_login`, `login_test`,
/// `testLogin`, `TestLogin` all target `login` (case-insensitive).
/// Bare affixes without a separator only count on a camel boundary, so
/// `testing` is not a test for `ing`.
pub fn test_targets(test_name: &str, symbol_name: &str) -> bool {
    if test_name.eq_ignore_ascii_case(symbol_name) {
        return true;
    }
    let want = symbol_name.to_lowercase();
    let mut cores: Vec<&str> = Vec::new();
    for p in ["test_", "should_"] {
        if let Some(r) = test_name.strip_prefix(p) {
            cores.push(r);
        }
    }
    if let Some(r) = test_name.strip_suffix("_test") {
        cores.push(r);
    }
    for p in ["test", "Test"] {
        if let Some(r) = test_name.strip_prefix(p) {
            if r.starts_with(|c: char| c.is_ascii_uppercase()) {
                cores.push(r);
            }
        }
    }
    if let Some(r) = test_name.strip_suffix("Test") {
        if !r.is_empty() {
            cores.push(r);
        }
    }
    cores.iter().any(|c| c.eq_ignore_ascii_case(&want))
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
    // Split once: body comparison touches every matched pair, so per-pair
    // `lines().collect()` goes superlinear on symbol-dense files.
    let old_table: Vec<&str> = old_body.map(|b| b.lines().collect()).unwrap_or_default();
    let new_table: Vec<&str> = new_body.map(|b| b.lines().collect()).unwrap_or_default();
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
                } else if bodies_differ(&old_table, &new_table, os, ns) {
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
fn bodies_differ(old_lines: &[&str], new_lines: &[&str], os: &Symbol, ns: &Symbol) -> bool {
    if os.start_line == 0 || ns.start_line == 0 {
        return false;
    }
    if old_lines.is_empty() || new_lines.is_empty() {
        return false;
    }
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
    let o = slice(old_lines, os.start_line, os.end_line);
    let n = slice(new_lines, ns.start_line, ns.end_line);
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

    #[test]
    fn called_names_finds_calls_not_keywords() {
        let body = "if ready {\n    let v = login(user);\n    store.save(v);\n    return v;\n}";
        let calls = called_names(body);
        assert!(calls.contains(&"login".to_string()), "{calls:?}");
        assert!(calls.contains(&"save".to_string()), "{calls:?}");
        assert!(!calls.contains(&"if".to_string()), "{calls:?}");
        assert!(!calls.contains(&"return".to_string()), "{calls:?}");
    }

    #[test]
    fn test_target_patterns() {
        assert!(test_targets("test_login", "login"));
        assert!(test_targets("login_test", "login"));
        assert!(test_targets("testLogin", "login"));
        assert!(test_targets("TestLogin", "login"));
        assert!(test_targets("should_reject_expired", "reject_expired"));
        assert!(!test_targets("test_login", "logout"));
        assert!(!test_targets("testing", "ing"));
    }

    #[test]
    fn imported_names_per_language() {
        let rs = "use crate::auth::{login, session};\nuse super::util;\nfn f() {}";
        let ids = imported_names(Language::Rust, rs);
        assert!(ids.contains(&"login".to_string()), "{ids:?}");
        assert!(ids.contains(&"session".to_string()), "{ids:?}");
        assert!(!ids.contains(&"crate".to_string()), "{ids:?}");

        let js = "import { login } from './auth';\nimport def from 'x';\nconst y = login();";
        let ids = imported_names(Language::JavaScript, js);
        assert!(ids.contains(&"login".to_string()), "{ids:?}");
        assert!(!ids.contains(&"from".to_string()), "{ids:?}");

        let py = "from .auth import login\nimport os\n";
        let ids = imported_names(Language::Python, py);
        assert!(ids.contains(&"login".to_string()), "{ids:?}");
        assert!(!ids.contains(&"from".to_string()), "{ids:?}");
    }

    fn syms(path: &str, lang: Language, src: &str) -> Vec<(String, SymbolKind, u32, u32)> {
        extract_symbols(path, lang, src)
            .into_iter()
            .map(|s| (s.name, s.kind, s.start_line, s.end_line))
            .collect()
    }

    #[test]
    fn python_class_methods_functions_consts() {
        let src = "SESSION_TIMEOUT = 900\n\n\nclass Auth:\n    def login(self, user):\n        return True\n\n    def _helper(self):\n        return False\n\n\ndef check(token):\n    return True\n";
        let got = syms("a.py", Language::Python, src);
        assert!(
            got.contains(&("Auth".to_string(), SymbolKind::Class, 4, 9)),
            "{got:?}"
        );
        assert!(
            got.contains(&("Auth::login".to_string(), SymbolKind::Method, 5, 6)),
            "{got:?}"
        );
        assert!(
            got.contains(&("check".to_string(), SymbolKind::Function, 12, 13)),
            "{got:?}"
        );
        assert!(
            got.contains(&("SESSION_TIMEOUT".to_string(), SymbolKind::Const, 1, 1)),
            "{got:?}"
        );
        // _helper is private but still extracted.
        let all = extract_symbols("a.py", Language::Python, src);
        let helper = all.iter().find(|s| s.name == "Auth::_helper").unwrap();
        assert!(!helper.is_public);
    }

    #[test]
    fn python_tests_detected() {
        let src = "def test_login():\n    assert True\n\ndef helper():\n    pass\n";
        let all = extract_symbols("t.py", Language::Python, src);
        let t = all.iter().find(|s| s.name == "test_login").unwrap();
        assert!(t.is_test && t.kind == SymbolKind::Test);
        let h = all.iter().find(|s| s.name == "helper").unwrap();
        assert!(!h.is_test);
    }

    #[test]
    fn python_adjacent_fn_stays_quiet() {
        let old = "def a():\n    return 1\n\ndef b():\n    return 2\n";
        let new = "def a():\n    return 1\n\ndef b():\n    return 3\n";
        let old_syms = extract_symbols("f.py", Language::Python, old);
        let new_syms = extract_symbols("f.py", Language::Python, new);
        let d = diff_symbols("f.py", &old_syms, &new_syms, Some(old), Some(new));
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].name, "b");
        assert!(matches!(d[0].change, SymbolChangeKind::Modified));
    }

    #[test]
    fn go_funcs_methods_types() {
        let src = "package auth\n\ntype Server struct {\n    Port int\n}\n\nfunc login(user string) bool {\n    return true\n}\n\nfunc (s *Server) handle() {\n}\n\nfunc helper() {}\n";
        let got = syms("a.go", Language::Go, src);
        assert!(
            got.contains(&("Server".to_string(), SymbolKind::Struct, 3, 5)),
            "{got:?}"
        );
        assert!(
            got.contains(&("login".to_string(), SymbolKind::Function, 7, 9)),
            "{got:?}"
        );
        assert!(
            got.contains(&("Server::handle".to_string(), SymbolKind::Method, 11, 12)),
            "{got:?}"
        );
        let all = extract_symbols("a.go", Language::Go, src);
        assert!(!all.iter().find(|s| s.name == "helper").unwrap().is_public);
        assert!(all.iter().find(|s| s.name == "Server").unwrap().is_public);
    }

    #[test]
    fn go_test_funcs_detected() {
        let src = "package auth\n\nfunc TestLogin(t *testing.T) {}\n\nfunc setup() {}\n";
        let all = extract_symbols("a_test.go", Language::Go, src);
        let t = all.iter().find(|s| s.name == "TestLogin").unwrap();
        assert!(t.is_test && t.kind == SymbolKind::Test);
        // Helpers in _test.go files count as test helpers.
        assert!(all.iter().find(|s| s.name == "setup").unwrap().is_test);
    }

    #[test]
    fn csharp_class_members_and_tests() {
        let src = "public class Auth {\n    private readonly int _retries;\n    public string Name { get; set; }\n    public Auth() {}\n    public bool Login(string user) {\n        return true;\n    }\n    [Fact]\n    public void Login_works() {}\n}\n";
        let all = extract_symbols("A.cs", Language::CSharp, src);
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        for want in [
            "Auth",
            "Auth::_retries",
            "Auth::Name",
            "Auth::new",
            "Auth::Login",
        ] {
            assert!(names.contains(&want), "{names:?}");
        }
        let t = all.iter().find(|s| s.name == "Auth::Login_works").unwrap();
        assert!(t.is_test && t.kind == SymbolKind::Test, "{t:?}");
        assert!(
            !all.iter()
                .find(|s| s.name == "Auth::_retries")
                .unwrap()
                .is_public
        );
        assert!(
            all.iter()
                .find(|s| s.name == "Auth::Login")
                .unwrap()
                .is_public
        );
        let field = all.iter().find(|s| s.name == "Auth::_retries").unwrap();
        assert_eq!(field.kind, SymbolKind::Field);
    }
}
