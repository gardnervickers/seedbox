//! Syntax-aware no-sleep rule for test code.
use proc_macro2::{Span, TokenStream, TokenTree};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use syn::{
    spanned::Spanned,
    visit::{self, Visit},
    Expr, Item, Meta, Stmt, UseTree,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: PathBuf,
    pub line: usize,
    pub detail: String,
}
impl std::fmt::Display for Finding {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "{}:{}: {}",
            self.path.display(),
            self.line,
            self.detail
        )
    }
}
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

fn test_gate(meta: &Meta, negative: bool) -> bool {
    match meta {
        Meta::Path(path) => !negative && path.is_ident("test"),
        Meta::NameValue(value) => {
            !negative
                && value.path.is_ident("feature")
                && matches!(&value.value, Expr::Lit(lit) if matches!(&lit.lit, syn::Lit::Str(s) if s.value() == "test-support"))
        }
        Meta::List(list) => {
            use syn::parse::Parser;
            let parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
            parser.parse2(list.tokens.clone()).is_ok_and(|items| {
                items
                    .iter()
                    .any(|item| test_gate(item, negative ^ list.path.is_ident("not")))
            })
        }
    }
}
fn test_attrs(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path()
            .segments
            .last()
            .is_some_and(|s| s.ident == "test")
            || (attr.path().is_ident("cfg")
                && attr
                    .parse_args::<Meta>()
                    .is_ok_and(|meta| test_gate(&meta, false)))
    })
}
fn attrs(item: &Item) -> &[syn::Attribute] {
    match item {
        Item::Const(v) => &v.attrs,
        Item::Enum(v) => &v.attrs,
        Item::ExternCrate(v) => &v.attrs,
        Item::Fn(v) => &v.attrs,
        Item::ForeignMod(v) => &v.attrs,
        Item::Impl(v) => &v.attrs,
        Item::Macro(v) => &v.attrs,
        Item::Mod(v) => &v.attrs,
        Item::Static(v) => &v.attrs,
        Item::Struct(v) => &v.attrs,
        Item::Trait(v) => &v.attrs,
        Item::TraitAlias(v) => &v.attrs,
        Item::Type(v) => &v.attrs,
        Item::Union(v) => &v.attrs,
        Item::Use(v) => &v.attrs,
        _ => &[],
    }
}
fn expand_alias(aliases: &BTreeMap<String, String>, path: &str) -> String {
    let mut path = path.to_owned();
    let mut seen = BTreeSet::new();
    loop {
        if let Some(rest) = path
            .strip_prefix("self::")
            .or_else(|| path.strip_prefix("super::"))
        {
            path = rest.into();
            continue;
        }
        let (first, rest) = path.split_once("::").unwrap_or((&path, ""));
        if !seen.insert(first.to_owned()) {
            break;
        }
        let Some(prefix) = aliases.get(first) else {
            break;
        };
        let next = if rest.is_empty() {
            prefix.clone()
        } else {
            format!("{prefix}::{rest}")
        };
        if next == path {
            break;
        }
        path = next;
    }
    path
}
fn import_target(aliases: &BTreeMap<String, String>, target: String) -> String {
    if target.starts_with("super::") {
        expand_alias(aliases, &target)
    } else {
        target
    }
}
fn paths(tree: &UseTree, prefix: &str, aliases: &mut BTreeMap<String, String>) {
    match tree {
        UseTree::Path(path) => paths(&path.tree, &format!("{prefix}{}::", path.ident), aliases),
        UseTree::Name(name) => {
            if name.ident == "self" {
                let target = prefix.trim_end_matches("::");
                if let Some(name) = target.rsplit("::").next() {
                    let resolved = import_target(aliases, target.into());
                    aliases.insert(name.into(), resolved);
                }
            } else {
                let target = import_target(aliases, format!("{prefix}{}", name.ident));
                aliases.insert(name.ident.to_string(), target);
            }
        }
        UseTree::Rename(rename) => {
            let target = if rename.ident == "self" {
                prefix.trim_end_matches("::").into()
            } else {
                format!("{prefix}{}", rename.ident)
            };
            let target = import_target(aliases, target);
            aliases.insert(rename.rename.to_string(), target);
        }
        UseTree::Group(group) => {
            for item in &group.items {
                paths(item, prefix, aliases);
            }
        }
        UseTree::Glob(_) => {
            let prefix = expand_alias(aliases, prefix.trim_end_matches("::"));
            let names: &[&str] = match prefix.as_str() {
                "std::thread" | "tokio::time" | "async_std::task" => &["sleep", "sleep_until"],
                "libc" => &["sleep", "usleep", "nanosleep", "clock_nanosleep"],
                _ => &[],
            };
            for name in names {
                aliases.insert((*name).into(), format!("{prefix}::{name}"));
            }
        }
    }
}
fn imports(items: &[Item], aliases: &mut BTreeMap<String, String>) {
    // Rust imports are not source-order dependent, including aliased globs.
    for _ in 0..=items.len() {
        let before = aliases.clone();
        for item in items {
            if let Item::Use(item) = item {
                paths(&item.tree, "", aliases);
            }
        }
        if *aliases == before {
            break;
        }
    }
}

