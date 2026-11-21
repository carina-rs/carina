use carina_core::paths::CanonicalPath;
use proc_macro2::{TokenStream, TokenTree};
use std::path::{Path, PathBuf};
use syn::parse::Parser;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ImplItem, Item, Meta, Token, TraitItem};

fn unraw_name(name: &syn::Ident) -> String {
    name.to_string().trim_start_matches("r#").to_owned()
}

// Treat non-test feature/platform configurations as potentially production.
fn production_cfg(meta: &Meta) -> Option<bool> {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Some(false),
        Meta::NameValue(value)
            if value.path.is_ident("feature")
                && matches!(&value.value, Expr::Lit(lit) if matches!(&lit.lit, syn::Lit::Str(s) if s.value() == "test-utils")) =>
        {
            Some(false)
        }
        Meta::List(list) => {
            let children = syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .unwrap();
            let values: Vec<_> = children.iter().map(production_cfg).collect();
            if list.path.is_ident("any") {
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.iter().all(|v| *v == Some(false)) {
                    Some(false)
                } else {
                    None
                }
            } else if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|v| *v == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if list.path.is_ident("not") {
                values.first().copied().flatten().map(|v| !v)
            } else {
                None
            }
        }
        _ => None,
    }
}
fn production(attrs: &[Attribute]) -> bool {
    attrs.iter().all(|attr| {
        !attr.path().is_ident("test")
            && (!attr.path().is_ident("cfg")
                || production_cfg(&attr.parse_args::<Meta>().unwrap()) != Some(false))
    })
}
fn attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(x) => &x.attrs,
        Item::Enum(x) => &x.attrs,
        Item::ExternCrate(x) => &x.attrs,
        Item::Fn(x) => &x.attrs,
        Item::ForeignMod(x) => &x.attrs,
        Item::Impl(x) => &x.attrs,
        Item::Macro(x) => &x.attrs,
        Item::Mod(x) => &x.attrs,
        Item::Static(x) => &x.attrs,
        Item::Struct(x) => &x.attrs,
        Item::Trait(x) => &x.attrs,
        Item::TraitAlias(x) => &x.attrs,
        Item::Type(x) => &x.attrs,
        Item::Union(x) => &x.attrs,
        Item::Use(x) => &x.attrs,
        _ => &[],
    }
}
#[derive(Default)]
struct BoundaryCalls {
    file: PathBuf,
    module_dir: PathBuf,
    explicit_dir: PathBuf,
    seen: std::collections::HashSet<CanonicalPath>,
    function: Option<String>,
    core_mints: Vec<PathBuf>,
    cli_mints: Vec<PathBuf>,
    unwraps: Vec<PathBuf>,
    state_entries: Vec<PathBuf>,
    provider_entries: Vec<PathBuf>,
    observation_entries: Vec<PathBuf>,
}
impl BoundaryCalls {
    fn record_state_entry(&mut self, name: &str) {
        let sites = match name {
            "from_write_with_final_desired" => &mut self.state_entries,
            "from_provider_state_with_final_desired" => &mut self.provider_entries,
            "from_observation_with_final_desired" => &mut self.observation_entries,
            _ => return,
        };
        sites.push(self.file.clone());
    }

