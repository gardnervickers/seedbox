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
                "std::thread" | "tokio::time" | "async_std::task" | "smol" => {
                    &["sleep", "sleep_until"]
                }
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
        resolved.rsplit("::").next().is_some_and(Self::sleep_name)
    }
    fn sleep_name(name: &str) -> bool {
        matches!(
            name,
            "sleep" | "sleep_until" | "usleep" | "nanosleep" | "clock_nanosleep"
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
    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        let name = expr.method.to_string();
        if Self::sleep_name(&name) {
            self.report(expr.method.span(), &name);
        }
        visit::visit_expr_method_call(self, expr);
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
                                || value.starts_with("smol::")
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
    let mut force: BTreeSet<_> = files.keys().filter(|path| forced(path)).cloned().collect();
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
    report
        .findings
        .sort_by(|a, b| (&a.path, a.line, &a.detail).cmp(&(&b.path, b.line, &b.detail)));
    Ok(report)
}

#[cfg(test)]
mod tests;