struct Policy {
    test: bool,
    aliases: BTreeMap<String, String>,
    findings: Vec<(usize, String)>,
    seen: BTreeSet<(usize, usize)>,
}
impl Policy {
    fn report(&mut self, span: Span, path: &str) {
        let start = span.start();
        if self.test && self.seen.insert((start.line, start.column)) {
            self.findings.push((
                start.line,
                format!("sleep call {path} in test code; wait on a signal instead"),
            ));
        }
    }
    fn sleep_path(&self, path: &str) -> bool {
        let resolved = expand_alias(&self.aliases, path);
        matches!(
            resolved.as_str(),
            "std::thread::sleep"
                | "std::thread::sleep_until"
                | "tokio::time::sleep"
                | "tokio::time::sleep_until"
                | "async_std::task::sleep"
                | "libc::sleep"
                | "libc::usleep"
                | "libc::nanosleep"
                | "libc::clock_nanosleep"
        )
    }
    // Macro grammars such as select! are not Rust expressions. Inspect calls
    // recursively, but never interpret string literals or ordinary data as code.
    fn macro_tokens(&mut self, stream: TokenStream) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (i, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                self.macro_tokens(group.stream());
            }
            let TokenTree::Ident(ident) = token else {
                continue;
            };
            if i >= 3
                && matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == ':')
                && matches!(&tokens[i - 2], TokenTree::Punct(p) if p.as_char() == ':')
                && matches!(&tokens[i - 3], TokenTree::Ident(_))
            {
                continue;
            }
            if i > 0 && matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == '.') {
                continue; // A receiver method is not a resolved free-function import.
            }
            let mut path = ident.to_string();
            let mut end = i + 1;
            while matches!(tokens.get(end), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                && matches!(tokens.get(end + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
            {
                if let Some(TokenTree::Ident(part)) = tokens.get(end + 2) {
                    path.push_str("::");
                    path.push_str(&part.to_string());
                    end += 3;
                } else {
                    break;
                }
            }
            if self.sleep_path(&path)
                && matches!(tokens.get(end), Some(TokenTree::Group(g)) if g.delimiter() == proc_macro2::Delimiter::Parenthesis)
            {
                self.report(ident.span(), &path);
            }
        }
    }
}
impl<'ast> Visit<'ast> for Policy {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let before = self.aliases.clone();
        if let Some((_, items)) = &item.content {
            imports(items, &mut self.aliases);
        }
        visit::visit_item_mod(self, item);
        self.aliases = before;
    }
    fn visit_block(&mut self, block: &'ast syn::Block) {
        let before = self.aliases.clone();
        for _ in 0..=block.stmts.len() {
            let previous = self.aliases.clone();
            for stmt in &block.stmts {
                if let Stmt::Item(Item::Use(item)) = stmt {
                    paths(&item.tree, "", &mut self.aliases);
                }
            }
            if self.aliases == previous {
                break;
            }
        }
        visit::visit_block(self, block);
        self.aliases = before;
    }
    fn visit_item(&mut self, item: &'ast Item) {
        let before = self.test;
        self.test |= test_attrs(attrs(item));
        visit::visit_item(self, item);
        self.test = before;
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let before = self.test;
        self.test |= test_attrs(&item.attrs);
        visit::visit_impl_item_fn(self, item);
        self.test = before;
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        let before = self.test;
        self.test |= test_attrs(&item.attrs);
        visit::visit_trait_item_fn(self, item);
        self.test = before;
    }
    fn visit_expr_call(&mut self, expr: &'ast syn::ExprCall) {
        let mut function = expr.func.as_ref();
        loop {
            function = match function {
                Expr::Paren(expr) => &expr.expr,
                Expr::Group(expr) => &expr.expr,
                _ => break,
            };
        }
        if let Expr::Path(function) = function {
            let path = function
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            if self.sleep_path(&path) {
                self.report(function.span(), &path);
            }
        }
        visit::visit_expr_call(self, expr);
    }
    fn visit_macro(&mut self, item: &'ast syn::Macro) {
        if self.test {
            self.macro_tokens(item.tokens.clone());
        }
    }
}