    // Macro grammars need not be Rust expressions. Count guarded identifiers
    // without parsing that grammar: even an unparseable hit fails closed.
    fn scan_macro_tokens(&mut self, tokens: TokenStream) {
        let mut tokens = tokens.into_iter().peekable();
        while let Some(token) = tokens.next() {
            match token {
                TokenTree::Group(group) => self.scan_macro_tokens(group.stream()),
                TokenTree::Ident(name) => match unraw_name(&name).as_str() {
                    "desired_values" => {
                        assert_eq!(
                            self.function.as_deref(),
                            Some("prepare_post_expansion_inputs")
                        );
                        self.core_mints.push(self.file.clone());
                    }
                    "release_values" => {
                        if matches!(tokens.peek(), Some(TokenTree::Punct(next)) if next.as_char() == ':')
                        {
                            assert_eq!(
                                self.function.as_deref(),
                                Some("prepare_post_expansion_inputs")
                            );
                            self.cli_mints.push(self.file.clone());
                        } else {
                            assert_eq!(self.function.as_deref(), Some("write_resource_state"));
                            self.unwraps.push(self.file.clone());
                        }
                    }
                    name => self.record_state_entry(name),
                },
                _ => {}
            }
        }
    }
    fn scan(&mut self, file: PathBuf, module_dir: PathBuf) {
        if !self.seen.insert(CanonicalPath::new(&file).unwrap()) {
            return;
        }
        let ast = syn::parse_file(&std::fs::read_to_string(&file).unwrap())
            .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", file.display()));
        if !production(&ast.attrs) {
            return;
        }
        let previous_explicit =
            std::mem::replace(&mut self.explicit_dir, file.parent().unwrap().to_path_buf());
        let previous_file = std::mem::replace(&mut self.file, file);
        let previous_dir = std::mem::replace(&mut self.module_dir, module_dir);
        self.visit_file(&ast);
        self.file = previous_file;
        self.module_dir = previous_dir;
        self.explicit_dir = previous_explicit;
    }
}
impl<'ast> Visit<'ast> for BoundaryCalls {
    fn visit_item(&mut self, item: &'ast Item) {
        if production(attributes(item)) {
            visit::visit_item(self, item);
        }
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !production(&item.attrs) {
            return;
        }
        let child_dir = self.module_dir.join(item.ident.to_string());
        if let Some((_, items)) = &item.content {
            let previous_explicit = std::mem::replace(&mut self.explicit_dir, child_dir.clone());
            let previous_dir = std::mem::replace(&mut self.module_dir, child_dir);
            for item in items {
                self.visit_item(item);
            }
            self.module_dir = previous_dir;
            self.explicit_dir = previous_explicit;
        } else {
            let explicit = item.attrs.iter().find_map(|attr| {
                if !attr.path().is_ident("path") {
                    return None;
                }
                let Meta::NameValue(value) = &attr.meta else {
                    return None;
                };
                let Expr::Lit(lit) = &value.value else {
                    return None;
                };
                let syn::Lit::Str(path) = &lit.lit else {
                    return None;
                };
                Some(self.explicit_dir.join(path.value()))
            });
            let file = explicit.unwrap_or_else(|| {
                let flat = self.module_dir.join(format!("{}.rs", item.ident));
                if flat.exists() {
                    flat
                } else {
                    child_dir.join("mod.rs")
                }
            });
            let directory = if file.file_name().unwrap() == "mod.rs" {
                file.parent().unwrap().to_path_buf()
            } else {
                file.parent().unwrap().join(file.file_stem().unwrap())
            };
            self.scan(file, directory);
        }
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let previous = self.function.replace(item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.function = previous;
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let previous = self.function.replace(item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
        self.function = previous;
    }
    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if unraw_name(&item.ident) == "ReleaseWitness" {
            assert!(
                item.fields
                    .iter()
                    .all(|field| matches!(field.vis, syn::Visibility::Inherited)),
                "release witness fields must stay private to their module"
            );
        }
        visit::visit_item_struct(self, item);
    }
    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        let attrs = match item {
            ImplItem::Const(x) => &x.attrs,
            ImplItem::Fn(x) => &x.attrs,
            ImplItem::Type(x) => &x.attrs,
            ImplItem::Macro(x) => &x.attrs,
            _ => return,
        };
        if production(attrs) {
            visit::visit_impl_item(self, item);
        }
    }
    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        let attrs = match item {
            TraitItem::Const(x) => &x.attrs,
            TraitItem::Fn(x) => &x.attrs,
            TraitItem::Type(x) => &x.attrs,
            TraitItem::Macro(x) => &x.attrs,
            _ => return,
        };
        if production(attrs) {
            visit::visit_trait_item(self, item);
        }
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.scan_macro_tokens(mac.tokens.clone());
    }
    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        self.record_state_entry(&unraw_name(&expr.method));
        if unraw_name(&expr.method) == "desired_values" {
            assert_eq!(
                self.function.as_deref(),
                Some("prepare_post_expansion_inputs"),
                "only the final pipeline stage may construct core desired data"
            );
            self.core_mints.push(self.file.clone());
        }
        visit::visit_expr_method_call(self, expr);
    }
    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        let name = unraw_name(&expr.path.segments.last().unwrap().ident);
        if (expr.path.segments.len() > 1 || expr.qself.is_some()) && name == "desired_values" {
            assert_eq!(
                self.function.as_deref(),
                Some("prepare_post_expansion_inputs"),
                "only the final pipeline stage may construct core desired data"
            );
            self.core_mints.push(self.file.clone());
        }
        if expr.path.segments.len() > 1 || expr.qself.is_some() {
            self.record_state_entry(&name);
        }
        visit::visit_expr_path(self, expr);
    }
    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        if expr.fields.iter().any(
            |field| matches!(&field.member, syn::Member::Named(name) if unraw_name(name) == "release_values"),
        ) {
            assert_eq!(
                self.function.as_deref(),
                Some("prepare_post_expansion_inputs"),
                "only the final pipeline stage may mint a release witness"
            );
            self.cli_mints.push(self.file.clone());
        }
        visit::visit_expr_struct(self, expr);
    }
    fn visit_pat_struct(&mut self, pattern: &'ast syn::PatStruct) {
        if pattern.fields.iter().any(
            |field| matches!(&field.member, syn::Member::Named(name) if unraw_name(name) == "release_values"),
        ) {
            assert_eq!(
                self.function.as_deref(),
                Some("write_resource_state"),
                "destructuring is also a witness unwrap"
            );
            self.unwraps.push(self.file.clone());
        }
        visit::visit_pat_struct(self, pattern);
    }
    fn visit_expr_field(&mut self, expr: &'ast syn::ExprField) {
        if matches!(&expr.member, syn::Member::Named(name) if unraw_name(name) == "release_values")
        {
            assert_eq!(
                self.function.as_deref(),
                Some("write_resource_state"),
                "only the CLI-to-state adapter may unwrap the witness"
            );
            self.unwraps.push(self.file.clone());
        }
        visit::visit_expr_field(self, expr);
    }
}

