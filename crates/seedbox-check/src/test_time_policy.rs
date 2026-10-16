//! Syntax-aware test-time policy. Kept in seedbox-check, not the leaf toolkit.
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
    site: Option<SiteKey>,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SiteKey {
    owner: String,
    operation: String,
    context: String,
    occurrence: usize,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSite {
    pub bead: &'static str,
    pub id: &'static str,
    pub path: &'static str,
    pub owner: &'static str,
    pub operations: &'static [&'static str],
}
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub pending_sites: Vec<String>,
    pub baseline_registered: usize,
    pub baseline_matched: usize,
}
mod baseline;
mod pending;

#[derive(Clone)]
struct Exemption {
    kind: String,
    reason: String,
    used: bool,
    pending: Option<(String, String)>,
    owner: String,
    operations: Vec<String>,
}
const KINDS: [&str; 5] = [
    "hang-guard",
    "controlled-time",
    "measurement",
    "host-time-subject",
    "host-timing-perturbation",
];
const MARKER: &str = "// test-time: allow(";

fn exemptions(source: &str, errors: &mut Vec<(usize, String)>) -> BTreeMap<usize, Exemption> {
    // Raw strings in policy fixtures may contain exemption examples. They are
    // not comments and cannot grant an exemption or create an unused one.
    fn literal_lines(stream: TokenStream, lines: &mut BTreeSet<usize>) {
        for token in stream {
            match token {
                TokenTree::Group(group) => literal_lines(group.stream(), lines),
                TokenTree::Literal(value) => {
                    lines.extend(value.span().start().line..=value.span().end().line)
                }
                _ => {}
            }
        }
    }
    let mut literals = BTreeSet::new();
    if let Ok(tokens) = source.parse::<TokenStream>() {
        literal_lines(tokens, &mut literals);
    }
    let mut allows = BTreeMap::new();
    for (index, line) in source.lines().enumerate() {
        if literals.contains(&(index + 1)) {
            continue;
        }
        let line = line.trim();
        if !line.starts_with("// test-time:") {
            continue;
        }
        let pending = line
            .strip_prefix("// test-time: pending(")
            .and_then(|tail| tail.split_once(") -- "));
        let parsed = line
            .strip_prefix(MARKER)
            .and_then(|tail| tail.split_once(") -- "));
        if let Some((identity, reason)) = pending {
            if let Some((bead, id)) = identity.split_once(", ") {
                if bead == "bd-dd9" && !id.is_empty() && !reason.trim().is_empty() {
                    allows.insert(
                        index + 2,
                        Exemption {
                            kind: "pending".into(),
                            reason: reason.trim().into(),
                            used: false,
                            pending: Some((bead.into(), id.into())),
                            owner: String::new(),
                            operations: Vec::new(),
                        },
                    );
                    continue;
                }
            }
            errors.push((
                index + 1,
                "pending exemption requires pending(bd-dd9, <registered-site-id>) -- <reason>"
                    .into(),
            ));
            continue;
        }
        let Some((kind, reason)) = parsed else {
            errors.push((
                index + 1,
                "expected // test-time: allow(<reason-kind>) -- <site-specific explanation>".into(),
            ));
            continue;
        };
        if !KINDS.contains(&kind) || reason.trim().is_empty() {
            errors.push((
                index + 1,
                "invalid test-time exemption category or empty explanation".into(),
            ));
            continue;
        }
        allows.insert(
            index + 2,
            Exemption {
                kind: kind.into(),
                reason: reason.trim().into(),
                used: false,
                pending: None,
                owner: String::new(),
                operations: Vec::new(),
            },
        );
    }
    allows
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
                "std::thread" => &["sleep", "sleep_until", "park_timeout"],
                "std::time" => &["Instant", "SystemTime"],
                "tokio::time" => &["Instant", "sleep", "sleep_until", "timeout", "timeout_at"],
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
    owners: Vec<String>,
    statements: Vec<usize>,
    aliases: BTreeMap<String, String>,
    allows: BTreeMap<usize, Exemption>,
    active_allow: Vec<usize>,
    claimed_allow: BTreeSet<usize>,
    findings: Vec<(usize, String)>,
    seen: BTreeSet<(usize, usize, String)>,
    source: String,
    contexts: Vec<Span>,
    identities: BTreeMap<usize, SiteKey>,
    occurrences: BTreeMap<(String, String, String), usize>,
}
impl Policy {
    fn reject_item_exemption(&mut self, span: Span) {
        let line = span.start().line;
        if self.allows.remove(&line).is_some() {
            self.findings.push((line - 1, "test-time exemptions cannot cover an item, function, file or module; annotate the smallest statement/expression".into()));
        }
    }
    fn activate(&mut self, line: usize) -> bool {
        if self.test && self.allows.contains_key(&line) && self.claimed_allow.insert(line) {
            self.allows.get_mut(&line).unwrap().owner = self.owners.join("::");
            self.active_allow.push(line);
            true
        } else {
            false
        }
    }
    fn report(&mut self, span: Span, operation: &str, detail: &str) {
        if !self.test {
            return;
        }
        // Token-scanned macro bodies have no syn expression visitor to activate
        // a narrow annotation immediately before the primitive itself.
        let direct = self.activate(span.start().line);
        if let Some(key) = self.active_allow.last() {
            let exemption = self.allows.get_mut(key).unwrap();
            // Keep the category/reason explicit and inspectable; neither is
            // inferred from the identifier spelling or a preceding file comment.
            debug_assert!(
                (exemption.pending.is_some() || KINDS.contains(&exemption.kind.as_str()))
                    && !exemption.reason.is_empty()
            );
            exemption.used = true;
            exemption.operations.push(operation.into());
            if direct {
                self.active_allow.pop();
            }
            return;
        }
        let line = span.start().line;
        if self.seen.insert((line, span.start().column, detail.into())) {
            let owner = self.owners.join("::");
            let context = self.contexts.last().copied().unwrap_or(span);
            let context = baseline::context(&self.source, context);
            let occurrence = self
                .occurrences
                .entry((owner.clone(), operation.into(), context.clone()))
                .or_default();
            *occurrence += 1;
            self.identities.insert(
                self.findings.len(),
                SiteKey {
                    owner,
                    operation: operation.into(),
                    context,
                    occurrence: *occurrence,
                },
            );
            let site = if detail.contains("in test macro") {
                self.statements.last().copied().unwrap_or(line)
            } else {
                line
            };
            self.findings.push((
                line,
                format!(
                    "[owner={};site={site};op={operation}] {detail}",
                    self.owners.join("::")
                ),
            ));
        }
    }
    fn resolve(&self, path: &str) -> String {
        expand_alias(&self.aliases, path)
    }
    fn raw_path(&self, path: &str) -> bool {
        let path = self.resolve(path);
        matches!(
            path.as_str(),
            "std::thread::sleep"
                | "std::thread::sleep_until"
                | "std::thread::park_timeout"
                | "tokio::time::sleep"
                | "tokio::time::sleep_until"
                | "tokio::time::timeout"
                | "tokio::time::timeout_at"
                | "std::time::Instant::now"
                | "tokio::time::Instant::now"
                | "std::time::SystemTime::now"
                | "seedbox_engine_protocol::host_clock::Instant::now"
                | "libc::poll"
                | "libc::ppoll"
                | "libc::select"
                | "libc::pselect"
        ) || matches!(path.as_str(), "Instant::now" | "SystemTime::now")
            || (path.contains("::") && path.rsplit("::").next().is_some_and(Self::method))
    }
    fn method(name: &str) -> bool {
        matches!(
            name,
            "elapsed"
                | "recv_timeout"
                | "wait_timeout"
                | "wait_timeout_while"
                | "set_read_timeout"
                | "set_write_timeout"
                | "connect_timeout"
                | "wait_timeout_ms"
                | "timeout"
                | "timeout_at"
                | "duration_since"
                | "checked_duration_since"
                | "saturating_duration_since"
        )
    }
    // syn cannot parse select! or arbitrary user macro grammars as expressions.
    // Inspect their token structure without treating string literals as code.
    fn macro_tokens(&mut self, stream: TokenStream) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (i, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                let outer = (group.delimiter() == proc_macro2::Delimiter::Brace)
                    .then(|| std::mem::take(&mut self.active_allow));
                self.macro_tokens(group.stream());
                if let Some(outer) = outer {
                    self.active_allow = outer;
                }
            }
            let TokenTree::Ident(ident) = token else {
                continue;
            };
            let name = ident.to_string();
            if i > 0 && matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == '.') {
                if Self::method(&name) && matches!(tokens.get(i + 1), Some(TokenTree::Group(_))) {
                    self.report(ident.span(), &format!("method:{name}"), &format!("raw host-time method {name} in test macro; use test-time helpers or a reasoned per-site exemption"));
                }
                continue;
            }
            if i >= 3
                && matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == ':')
                && matches!(&tokens[i - 2], TokenTree::Punct(p) if p.as_char() == ':')
                && matches!(&tokens[i - 3], TokenTree::Ident(_))
            {
                continue; // A preceding path segment already inspected this path.
            }
            let mut path = name;
            let mut end = i + 1;
            while matches!(tokens.get(end), Some(TokenTree::Punct(p)) if p.as_char()==':')
                && matches!(tokens.get(end + 1), Some(TokenTree::Punct(p)) if p.as_char()==':')
            {
                if let Some(TokenTree::Ident(part)) = tokens.get(end + 2) {
                    path.push_str("::");
                    path.push_str(&part.to_string());
                    end += 3;
                } else {
                    break;
                }
            }
            if self.raw_path(&path) {
                self.report(ident.span(), &format!("function:{}", self.resolve(&path)), &format!("raw host-time primitive {path} in test macro; use test-time helpers or a reasoned per-site exemption"));
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
        self.owners.push(item.ident.to_string());
        visit::visit_item_mod(self, item);
        self.owners.pop();
        self.aliases = before;
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.owners.push(item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.owners.pop();
    }
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let name = match item.self_ty.as_ref() {
            syn::Type::Path(path) => path
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default(),
            _ => "impl".into(),
        };
        self.owners.push(name);
        visit::visit_item_impl(self, item);
        self.owners.pop();
    }
    fn visit_block(&mut self, block: &'ast syn::Block) {
        // A timeout/spawn annotation covers its primitive, never the async,
        // closure or loop body supplied to it.
        let outer = std::mem::take(&mut self.active_allow);
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
        self.active_allow = outer;
    }
    fn visit_item(&mut self, item: &'ast Item) {
        self.reject_item_exemption(item.span());
        let before = self.test;
        self.test |= test_attrs(attrs(item));
        visit::visit_item(self, item);
        self.test = before;
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.reject_item_exemption(item.span());
        let before = self.test;
        self.test |= test_attrs(&item.attrs);
        self.owners.push(item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
        self.owners.pop();
        self.test = before;
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.reject_item_exemption(item.span());
        let before = self.test;
        self.test |= test_attrs(&item.attrs);
        self.owners.push(item.sig.ident.to_string());
        visit::visit_trait_item_fn(self, item);
        self.owners.pop();
        self.test = before;
    }
    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        let outer = std::mem::take(&mut self.active_allow);
        visit::visit_expr_closure(self, closure);
        self.active_allow = outer;
    }
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let line = stmt.span().start().line;
        let allowed = !matches!(stmt, Stmt::Item(_)) && self.activate(line);
        self.statements.push(line);
        self.contexts.push(stmt.span());
        visit::visit_stmt(self, stmt);
        self.contexts.pop();
        self.statements.pop();
        if allowed {
            self.active_allow.pop();
        }
    }
    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        self.contexts.push(field.span());
        visit::visit_field_value(self, field);
        self.contexts.pop();
    }
    fn visit_expr(&mut self, expr: &'ast Expr) {
        let allowed = self.activate(expr.span().start().line);
        visit::visit_expr(self, expr);
        if allowed {
            self.active_allow.pop();
        }
    }
    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        let path = expr
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        if self.raw_path(&path) {
            self.report(expr.span(), &format!("function:{}", self.resolve(&path)), &format!("raw host-time primitive {path}; use test-time helpers or a reasoned per-site exemption"));
        }
        visit::visit_expr_path(self, expr);
    }
    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        let name = expr.method.to_string();
        if Self::method(&name) {
            self.report(expr.method.span(), &format!("method:{name}"), &format!("raw host-time method {name}; use test-time helpers or a reasoned per-site exemption"));
        }
        visit::visit_expr_method_call(self, expr);
    }
    fn visit_macro(&mut self, item: &'ast syn::Macro) {
        if self.test {
            let outer = matches!(item.delimiter, syn::MacroDelimiter::Brace(_))
                .then(|| std::mem::take(&mut self.active_allow));
            self.macro_tokens(item.tokens.clone());
            if let Some(outer) = outer {
                self.active_allow = outer;
            }
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

/// Analyze one Rust source; callers propagate test context for gated modules.
#[cfg(test)]
pub fn source(path: &Path, text: &str, test_context: bool) -> Result<Vec<Finding>, String> {
    source_with_pending(path, text, test_context, &[]).map(|report| report.findings)
}
#[cfg(test)]
fn source_with_pending(
    path: &Path,
    text: &str,
    test_context: bool,
    registry: &[PendingSite],
) -> Result<Report, String> {
    source_in_context(path, text, test_context, registry, &BTreeMap::new())
}
fn source_in_context(
    path: &Path,
    text: &str,
    test_context: bool,
    registry: &[PendingSite],
    inherited: &BTreeMap<String, String>,
) -> Result<Report, String> {
    let ast = syn::parse_file(text).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut aliases = inherited.clone();
    imports(&ast.items, &mut aliases);
    let mut errors = Vec::new();
    let allows = exemptions(text, &mut errors);
    let mut policy = Policy {
        test: test_context || forced(path) || test_attrs(&ast.attrs),
        owners: Vec::new(),
        statements: Vec::new(),
        aliases,
        allows,
        active_allow: Vec::new(),
        claimed_allow: BTreeSet::new(),
        findings: errors,
        seen: BTreeSet::new(),
        source: text.into(),
        contexts: Vec::new(),
        identities: BTreeMap::new(),
        occurrences: BTreeMap::new(),
    };
    policy.visit_file(&ast);
    let mut pending_sites = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for (line, exemption) in policy.allows {
        if !exemption.used {
            policy.findings.push((line - 1, "unused or nonlocal test-time exemption (only the next statement/expression may be exempted)".into()));
            continue;
        }
        if let Some((bead, id)) = exemption.pending {
            let matching = registry.iter().find(|site| site.id == id);
            let error=match matching {
                None=>Some("new pending site is not in the approved current list"),
                Some(site) if site.bead!=bead || Path::new(site.path)!=path || site.owner!=exemption.owner=>Some("pending site bead/path/enclosing-function does not match its approved location"),
                Some(site)=>{
                    let mut actual=exemption.operations;actual.sort();
                    let mut expected:Vec<String>=site.operations.iter().map(|value|(*value).into()).collect();expected.sort();
                    if actual!=expected {Some("pending site's raw operations changed or expanded")}
                    else if !seen_ids.insert(id.clone()) {Some("duplicate pending site")}
                    else {None}
                }
            };
            if let Some(error) = error {
                policy.findings.push((line - 1, format!("{id}: {error}")));
            } else {
                pending_sites.push(id);
            }
        }
    }
    Ok(Report {
        findings: policy
            .findings
            .into_iter()
            .enumerate()
            .map(|(index, (line, detail))| Finding {
                path: path.into(),
                line,
                detail,
                site: policy.identities.remove(&index),
            })
            .collect(),
        pending_sites,
        ..Report::default()
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
                                || value.starts_with("seedbox_engine_protocol::host_clock::")
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

/// Number of explicitly approved pending sites (also printed if parsing fails).
pub fn registered_pending_count() -> usize {
    pending::SITES.len()
}

/// Scan root, VMM and ARM workspaces together, deduplicating linked sources.
pub fn registered_baseline_count(root: &Path) -> Result<usize, String> {
    baseline::registered_count(root)
}
pub fn check(root: &Path) -> Result<Report, String> {
    let report = check_with_pending(root, pending::SITES)?;
    baseline::apply(root, report)
}
fn check_with_pending(root: &Path, registry: &[PendingSite]) -> Result<Report, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let root = root.as_path();
    let mut registry_ids = BTreeSet::new();
    for site in registry {
        if site.bead != "bd-dd9" || !registry_ids.insert(site.id) {
            return Err("invalid or duplicate approved pending registry entry".into());
        }
    }
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
            registry,
            &inherited.get(&path).cloned().unwrap_or_default(),
        )?;
        report.findings.extend(local.findings);
        report.pending_sites.extend(local.pending_sites);
    }
    for site in registry {
        if root.join(site.path).is_file() && !report.pending_sites.iter().any(|id| id == site.id) {
            report.findings.push(Finding {
                path: site.path.into(),
                line: 1,
                detail: format!("{}: stale or unmatched pending registry entry", site.id),
                site: None,
            });
        }
    }
    report
        .findings
        .sort_by(|a, b| (&a.path, a.line, &a.detail).cmp(&(&b.path, b.line, &b.detail)));
    Ok(report)
}

#[cfg(test)]
mod tests;