fn forced(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.split('/')
        .any(|part| matches!(part, "tests" | "test_support"))
        || path
            .file_name()
            .is_some_and(|name| name == "tests.rs" || name.to_string_lossy().ends_with("_tests.rs"))
}

#[cfg(test)]
fn source(path: &Path, text: &str, test_context: bool) -> Result<Vec<Finding>, String> {
    source_in_context(path, text, test_context, &BTreeMap::new()).map(|report| report.findings)
}
fn source_in_context(
    path: &Path,
    text: &str,
    test_context: bool,
    inherited: &BTreeMap<String, String>,
) -> Result<Report, String> {
    let ast = syn::parse_file(text).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut aliases = inherited.clone();
    imports(&ast.items, &mut aliases);
    let mut policy = Policy {
        test: test_context || forced(path) || test_attrs(&ast.attrs),
        aliases,
        findings: Vec::new(),
        seen: BTreeSet::new(),
    };
    policy.visit_file(&ast);
    Ok(Report {
        findings: policy
            .findings
            .into_iter()
            .map(|(line, detail)| Finding {
                path: path.into(),
                line,
                detail,
            })
            .collect(),
    })
}

fn collect(
    directory: &Path,
    visited: &mut BTreeSet<PathBuf>,
    files: &mut BTreeMap<PathBuf, String>,
) -> Result<(), String> {
    if !directory.is_dir() {
        return Ok(());
    }
    let canonical = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !visited.insert(canonical) {
        return Ok(());
    }
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            let name = path.file_name().unwrap().to_string_lossy();
            if !matches!(name.as_ref(), ".git" | "build" | "node_modules")
                && !name.starts_with("target")
            {
                collect(&path, visited, files)?;
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let canonical = path.canonicalize().map_err(|error| error.to_string())?;
            files
                .entry(canonical)
                .or_insert(fs::read_to_string(&path).map_err(|error| error.to_string())?);
        }
    }
    Ok(())
}