#[test]
fn release_witness_production_boundary_has_single_mint_and_unwrap() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let expected = root.join("carina-cli/src/wiring/release_witness.rs");
    let mut calls = BoundaryCalls::default();
    for crate_name in ["carina-cli", "carina-state", "carina-core"] {
        let src = root.join(crate_name).join("src");
        calls.scan(src.join("lib.rs"), src.clone());
        if src.join("main.rs").exists() {
            calls.scan(src.join("main.rs"), src.clone());
        }
        let bins = src.join("bin");
        if bins.is_dir() {
            for entry in std::fs::read_dir(&bins).unwrap() {
                let path = entry.unwrap().path();
                let binary = if path.is_dir() {
                    path.join("main.rs")
                } else {
                    path
                };
                if binary
                    .extension()
                    .is_some_and(|extension| extension == "rs")
                    && binary.is_file()
                {
                    calls.scan(binary.clone(), binary.parent().unwrap().to_path_buf());
                }
            }
        }
    }
    let state_owner = root.join("carina-state/src/state/mod.rs");
    for (label, sites) in [
        ("provider final-desired entry", &calls.provider_entries),
        (
            "observation final-desired entry",
            &calls.observation_entries,
        ),
    ] {
        // Each has its no-witness convenience wrapper and the common adapter.
        assert_eq!(
            sites.as_slice(),
            &[state_owner.clone(), state_owner.clone()],
            "{label}"
        );
    }
    // Adding a second method/associated call or field unwrap fails these counts.
    for (label, sites) in [
        ("core desired constructor", &calls.core_mints),
        ("CLI witness construction", &calls.cli_mints),
        ("CLI witness unwrap", &calls.unwraps),
        ("CLI-to-state entry", &calls.state_entries),
    ] {
        assert_eq!(
            sites.as_slice(),
            std::slice::from_ref(&expected),
            "{label} must stay inside the sole pipeline owner"
        );
    }
}

#[test]
fn release_witness_boundary_scan_recognizes_duplicate_calls() {
    let source = "fn prepare_post_expansion_inputs() { let desired_values = (); let _ = desired_values; first.r#desired_values(refs); FinalPlanInputs::desired_values(second, refs); <FinalPlanInputs>::desired_values(third, refs); opaque! { invalid => nested!(fourth.r#desired_values(refs)); } } fn write_resource_state() { a.release_values; b.r#release_values; vec![c.release_values]; }";
    let mut calls = BoundaryCalls::default();
    calls.visit_file(&syn::parse_file(source).unwrap());
    assert_eq!(calls.core_mints.len(), 4);
    assert_eq!(calls.unwraps.len(), 3);
    // Macro tokens are scanned even when their grammar is not a Rust expression.
    let source = "fn other() { opaque! { nested! { ResourceState::from_provider_state_with_final_desired(unknown => syntax); ResourceState::from_observation_with_final_desired(other); } } }";
    calls.visit_file(&syn::parse_file(source).unwrap());
    assert_eq!(calls.provider_entries.len(), 1);
    assert_eq!(calls.observation_entries.len(), 1);
    // Privacy is primary; this scan does not resolve aliases or generated code.
}