// Infer external module contexts too: cfg(test) mod helper; must not evade the
// rule merely because helper.rs is not named tests.rs or under a tests directory.
fn module_contexts(
    items: &[Item],
    directory: &Path,
    source_directory: &Path,
    test: bool,
    force: &mut BTreeSet<PathBuf>,
    aliases: &BTreeMap<String, String>,
    inherited: &mut BTreeMap<PathBuf, BTreeMap<String, String>>,
) {
    let mut scope = aliases.clone();
    imports(items, &mut scope);
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        let test = test || test_attrs(&module.attrs);
        let name = module.ident.to_string();
        if let Some((_, items)) = &module.content {
            module_contexts(
                items,
                &directory.join(name),
                source_directory,
                test,
                force,
                &scope,
                inherited,
            );
        } else {
            let override_path = module.attrs.iter().find_map(|attr| {
                if !attr.path().is_ident("path") {
                    return None;
                }
                match &attr.meta {
                    Meta::NameValue(value) => match &value.value {
                        Expr::Lit(lit) => match &lit.lit {
                            syn::Lit::Str(s) => Some(vec![
                                source_directory.join(s.value()),
                                directory.join(s.value()),
                            ]),
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                }
            });
            let candidates = match override_path {
                Some(paths) => paths,
                None => vec![
                    directory.join(format!("{name}.rs")),
                    directory.join(name).join("mod.rs"),
                ],
            };
            for path in candidates {
                if let Ok(path) = path.canonicalize() {
                    if test {
                        force.insert(path.clone());
                    }
                    let target = inherited.entry(path).or_default();
                    for (name, value) in &scope {
                        let value = expand_alias(&scope, value);
                        let known = |value: &str| {
                            value == "std"
                                || value == "tokio"
                                || value.starts_with("std::")
                                || value.starts_with("tokio::")
                                || value.starts_with("async_std::")
                                || value == "libc"
                                || value.starts_with("libc::")
                        };
                        target
                            .entry(name.clone())
                            .and_modify(|old| {
                                if (known(&value) && !known(old))
                                    || (known(&value) == known(old) && value < *old)
                                {
                                    *old = value.clone();
                                }
                            })
                            .or_insert(value);
                    }
                }
            }
        }
    }
}

pub fn check(root: &Path) -> Result<Report, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let root = root.as_path();
    let mut files = BTreeMap::new();
    collect(&root.join("crates"), &mut BTreeSet::new(), &mut files)?;
    // Only the downstream suite is test policy input, never upstream DR tests.
    let dr_suite = root.join("vendor/dynamorio/seedbox-tests");
    if root.join("crates/seedbox-machine/Cargo.toml").is_file()
        && !dr_suite.join("dynamorio.rs").is_file()
    {
        return Err("DynamoRIO fork tests missing; run git submodule update --init --recursive vendor/dynamorio from the Seedbox checkout".into());
    }
    let mut dr_files = BTreeMap::new();
    collect(&dr_suite, &mut BTreeSet::new(), &mut dr_files)?;
    let mut force: BTreeSet<_> = files.keys().filter(|path| forced(path)).cloned().collect();
    force.extend(dr_files.keys().cloned());
    files.extend(dr_files);
    let asts: BTreeMap<_, _> = files
        .iter()
        .map(|(path, text)| {
            syn::parse_file(text)
                .map(|ast| (path.clone(), ast))
                .map_err(|error| format!("{}: {error}", path.display()))
        })
        .collect::<Result<_, _>>()?;
    let mut inherited = BTreeMap::<PathBuf, BTreeMap<String, String>>::new();
    loop {
        let before = force.len();
        let previous_aliases = inherited.clone();
        for (path, ast) in &asts {
            if test_attrs(&ast.attrs) {
                force.insert(path.clone());
            }
            let aliases = inherited.get(path).cloned().unwrap_or_default();
            let directory = if path
                .file_name()
                .is_some_and(|name| matches!(name.to_str(), Some("lib.rs" | "main.rs" | "mod.rs")))
            {
                path.parent().unwrap().to_owned()
            } else {
                path.with_extension("")
            };
            module_contexts(
                &ast.items,
                &directory,
                path.parent().unwrap(),
                force.contains(path),
                &mut force,
                &aliases,
                &mut inherited,
            );
        }
        if force.len() == before && inherited == previous_aliases {
            break;
        }
    }
    let mut report = Report::default();
    for (path, text) in files {
        let context = force.contains(&path);
        let local = source_in_context(
            path.strip_prefix(root).unwrap_or(&path),
            &text,
            context,
            &inherited.get(&path).cloned().unwrap_or_default(),
        )?;
        report.findings.extend(local.findings);
    }
    dr_fixture_sources(root, &dr_suite, &mut report)?;
    report
        .findings
        .sort_by(|a, b| (&a.path, a.line, &a.detail).cmp(&(&b.path, b.line, &b.detail)));
    Ok(report)
}

// The exception is limited to the native DR syscall subject, not Rust timer
// helpers or incidental application pacing. Keep its bound/invariant adjacent
// to one call; a marker cannot exempt another call or an entire file/function.
fn dr_c_subject(marker: &str, syscall: &str) -> bool {
    let Some(marker) = marker.trim().strip_prefix("seedbox-syscall-subject:") else {
        return false;
    };
    let fields: Vec<_> = marker.split(';').map(str::trim).collect();
    fields.len() == 3
        && fields[0] == syscall
        && fields[1]
            .strip_prefix("bound-ms=")
            .is_some_and(|n| n.parse::<u64>().is_ok_and(|bound| bound > 0))
        && fields[2]
            .strip_prefix("invariant=")
            .is_some_and(|s| !s.trim().is_empty())
}

fn dr_fixture_findings(path: &Path, source: &str) -> Vec<Finding> {
    // Lex comments and quoted literals rather than treating diagnostic strings,
    // source provenance or commented examples as executable calls.
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("c");
    let c = matches!(extension, "c" | "h");
    let assembly = matches!(extension, "S" | "s");
    let bytes = source.as_bytes();
    let mut tokens = Vec::<(String, usize)>::new();
    let (mut i, mut line) = (0, 1);
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if (extension == "py" && bytes[i] == b'#')
            || (assembly
                && bytes[i] == b'#'
                && source[..i]
                    .rsplit('\n')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .is_empty())
        {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") || bytes[i..].starts_with(b"//") {
            let block = bytes[i + 1] == b'*';
            let start = i + 2;
            let comment_line = line;
            i = start;
            while i < bytes.len()
                && !(if block {
                    bytes[i..].starts_with(b"*/")
                } else {
                    bytes[i] == b'\n'
                })
            {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            let comment = source[start..i].trim();
            if c && comment.starts_with("seedbox-syscall-subject:") {
                tokens.push((comment.to_owned(), comment_line));
            }
            if block {
                i = (i + 2).min(bytes.len());
            }
            continue;
        }
        if matches!(bytes[i], b'"' | b'\'' | b'`') {
            let quote = bytes[i];
            let width = if extension == "py" && bytes[i..].starts_with(&[quote; 3]) {
                3
            } else {
                1
            };
            i += width;
            while i < bytes.len() {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i..].starts_with(&[quote; 3][..width]) {
                    i += width;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            tokens.push((source[start..i].to_owned(), line));
        } else {
            if !bytes[i].is_ascii_whitespace() {
                tokens.push(((bytes[i] as char).to_string(), line));
            }
            i += 1;
        }
    }
    let mut marker = None;
    let mut findings = Vec::new();
    for (index, (token, line)) in tokens.iter().enumerate() {
        if token.starts_with("seedbox-syscall-subject:") {
            marker = Some(token.as_str());
            continue;
        }
        if matches!(token.as_str(), ";" | "{" | "}") {
            marker = None;
        }
        let previous = index.checked_sub(1).map(|i| tokens[i].0.as_str());
        let call = tokens.get(index + 1).map(|t| t.0.as_str()) == Some("(");
        let args = if call {
            dr_call_args(&tokens, index + 1)
        } else {
            None
        };
        // Ordinary prototypes/definitions have a return type before the name
        // and typed parameters. Cast arguments in an actual call must still fail.
        let declaration = matches!(extension, "c" | "h" | "java" | "cs")
            && matches!(
                previous,
                Some("void" | "int" | "unsigned" | "long" | "bool")
            )
            && args.as_ref().is_some_and(|args| {
                args.iter().all(|arg| {
                    arg.is_empty()
                        || (!arg.iter().any(|s| s == "(")
                            && arg.iter().any(|s| {
                                matches!(
                                    s.as_str(),
                                    "void"
                                        | "int"
                                        | "unsigned"
                                        | "long"
                                        | "struct"
                                        | "const"
                                        | "bool"
                                )
                            }))
                })
            });
        if declaration {
            marker = None;
            continue;
        }
        let reference = c && !call && matches!(previous, Some("=" | "&"));
        let assembly_reference = assembly
            && (matches!(previous, Some("call" | "callq" | "bl" | "$"))
                || token.starts_with("SYS_")
                || token.starts_with("__NR_"));
        let name = if token == "syscall" && call {
            args.as_ref().and_then(|args| args.first()).and_then(|arg| {
                arg.iter()
                    .find_map(|t| t.strip_prefix("SYS_").or_else(|| t.strip_prefix("__NR_")))
            })
        } else if assembly_reference {
            Some(
                token
                    .strip_prefix("SYS_")
                    .or_else(|| token.strip_prefix("__NR_"))
                    .unwrap_or(token),
            )
        } else if call || reference {
            Some(token.as_str())
        } else {
            None
        };
        let Some(name) = name else { continue };
        if matches!(previous, Some("def" | "func" | "function")) {
            marker = None;
            continue;
        }
        let sleep = matches!(name, "sleep" | "usleep" | "nanosleep" | "clock_nanosleep")
            || matches!(
                (extension, name),
                ("go" | "cs", "Sleep")
                    | ("java", "sleep")
                    | ("js", "setTimeout" | "setInterval")
                    | ("cs", "Delay")
            );
        // select with descriptors waits for a positive event; the zero-descriptor
        // form is a delay. Do not reject normal bounded readiness waits.
        let select_sleep = c
            && name == "select"
            && args.as_ref().is_some_and(|args| {
                args.len() == 5
                    && args[..4]
                        .iter()
                        .all(|arg| arg.len() == 1 && matches!(arg[0].as_str(), "0" | "NULL"))
                    && args[4] != ["NULL"]
                    && args[4] != ["0"]
            });
        if !sleep && !select_sleep {
            if call && !matches!(token.as_str(), "if" | "while" | "for" | "switch") {
                marker = None;
            }
            continue;
        }
        // Only direct C calls can consume a subject marker. Taking a function
        // address, a macro body or another language cannot share that exception.
        let macro_body = source
            .lines()
            .nth(line - 1)
            .is_some_and(|line| line.trim_start().starts_with("#define"));
        let subject_marker = marker.take();
        let subject = c
            && call
            && !reference
            && !macro_body
            && matches!(name, "nanosleep" | "clock_nanosleep")
            && subject_marker.is_some_and(|marker| dr_c_subject(marker, name));
        if !subject {
            findings.push(Finding {
                path: path.into(), line: *line,
                detail: format!("host sleep {name} in DR fixture; use event readiness/pacing; only a documented bounded syscall subject may use nanosleep/clock_nanosleep"),
            });
        }
    }
    findings
}

// Split call arguments without losing casts or nested expressions.
fn dr_call_args(tokens: &[(String, usize)], open: usize) -> Option<Vec<Vec<String>>> {
    let mut args = vec![Vec::new()];
    let mut depth = 0;
    for (token, _) in &tokens[open + 1..] {
        match token.as_str() {
            ")" if depth == 0 => return Some(args),
            "," if depth == 0 => {
                args.push(Vec::new());
                continue;
            }
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth -= 1,
            _ => {}
        }
        args.last_mut()?.push(token.clone());
    }
    None
}

fn dr_fixture_sources(root: &Path, directory: &Path, report: &mut Report) -> Result<(), String> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            dr_fixture_sources(root, &path, report)?;
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            matches!(
                e,
                "c" | "h" | "S" | "s" | "py" | "go" | "js" | "java" | "cs"
            )
        }) {
            let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            report.findings.extend(dr_fixture_findings(
                path.strip_prefix(root).unwrap_or(&path),
                &text,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
