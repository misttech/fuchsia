//! Extracts functions and their units from C++ source.

use crate::model::{Features, Function, Lang, Ret, Unit, UnitKind};
use crate::normalize;
use crate::ts::{self, FeatureAcc, UnitBuilder};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tree_sitter::Node;

/// Comments attached to a function declaration (typically in a class body
/// in a header), keyed by `(class, base name)`.
pub type DeclComments = HashMap<(String, String), Vec<Unit>>;

/// Base classes of each class defined in a file, keyed by the class's
/// unqualified name, also unqualified and without template arguments.
pub type ClassBases = HashMap<String, Vec<String>>;

/// Everything extracted from one C++ file.
pub struct CppFile {
    pub functions: Vec<Function>,
    pub decl_comments: DeclComments,
    pub bases: ClassBases,
}

pub fn extract(path: &str, src: &str) -> CppFile {
    let lines = crate::extract::split_lines(src);
    let src = &mask_annotations(src);
    let tree = ts::parse(Lang::Cpp, src);
    let mut out =
        CppFile { functions: Vec::new(), decl_comments: HashMap::new(), bases: HashMap::new() };
    let ctx = Ctx { path, src: src.as_bytes(), lines: &lines };
    ctx.walk_scope(tree.root_node(), &[], &mut out);
    test_names(&mut out.functions, &lines);
    out
}

/// Records the test name of each test function: the name its
/// `UNITTEST("name", Fn)` registration gives it, or `Name` in a
/// `TEST(Suite, Name)` definition.
fn test_names(functions: &mut [Function], lines: &[String]) {
    static UNITTEST: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\bUNITTEST\s*\(\s*"([^"]*)"\s*,\s*(\w+)\s*\)"#).unwrap());
    static TEST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*(?:TEST|TEST_F|TEST_P)\s*\(\s*\w+\s*,\s*(\w+)\s*\)").unwrap()
    });
    let registered: HashMap<&str, &str> = lines
        .iter()
        .flat_map(|l| UNITTEST.captures_iter(l))
        .map(|c| (c.get(2).unwrap().as_str(), c.get(1).unwrap().as_str()))
        .collect();
    for f in functions.iter_mut().filter(|f| f.class.is_none()) {
        let sig = f.units.iter().find(|u| u.kind == UnitKind::Signature);
        let defined = sig.and_then(|u| TEST.captures(f.line(u.start_line)));
        f.test_name = match defined {
            Some(c) => Some(c[1].to_string()),
            None => registered.get(f.base.as_str()).map(|s| s.to_string()),
        };
    }
}

struct Ctx<'a> {
    path: &'a str,
    src: &'a [u8],
    lines: &'a [String],
}

impl<'a> Ctx<'a> {
    fn text(&self, n: Node) -> &'a str {
        ts::text(n, self.src)
    }

    fn walk_scope(&self, scope: Node, class: &[String], out: &mut CppFile) {
        for child in ts::named_children(scope) {
            self.walk_item(child, child, class, out);
        }
    }

    /// `outer` is the node whose preceding siblings hold leading comments
    /// (a template declaration wraps the function definition).
    fn walk_item(&self, n: Node, outer: Node, class: &[String], out: &mut CppFile) {
        match n.kind() {
            "function_definition" => {
                if let Some(f) = self.function(n, outer, class) {
                    out.functions.push(f);
                }
            }
            "template_declaration" => {
                for c in ts::named_children(n) {
                    if matches!(
                        c.kind(),
                        "function_definition"
                            | "declaration"
                            | "class_specifier"
                            | "struct_specifier"
                            | "template_declaration"
                            | "field_declaration"
                    ) {
                        self.walk_item(c, outer, class, out);
                    }
                }
            }
            "namespace_definition" | "linkage_specification" => {
                if let Some(body) = n.child_by_field_name("body") {
                    self.walk_scope(body, class, out);
                }
            }
            "declaration_list" | "preproc_if" | "preproc_ifdef" | "preproc_else"
            | "preproc_elif" => self.walk_scope(n, class, out),
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                self.class(n, class, out);
            }
            "declaration" | "field_declaration" => {
                // Class definitions inside declarations: `class Foo { ... } foo;`
                if let Some(ty) = n.child_by_field_name("type") {
                    if matches!(ty.kind(), "class_specifier" | "struct_specifier") {
                        self.class(ty, class, out);
                    }
                }
                self.declaration(n, outer, class, out);
            }
            "expression_statement" => {
                self.expression_statement(n, class, out);
            }
            _ => {}
        }
    }

    fn class(&self, n: Node, class: &[String], out: &mut CppFile) {
        let (Some(name), Some(body)) =
            (n.child_by_field_name("name"), n.child_by_field_name("body"))
        else {
            return;
        };
        let mut path = class.to_vec();
        path.push(self.text(name).to_string());
        for c in ts::named_children(n) {
            if c.kind() != "base_class_clause" {
                continue;
            }
            let bases: Vec<String> = ts::named_children(c)
                .into_iter()
                .filter(|b| {
                    matches!(b.kind(), "type_identifier" | "qualified_identifier" | "template_type")
                })
                .map(|b| unqualified_type(self.text(b)))
                .collect();
            out.bases.entry(unqualified_type(self.text(name))).or_default().extend(bases);
        }
        self.walk_scope(body, &path, out);
        self.synthesize_member_init_ctor(name, body, &path, out);
    }

    /// When a C++ class has no explicit constructor declaration or definition
    /// and initializes fields via in-class default member initializers,
    /// synthesizes a `ClassName::ClassName` constructor from its data member
    /// declarations so it can pair with Rust `Type::new` / `Type::init`.
    fn synthesize_member_init_ctor(
        &self,
        name: Node,
        body: Node,
        path: &[String],
        out: &mut CppFile,
    ) {
        let base = unqualified_type(self.text(name));
        if base.is_empty() || self.has_ctor_decl_or_def(body, &base) {
            return;
        }
        let mut fields = Vec::new();
        self.collect_data_fields(body, &mut fields);
        if !fields.iter().any(|f| f.child_by_field_name("default_value").is_some()) {
            return;
        }
        let start = ts::line(*fields.first().unwrap());
        let end = ts::end_line(*fields.last().unwrap());
        let mut b = UnitBuilder::new();
        let mut sig_acc = FeatureAcc::default();
        sig_acc.ident(&base);
        let sig = sig_acc.finish(Lang::Cpp);
        b.push(UnitKind::Signature, start, start, 0, sig);
        b.units[0].end_line = start.saturating_sub(1);

        for fd in fields {
            let mut acc = FeatureAcc::default();
            let ty = fd.child_by_field_name("type");
            let decl = fd.child_by_field_name("declarator");
            let dv = fd.child_by_field_name("default_value");
            if let Some(d) = decl {
                self.collect(d, &mut acc, &[]);
            }
            if let Some(v) = dv {
                self.collect(v, &mut acc, &[]);
            } else if decl.is_some_and(|d| matches!(d.kind(), "identifier" | "field_identifier")) {
                if let Some(t) = ty.filter(|t| {
                    matches!(t.kind(), "type_identifier" | "qualified_identifier")
                        && is_class_type(self.text(*t))
                }) {
                    acc.call(self.text(t));
                }
            }
            if let Some(t) = ty.filter(|t| t.kind() == "template_type") {
                let outer = t
                    .child_by_field_name("name")
                    .map(|x| unqualified_type(self.text(x)))
                    .unwrap_or_default();
                if !matches!(
                    outer.as_str(),
                    "array"
                        | "atomic"
                        | "RefPtr"
                        | "unique_ptr"
                        | "optional"
                        | "KernelHandle"
                        | "user_in_ptr"
                        | "user_out_ptr"
                        | "user_inout_ptr"
                ) {
                    acc.call(&normalize::ident(self.text(t)));
                }
            }
            acc.text = self.text(fd).to_string();
            let mut f = acc.finish(Lang::Cpp);
            f.plumbing = f.calls.is_empty()
                && f.errors.is_empty()
                && f.locks.is_empty()
                && !f.propagates
                && dv.is_none_or(|v| is_pure_or_accessor(v, self.src));
            b.push(UnitKind::Stmt, ts::line(fd), ts::end_line(fd), 1, f);
        }

        let mut calls: Vec<String> =
            b.units.iter().flat_map(|u| u.features.calls.clone()).collect();
        calls.sort();
        calls.dedup();
        let mut qcalls: Vec<String> =
            b.units.iter().flat_map(|u| u.features.qcalls.clone()).collect();
        qcalls.sort();
        qcalls.dedup();
        let cls = path.join("::");
        let full_name = format!("{cls}::{base}");
        out.functions.push(Function {
            lang: Lang::Cpp,
            path: self.path.to_string(),
            name: full_name,
            base,
            class: Some(cls),
            start_line: start,
            end_line: end,
            lines: self.lines[start - 1..end.min(self.lines.len())].to_vec(),
            units: b.units,
            calls,
            qcalls,
            is_ffi: false,
            is_pub: false,
            test_only: false,
            test_name: None,
        });
    }

    fn expression_statement(&self, n: Node, class: &[String], out: &mut CppFile) {
        let child = match ts::named_children(n).into_iter().next() {
            Some(c) => c,
            None => return,
        };
        if child.kind() == "call_expression" {
            if let Some(func) = child.child_by_field_name("function") {
                if self.text(func).trim() == "IMPLEMENT_DISPATCHER_RUST_STATE" {
                    if let Some(args) = child.child_by_field_name("arguments") {
                        self.implement_dispatcher_rust_state(n, args, class, out);
                    }
                }
            }
        }
    }

    fn implement_dispatcher_rust_state(
        &self,
        stmt: Node,
        args: Node,
        class: &[String],
        out: &mut CppFile,
    ) {
        let children: Vec<Node> = ts::named_children(args);
        if children.len() < 3 {
            return;
        }
        let raw_class = self.text(children[0]).trim();
        let class_name = unqualified_type(raw_class);
        let get_lock_fn = self.text(children[1]).trim();
        let destroy_fn = self.text(children[2]).trim();

        let start = ts::line(stmt);
        let end = ts::end_line(stmt);
        let lines = self.lines[start - 1..end.min(self.lines.len())].to_vec();

        let cls = if class.is_empty() {
            class_name.clone()
        } else {
            format!("{}::{class_name}", class.join("::"))
        };

        // 1. Synthesize Class::~Class() { destroy_fn(&opaque_storage_); }
        let dtor_base = format!("~{class_name}");
        let dtor_full = format!("{cls}::{dtor_base}");
        let mut dtor_b = UnitBuilder::new();
        let mut dtor_sig_acc = FeatureAcc::default();
        dtor_sig_acc.ident(&dtor_base);
        let dtor_sig = dtor_sig_acc.finish(Lang::Cpp);
        dtor_b.push(UnitKind::Signature, start, start, 0, dtor_sig);

        let mut dtor_stmt_acc = FeatureAcc::default();
        dtor_stmt_acc.call(destroy_fn);
        dtor_stmt_acc.ident(destroy_fn);
        dtor_stmt_acc.text = format!("{destroy_fn}(&opaque_storage_);");
        let mut dtor_f = dtor_stmt_acc.finish(Lang::Cpp);
        dtor_f.plumbing = true;
        dtor_b.push(UnitKind::Stmt, start, end, 1, dtor_f);

        out.functions.push(Function {
            lang: Lang::Cpp,
            path: self.path.to_string(),
            name: dtor_full,
            base: dtor_base,
            class: Some(cls.clone()),
            start_line: start,
            end_line: end,
            lines: lines.clone(),
            units: dtor_b.units,
            calls: vec![destroy_fn.to_string()],
            qcalls: Vec::new(),
            is_ffi: false,
            is_pub: false,
            test_only: false,
            test_name: None,
        });

        // 2. Synthesize Lock<CriticalMutex>* Class::get_lock() const { return get_lock_fn(&opaque_storage_); }
        let get_lock_base = "get_lock".to_string();
        let get_lock_full = format!("{cls}::{get_lock_base}");
        let mut lock_b = UnitBuilder::new();
        let mut lock_sig_acc = FeatureAcc::default();
        lock_sig_acc.ident(&get_lock_base);
        let lock_sig = lock_sig_acc.finish(Lang::Cpp);
        lock_b.push(UnitKind::Signature, start, start, 0, lock_sig);

        let mut lock_stmt_acc = FeatureAcc::default();
        lock_stmt_acc.call(get_lock_fn);
        lock_stmt_acc.ident(get_lock_fn);
        lock_stmt_acc.text = format!("return {get_lock_fn}(&opaque_storage_);");
        let mut lock_f = lock_stmt_acc.finish(Lang::Cpp);
        lock_f.plumbing = true;
        lock_b.push(UnitKind::Stmt, start, end, 1, lock_f);

        out.functions.push(Function {
            lang: Lang::Cpp,
            path: self.path.to_string(),
            name: get_lock_full,
            base: get_lock_base,
            class: Some(cls),
            start_line: start,
            end_line: end,
            lines,
            units: lock_b.units,
            calls: vec![get_lock_fn.to_string()],
            qcalls: Vec::new(),
            is_ffi: false,
            is_pub: false,
            test_only: false,
            test_name: None,
        });
    }

    fn has_ctor_decl_or_def(&self, scope: Node, base_cls: &str) -> bool {
        for c in ts::named_children(scope) {
            match c.kind() {
                "function_definition" | "declaration" | "field_declaration" => {
                    if let Some(fdecl) =
                        c.child_by_field_name("declarator").and_then(find_function_declarator)
                    {
                        if let Some(n) = fdecl.child_by_field_name("declarator") {
                            if unqualified_type(self.text(n)) == base_cls {
                                return true;
                            }
                        }
                    }
                }
                "declaration_list"
                | "field_declaration_list"
                | "preproc_if"
                | "preproc_ifdef"
                | "preproc_else"
                | "preproc_elif"
                | "template_declaration" => {
                    if self.has_ctor_decl_or_def(c, base_cls) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn collect_data_fields<'t>(&self, scope: Node<'t>, out: &mut Vec<Node<'t>>) {
        for c in ts::named_children(scope) {
            match c.kind() {
                "field_declaration" => {
                    let Some(decl) = c.child_by_field_name("declarator") else {
                        continue;
                    };
                    if find_function_declarator(decl).is_some() {
                        continue;
                    }
                    let is_static_or_constexpr = ts::named_children(c).iter().any(|ch| {
                        ch.kind() == "storage_class_specifier"
                            || (ch.kind() == "type_qualifier" && self.text(*ch) == "constexpr")
                    });
                    if !is_static_or_constexpr {
                        out.push(c);
                    }
                }
                "declaration_list"
                | "field_declaration_list"
                | "preproc_if"
                | "preproc_ifdef"
                | "preproc_else"
                | "preproc_elif" => {
                    self.collect_data_fields(c, out);
                }
                _ => {}
            }
        }
    }

    /// Records leading comments of a function declaration so they can be
    /// compared with the Rust doc comments of the corresponding function.
    fn declaration(&self, n: Node, outer: Node, class: &[String], out: &mut CppFile) {
        let Some(decl) = n.child_by_field_name("declarator") else {
            return;
        };
        let Some(fdecl) = find_function_declarator(decl) else {
            return;
        };
        let Some(name) = fdecl.child_by_field_name("declarator") else {
            return;
        };
        let (cls, base) = split_name(self.text(name), class);
        let mut b = UnitBuilder::new();
        let _ = self.leading_comments(outer, &mut b);
        if b.units.is_empty() {
            return;
        }
        for u in &mut b.units {
            u.file = Some(self.path.to_string());
            u.ext_lines = (u.start_line..=u.end_line)
                .map(|l| self.lines.get(l - 1).cloned().unwrap_or_default())
                .collect();
        }
        out.decl_comments.entry((cls.unwrap_or_default(), base)).or_insert(b.units);
    }

    /// Adds comment units for the comments directly above `outer` and returns
    /// the first line they occupy.
    fn leading_comments(&self, outer: Node, b: &mut UnitBuilder) -> usize {
        let mut comments = Vec::new();
        let mut first = ts::line(outer);
        let mut cur = outer.prev_sibling();
        while let Some(p) = cur {
            if !ts::is_comment(p) || ts::end_line(p) + 1 < first {
                break;
            }
            // A trailing comment on the previous item's line belongs to it.
            if let Some(pp) = p.prev_sibling() {
                if ts::end_line(pp) == ts::line(p) && !ts::is_comment(pp) {
                    break;
                }
            }
            first = ts::line(p);
            comments.push(p);
            cur = p.prev_sibling();
        }
        for c in comments.iter().rev() {
            b.comment(self.text(*c), ts::line(*c), ts::end_line(*c), 0);
        }
        first
    }

    fn function(&self, n: Node, outer: Node, class: &[String]) -> Option<Function> {
        let decl = n.child_by_field_name("declarator")?;
        let fdecl = find_function_declarator(decl)?;
        let name_node = fdecl.child_by_field_name("declarator")?;
        let (cls, base) = split_name(self.text(name_node), class);
        let body = n.child_by_field_name("body")?;

        let mut b = UnitBuilder::new();
        let start = self.leading_comments(outer, &mut b);

        // Signature: from the start of the definition to the opening brace.
        let mut acc = FeatureAcc::default();
        acc.ident(&base);
        if let Some(params) = fdecl.child_by_field_name("parameters") {
            self.collect(params, &mut acc, &[]);
        }
        acc.text = String::new();
        let sig = acc.finish(Lang::Cpp);
        b.push(UnitKind::Signature, ts::line(outer), ts::line(body), 0, sig);

        if body.kind() == "compound_statement" {
            self.block(body, 1, &mut b);
        }
        // `*out = value;` hands a result back through a pointer parameter,
        // which Rust returns instead.
        let outs = fdecl
            .child_by_field_name("parameters")
            .map(|p| self.pointer_params(p))
            .unwrap_or_default();
        if !outs.is_empty() {
            static OUT: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"^\*\s*(\w+)\s*=[^=]").unwrap());
            for u in &mut b.units {
                if u.kind != UnitKind::Stmt || !u.features.calls.is_empty() {
                    continue;
                }
                let text = self.lines.get(u.start_line - 1).map_or("", |l| l.trim());
                if OUT.captures(text).is_some_and(|c| outs.contains(&c[1].to_string())) {
                    u.features.plumbing = true;
                }
            }
        }
        // `return Foo();` in a function returning a status passes Foo's status
        // on, which Rust writes `foo()?; Ok(())`.
        let returns_status =
            n.child_by_field_name("type").is_some_and(|t| self.text(t) == "zx_status_t");
        if returns_status {
            for u in &mut b.units {
                if u.kind == UnitKind::Return
                    && u.features.ret == Some(Ret::Value)
                    && !u.features.calls.is_empty()
                {
                    u.features.ret = Some(Ret::Status);
                }
            }
        }

        if returns_status {
            chain_status(&mut b.units, self.lines);
        }
        branch_status(&mut b.units, self.lines);

        let end = ts::end_line(n);
        let mut calls: Vec<String> =
            b.units.iter().flat_map(|u| u.features.calls.clone()).collect();
        calls.sort();
        calls.dedup();
        let mut qcalls: Vec<String> =
            b.units.iter().flat_map(|u| u.features.qcalls.clone()).collect();
        qcalls.sort();
        qcalls.dedup();
        let name = match &cls {
            Some(c) => format!("{c}::{base}"),
            None => base.clone(),
        };
        Some(Function {
            lang: Lang::Cpp,
            path: self.path.to_string(),
            name,
            base,
            class: cls,
            start_line: start,
            end_line: end,
            lines: self.lines[start - 1..end.min(self.lines.len())].to_vec(),
            units: b.units,
            calls,
            qcalls,
            is_ffi: false,
            is_pub: false,
            test_only: false,
            test_name: None,
        })
    }

    /// Collects call and identifier features from a subtree, skipping the
    /// subtrees in `skip`.
    fn collect(&self, n: Node, acc: &mut FeatureAcc, skip: &[Node]) {
        if skip.iter().any(|s| s.id() == n.id()) {
            return;
        }
        match n.kind() {
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") {
                    let name = match f.kind() {
                        "field_expression" => f.child_by_field_name("field").map(|x| self.text(x)),
                        "template_function" => f.child_by_field_name("name").map(|x| self.text(x)),
                        _ => Some(self.text(f)),
                    };
                    if let Some(name) = name {
                        acc.call(name);
                    }
                }
            }
            "identifier" | "field_identifier" | "namespace_identifier" => acc.ident(self.text(n)),
            // `static_assert(cond);` asserts like Rust `const { assert!(cond) }`.
            "static_assert_declaration" => acc.call("static_assert"),
            "declaration" => {
                // `Foo foo{args};` constructs a Foo, like Rust `Foo::new(args)`.
                let ty = n.child_by_field_name("type");
                let ctor = n.child_by_field_name("declarator").is_some_and(|d| {
                    d.kind() == "init_declarator"
                        && d.child_by_field_name("value").is_some_and(|v| {
                            matches!(v.kind(), "initializer_list" | "argument_list")
                                && ts::named_children(v).iter().any(|a| !ts::is_comment(*a))
                        })
                });
                // `AutoLock guard;` and `AutoBlocked by(REASON);` (which parses
                // as a function declaration) construct RAII objects too.
                let raii = n.child_by_field_name("declarator").is_some_and(|d| {
                    d.kind() == "function_declarator"
                        || (d.kind() == "identifier"
                            && ty.is_some_and(|t| is_class_type(self.text(t))))
                });
                if let Some(ty) = ty.filter(|t| {
                    (ctor || raii) && matches!(t.kind(), "type_identifier" | "qualified_identifier")
                }) {
                    acc.call(self.text(ty));
                }
            }
            _ => {}
        }
        for c in ts::named_children(n) {
            self.collect(c, acc, skip);
        }
    }

    /// Features of `n`, excluding nested bodies in `skip`.
    fn features(&self, n: Node, skip: &[Node]) -> Features {
        let mut acc = FeatureAcc::default();
        self.collect(n, &mut acc, skip);
        acc.text = ts::text_without(n, self.src, skip);
        acc.finish(Lang::Cpp)
    }

    fn block(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        for c in ts::named_children(n) {
            self.statement(c, depth, b);
        }
    }

    /// `#if`, `#ifdef`, `#elif` or `#else` inside a body: a
    /// conditional-compilation unit for the directive, the code it guards
    /// one level deeper, then the `#elif`/`#else` that follows.
    fn preproc(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let line = ts::line(n);
        let cond = n.child_by_field_name("condition").or_else(|| n.child_by_field_name("name"));
        let alt = n.child_by_field_name("alternative");
        let directive = self.text(n).lines().next().unwrap_or("").to_string();
        let f = Features { idents: normalize::cfg_words(&directive), ..Features::default() };
        b.push(UnitKind::Cfg, line, line, depth, f);
        for c in ts::named_children(n) {
            if Some(c) == cond || Some(c) == alt {
                continue;
            }
            self.statement(c, depth + 1, b);
        }
        if let Some(a) = alt {
            self.preproc(a, depth, b);
        }
    }

    fn statement(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let line = ts::line(n);
        match n.kind() {
            "comment" => b.comment(self.text(n), line, ts::end_line(n), depth),
            "compound_statement" => self.block(n, depth + 1, b),
            "attributed_statement" => {
                // `[[likely]] { ... }` and friends: the attribute doesn't matter.
                for c in ts::named_children(n) {
                    if c.kind() != "attribute_declaration" {
                        self.statement(c, depth, b);
                    }
                }
            }
            "preproc_if" | "preproc_ifdef" | "preproc_else" | "preproc_elif"
            | "preproc_elifdef" => self.preproc(n, depth, b),
            "if_statement" => self.if_statement(n, depth, false, b),
            "for_statement" | "for_range_loop" | "while_statement" => {
                let body = n.child_by_field_name("body");
                let header_end = body.map_or(ts::end_line(n), |bd| ts::line(bd));
                let skip: Vec<Node> = body.into_iter().collect();
                let f = self.features(n, &skip);
                b.push(UnitKind::Loop, line, header_end, depth, f);
                if let Some(bd) = body {
                    self.body(bd, depth + 1, b);
                }
            }
            "do_statement" => {
                let body = n.child_by_field_name("body");
                let cond = n.child_by_field_name("condition");
                let header_end = body.map_or(line, |bd| ts::line(bd));
                b.push(UnitKind::Loop, line, header_end, depth, Features::default());
                if let Some(bd) = body {
                    self.body(bd, depth + 1, b);
                }
                if let Some(c) = cond {
                    let cond_start = body
                        .filter(|bd| bd.kind() == "compound_statement")
                        .map_or(ts::line(c), |bd| ts::end_line(bd))
                        .min(ts::line(c));
                    let cond_end = ts::end_line(n);
                    let mut f = self.features(c, &[]);
                    self.conjuncts(c, &mut f.conjuncts);
                    b.push(UnitKind::If, cond_start, cond_end, depth + 1, f);
                    b.push(UnitKind::Break, cond_end, cond_end, depth + 2, Features::default());
                }
            }
            "switch_statement" => {
                let body = n.child_by_field_name("body");
                let header_end = body.map_or(ts::end_line(n), |bd| ts::line(bd));
                let skip: Vec<Node> = body.into_iter().collect();
                let f = self.features(n, &skip);
                b.push(UnitKind::Switch, line, header_end, depth, f);
                if let Some(bd) = body {
                    for c in ts::named_children(bd) {
                        if c.kind() == "case_statement" {
                            self.case(c, depth + 1, b);
                        } else {
                            self.statement(c, depth + 1, b);
                        }
                    }
                }
            }
            "case_statement" => self.case(n, depth, b),
            "return_statement" => self.return_statement(n, depth, b),
            "break_statement" => {
                b.push(UnitKind::Break, line, ts::end_line(n), depth, Features::default())
            }
            "continue_statement" => {
                b.push(UnitKind::Continue, line, ts::end_line(n), depth, Features::default())
            }
            "goto_statement" => {
                let f = self.features(n, &[]);
                b.push(UnitKind::Goto, line, ts::end_line(n), depth, f)
            }
            "labeled_statement" => {
                let label = n.child_by_field_name("label");
                let mut f = Features::default();
                if let Some(l) = label {
                    f.idents.push(crate::normalize::ident(self.text(l)));
                }
                b.push(UnitKind::Label, line, line, depth, f);
                for c in ts::named_children(n) {
                    if c.kind() != "statement_identifier" {
                        self.statement(c, depth, b);
                    }
                }
            }
            _ => self.plain(n, depth, b),
        }
    }

    /// A statement or declaration. Lambda bodies spanning several lines are
    /// split out so their statements align individually.
    fn plain(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let mut lambdas = Vec::new();
        find_bodies(n, &mut lambdas);
        let lambdas: Vec<Node> =
            lambdas.into_iter().filter(|l| ts::end_line(*l) > ts::line(*l)).collect();
        let mut f = self.features(n, &lambdas);
        f.plumbing = self.is_plumbing(n, &f);
        let end = lambdas.first().map_or(ts::end_line(n), |l| ts::line(*l));
        b.push(UnitKind::Stmt, ts::line(n), end, depth, f);
        for l in lambdas {
            self.block(l, depth + 1, b);
        }
    }

    /// Names of a function's pointer parameters.
    fn pointer_params(&self, params: Node) -> Vec<String> {
        let mut out = Vec::new();
        for p in ts::named_children(params) {
            let mut d = p.child_by_field_name("declarator");
            let mut is_ptr = false;
            while let Some(x) = d {
                if x.kind() == "pointer_declarator" {
                    is_ptr = true;
                }
                if x.kind() == "identifier" {
                    if is_ptr {
                        out.push(self.text(x).to_string());
                    }
                    break;
                }
                d = x.child_by_field_name("declarator");
            }
        }
        out
    }

    /// Declarations that only name a value for later: `zx_status_t status;`,
    /// `auto count = count_;`, or binding the result of a zero-argument
    /// accessor (`auto* up = ProcessDispatcher::GetCurrent();`). Rust
    /// spells these differently or not at all; what matters is where the
    /// value is used.
    fn is_plumbing(&self, n: Node, f: &Features) -> bool {
        // Clang thread-safety assertions (`AssertHeld(lock)`,
        // `op->AssertParentLockHeld()`) have no runtime effect.
        static TA: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^[\w:.>*()-]*Assert\w*Held\s*\(").unwrap());
        if n.kind() == "expression_statement" && TA.is_match(self.text(n).trim()) {
            return true;
        }
        if n.kind() != "declaration" || !f.errors.is_empty() || !f.locks.is_empty() || f.propagates
        {
            return false;
        }
        let decls: Vec<Node> = ts::named_children(n)
            .into_iter()
            .filter(|c| {
                !matches!(
                    c.kind(),
                    "primitive_type"
                        | "type_identifier"
                        | "qualified_identifier"
                        | "template_type"
                        | "sized_type_specifier"
                        | "type_qualifier"
                        | "storage_class_specifier"
                        | "placeholder_type_specifier"
                        | "enum_specifier"
                        | "struct_specifier"
                        | "comment"
                )
            })
            .collect();
        if decls.is_empty() {
            return false;
        }
        decls.iter().all(|d| match d.kind() {
            "identifier" | "pointer_declarator" | "reference_declarator" | "array_declarator" => {
                // Constructing a class type is a call (see `collect`).
                f.calls.is_empty()
            }
            "init_declarator" => {
                d.child_by_field_name("value").is_some_and(|v| is_pure_or_accessor(v, self.src))
            }
            _ => false,
        })
    }

    fn body(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        if n.kind() == "attributed_statement" {
            for c in ts::named_children(n) {
                if c.kind() != "attribute_declaration" {
                    self.body(c, depth, b);
                }
            }
        } else if n.kind() == "compound_statement" {
            self.block(n, depth, b);
        } else {
            self.statement(n, depth, b);
        }
    }

    fn case(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let value = n.child_by_field_name("value");
        let mut f = match value {
            Some(v) => self.features(v, &[]),
            None => Features::default(),
        };
        if value.is_none() {
            f.idents.push("default".to_string());
        }
        let mut stmts: Vec<Node> = ts::named_children(n);
        if let Some(v) = value {
            stmts.retain(|s| s.id() != v.id());
        }
        drop_trailing_break(&mut stmts);
        // `case A: case B:` falls through, which Rust writes as `A | B =>`.
        if let Some(last) = b.units.last_mut() {
            if last.kind == UnitKind::Case
                && last.depth == depth
                && last.end_line + 1 == ts::line(n)
            {
                last.end_line = ts::line(n);
                last.features.idents.extend(f.idents);
                last.features.errors.extend(f.errors);
                for s in stmts {
                    self.case_body(s, depth, b);
                }
                return;
            }
        }
        b.push(UnitKind::Case, ts::line(n), ts::line(n), depth, f);
        drop_trailing_break(&mut stmts);
        for s in stmts {
            self.case_body(s, depth, b);
        }
    }

    fn case_body(&self, s: Node, depth: usize, b: &mut UnitBuilder) {
        if s.kind() == "compound_statement" {
            let mut stmts = ts::named_children(s);
            drop_trailing_break(&mut stmts);
            for c in stmts {
                self.statement(c, depth + 1, b);
            }
        } else {
            self.statement(s, depth + 1, b);
        }
    }

    fn return_statement(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let expr = ts::named_children(n).into_iter().find(|c| !ts::is_comment(*c));
        let (line, end) = (ts::line(n), ts::end_line(n));
        // `return c ? a : b;` reads as `if c { return a } else { return b }`,
        // which is how Rust spells it.
        if let Some(cond) = expr.map(strip_parens).filter(|e| e.kind() == "conditional_expression")
        {
            if let (Some(c), Some(t), Some(e)) = (
                cond.child_by_field_name("condition"),
                cond.child_by_field_name("consequence"),
                cond.child_by_field_name("alternative"),
            ) {
                b.push(UnitKind::If, line, end, depth, self.features(c, &[]));
                self.return_unit(Some(t), line, end, depth + 1, b);
                b.push(UnitKind::Else, line, end, depth, Features::default());
                self.return_unit(Some(e), line, end, depth + 1, b);
                return;
            }
        }
        self.return_unit(expr, line, end, depth, b);
    }

    fn return_unit(
        &self,
        expr: Option<Node>,
        line: usize,
        end: usize,
        depth: usize,
        b: &mut UnitBuilder,
    ) {
        let mut f = match expr {
            Some(e) => self.features(e, &[]),
            None => Features::default(),
        };
        f.ret = Some(match expr {
            Some(e) => ts::classify_return(self.text(e)),
            None => Ret::Value,
        });
        // `*actual = n; return ZX_OK;` is how C++ returns values with a
        // status; Rust returns `Ok(n)` or `Ok((a, b))`.
        static OUT_PARAM: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^\*\s*\w+\s*=[^=]").unwrap());
        let mut line = line;
        if f.ret == Some(Ret::Ok) {
            while let Some(prev) = b.units.last() {
                let text = self.lines.get(prev.start_line - 1).map_or("", |l| l.trim());
                if prev.kind == UnitKind::Stmt
                    && prev.depth == depth
                    && prev.file.is_none()
                    && OUT_PARAM.is_match(text)
                {
                    let prev = b.units.pop().unwrap();
                    line = prev.start_line;
                    f.calls.extend(prev.features.calls);
                    f.idents.extend(prev.features.idents);
                    f.names.extend(prev.features.names);
                } else {
                    break;
                }
            }
        }
        b.push(UnitKind::Return, line, end, depth, f);
    }

    fn if_statement(&self, n: Node, depth: usize, is_else_if: bool, b: &mut UnitBuilder) {
        let line = ts::line(n);
        let cond = n.child_by_field_name("condition");
        let cons = n.child_by_field_name("consequence");
        let alt = n.child_by_field_name("alternative");

        // `if (status != ZX_OK) { return status; }` is how C++ spells `?`.
        if let (Some(c), Some(t), None) = (cond, cons, alt) {
            if self.is_propagation(c, t) {
                if is_else_if {
                    b.push(UnitKind::Else, line, line, depth - 1, Features::default());
                }
                let mut f = self.features(c, &[]);
                f.propagates = true;
                let end = ts::end_line(n);
                let has_init = c.child_by_field_name("initializer").is_some();
                if !has_init && !is_else_if {
                    if let Some(prev) = b.units.last_mut() {
                        let var = propagation_var(self.text(c));
                        if prev.kind == UnitKind::Stmt
                            && prev.depth == depth
                            && !prev.features.propagates
                            && var.is_some_and(|v| self.mentions(prev, &v))
                        {
                            prev.features.propagates = true;
                            prev.end_line = end;
                            return;
                        }
                    }
                }
                b.push(UnitKind::Stmt, line, end, depth, f);
                return;
            }
        }

        // `Foo* x = Get(); if (!x) return ZX_ERR_X;` is how C++ spells
        // `let x = get().ok_or(X)?;`.
        // A standalone `if (!x) return ZX_ERR_X;` or `if (!x) return nullptr;`
        // is how C++ spells `?` on an Option.
        if let (Some(c), Some(t), None) = (cond, cons, alt) {
            if let Some((var, errors)) = self.null_check(c, t) {
                let d = if is_else_if { depth - 1 } else { depth };
                let has_init = c.child_by_field_name("initializer").is_some();
                if !has_init && !is_else_if {
                    if let Some(prev) = b.units.last_mut() {
                        if prev.kind == UnitKind::Stmt
                            && prev.depth == depth
                            && prev.file.is_none()
                            && !prev.features.propagates
                            && self.mentions(prev, &var)
                        {
                            prev.features.propagates = true;
                            prev.features.errors.extend(errors);
                            prev.end_line = ts::end_line(n);
                            return;
                        }
                    }
                }
                if is_else_if {
                    b.push(UnitKind::Else, line, line, depth - 1, Features::default());
                }
                let mut f = self.features(c, &[]);
                f.propagates = true;
                f.errors.extend(errors);
                b.push(UnitKind::Stmt, line, ts::end_line(n), d, f);
                return;
            }
        }

        let header_end = cons.map_or(line, |c| {
            let inner = if c.kind() == "attributed_statement" {
                ts::named_children(c)
                    .into_iter()
                    .find(|x| x.kind() != "attribute_declaration")
                    .unwrap_or(c)
            } else {
                c
            };
            if inner.kind() == "compound_statement" {
                ts::line(inner)
            } else {
                ts::line(c).saturating_sub(1).max(line)
            }
        });
        let mut f = match cond {
            Some(c) => self.features(c, &[]),
            None => Features::default(),
        };
        if let Some(c) = cond {
            self.conjuncts(c, &mut f.conjuncts);
        }
        let kind = if is_else_if { UnitKind::ElseIf } else { UnitKind::If };
        let d = if is_else_if { depth - 1 } else { depth };
        let mut line = line;
        // `zx_status_t status = Foo(); if (status != ZX_OK) { ... }` checks
        // the call itself, as Rust's `if foo().is_err() { ... }` does.
        if let Some(var) = cond.and_then(|c| propagation_var(self.text(c))) {
            if let Some(prev) = b.units.last() {
                if !is_else_if
                    && prev.kind == UnitKind::Stmt
                    && prev.depth == d
                    && prev.file.is_none()
                    && !prev.features.propagates
                    && self.mentions(prev, &var)
                    && !prev.features.calls.is_empty()
                {
                    let prev = b.units.pop().unwrap();
                    line = prev.start_line;
                    f.checks_error = true;
                    f.calls.splice(0..0, prev.features.calls);
                    f.names.extend(prev.features.names);
                    f.idents.extend(prev.features.idents);
                    f.errors.extend(prev.features.errors);
                    f.locks.extend(prev.features.locks);
                }
            }
        }
        b.push(kind, line, header_end, d, f);
        if let Some(c) = cons {
            self.body(c, d + 1, b);
        }
        if let Some(a) = alt {
            // else_clause: `else` followed by a statement.
            let inner = ts::named_children(a).into_iter().find(|x| !ts::is_comment(*x));
            match inner {
                Some(i) if i.kind() == "if_statement" => self.if_statement(i, d + 1, true, b),
                Some(i) => {
                    let header_end =
                        if i.kind() == "compound_statement" { ts::line(i) } else { ts::line(a) };
                    b.push(UnitKind::Else, ts::line(a), header_end, d, Features::default());
                    self.body(i, d + 1, b);
                }
                None => {}
            }
        }
    }

    /// The names each top-level `&&` or `||` operand of a condition mentions.
    fn conjuncts(&self, n: Node, out: &mut Vec<crate::model::Conjunct>) {
        let n = match n.kind() {
            "condition_clause" => match n.child_by_field_name("value") {
                Some(v) => v,
                None => return,
            },
            _ => n,
        };
        let n = strip_parens(n);
        if n.kind() == "binary_expression" {
            let op = n.child_by_field_name("operator").map_or("", |o| self.text(o));
            if matches!(op, "&&" | "||" | "and" | "or") {
                if let (Some(l), Some(r)) =
                    (n.child_by_field_name("left"), n.child_by_field_name("right"))
                {
                    self.conjuncts(l, out);
                    self.conjuncts(r, out);
                    return;
                }
            }
        }
        out.push(crate::model::Conjunct {
            text: self.text(n).split_whitespace().collect::<Vec<_>>().join(" "),
            names: self.features(n, &[]).names,
        });
    }

    /// Whether a unit's source mentions `name` as a whole word.
    fn mentions(&self, u: &crate::model::Unit, name: &str) -> bool {
        ts::mentions(self.lines, u, name)
    }

    /// For `if (!x) return ZX_ERR_X;` (or `x == nullptr`, or an
    /// `fbl::AllocChecker`'s `!ac.check()`), the variable and the error codes
    /// returned.
    fn null_check(&self, cond: Node, cons: Node) -> Option<(String, Vec<String>)> {
        static NULL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"^\(\s*(?:!\s*(\w+)(?:\.check\(\)|\.has_value\(\))?|(\w+)\s*==\s*(?:nullptr|NULL|std::nullopt|ktl::nullopt|nullopt)|(?:nullptr|NULL|std::nullopt|ktl::nullopt|nullopt)\s*==\s*(\w+))\s*\)$",
            )
            .unwrap()
        });
        // `if (unlikely(!x))` tests the same thing.
        static HINT: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^\(\s*(?:un)?likely\s*(\(.*\))\s*\)$").unwrap());
        let text = self.text(cond).trim();
        let text = HINT.captures(text).map_or(text, |c| c.get(1).map_or(text, |m| m.as_str()));
        let c = NULL.captures(text)?;
        let var = (1..=3).find_map(|i| c.get(i))?.as_str().to_string();
        let ret = single_statement(cons)?;
        if ret.kind() != "return_statement" {
            return None;
        }
        // Returning an error code, or null for a failed lookup (which Rust
        // spells `?` on an `Option`).
        let errors = crate::normalize::error_codes(self.text(ret));
        let ret_text = self.text(ret).trim_end_matches(';').trim();
        let null = matches!(
            ret_text,
            "return nullptr"
                | "return NULL"
                | "return 0"
                | "return {}"
                | "return {nullptr}"
                | "return std::nullopt"
                | "return ktl::nullopt"
                | "return nullopt"
        );
        (!errors.is_empty() || null).then_some((var, errors))
    }

    fn is_propagation(&self, cond: Node, cons: Node) -> bool {
        static FAIL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"!=\s*ZX_OK|ZX_OK\s*!=|\.is_error\(\)|!\s*[\w.>-]+(?:\.|->)is_ok\(\)|\bstatus\s*<\s*0|^\(\s*\w*status\s*\)$")
                .unwrap()
        });
        if !FAIL.is_match(self.text(cond)) {
            return false;
        }
        let ret = if cons.kind() == "compound_statement" {
            let stmts: Vec<Node> =
                ts::named_children(cons).into_iter().filter(|c| !ts::is_comment(*c)).collect();
            if stmts.len() != 1 {
                return false;
            }
            stmts[0]
        } else {
            cons
        };
        if ret.kind() != "return_statement" {
            return false;
        }
        let expr = ts::named_children(ret).into_iter().find(|c| !ts::is_comment(*c));
        expr.is_some_and(|e| ts::classify_return(self.text(e)) == Ret::Status)
    }
}

/// Status chaining: `status = A(); if (status == ZX_OK) { status = B(); }
/// return status;` passes each call's error on, as Rust's `a()?; b()?;
/// Ok(())` does. When a status variable is used only that way, its
/// assignments propagate, its `== ZX_OK` guards are bookkeeping, and the
/// final `return status` is the success path.
fn chain_status(units: &mut [crate::model::Unit], lines: &[String]) {
    static RET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^return\s+(\w+)\s*;").unwrap());
    let text = |u: &crate::model::Unit| -> String {
        (u.start_line..=u.end_line)
            .filter_map(|l| lines.get(l - 1))
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let Some(var) = units
        .iter()
        .rev()
        .find(|u| u.kind == UnitKind::Return && u.depth == 1 && u.file.is_none())
        .and_then(|u| RET.captures(&text(u)).map(|c| c[1].to_string()))
    else {
        return;
    };
    let v = regex::escape(&var);
    let word = Regex::new(&format!(r"\b{v}\b")).unwrap();
    // `status = f();`, `zx_status_t status = f();`, or a structured binding
    // `auto [status, n] = f();`.
    let assign = Regex::new(&format!(
        r"^(?:(?:zx_status_t\s+)?{v}|auto\s*\[\s*{v}\s*,[^\]]*\])\s*=[^=].*\w\s*\("
    ))
    .unwrap();
    let decl = Regex::new(&format!(r"^zx_status_t\s+{v}\s*(?:=\s*ZX_OK\s*)?;")).unwrap();
    let guard = Regex::new(&format!(r"\b{v}\s*==\s*ZX_OK\b|\bZX_OK\s*==\s*{v}\b")).unwrap();
    let ret = Regex::new(&format!(r"^return\s+{v}\s*;")).unwrap();
    let mut assigns = Vec::new();
    let mut guards = Vec::new();
    let mut rets = Vec::new();
    for (i, u) in units.iter().enumerate() {
        if u.file.is_some() || u.kind == UnitKind::Comment {
            continue;
        }
        let t = text(u);
        if !word.is_match(&t) {
            continue;
        }
        match u.kind {
            UnitKind::Stmt if assign.is_match(&t) && !u.features.calls.is_empty() => {
                assigns.push(i)
            }
            UnitKind::Stmt if decl.is_match(&t) => {}
            UnitKind::If | UnitKind::ElseIf if guard.is_match(&t) => guards.push(i),
            UnitKind::Return if ret.is_match(&t) => rets.push(i),
            _ => return,
        }
    }
    if assigns.is_empty() || (assigns.len() > 1 && guards.is_empty()) {
        return;
    }
    for i in assigns {
        units[i].features.propagates = true;
    }
    for i in guards {
        let f = &mut units[i].features;
        f.conjuncts.retain(|c| !guard.is_match(&c.text));
        if f.conjuncts.is_empty() {
            f.plumbing = true;
        }
    }
    for i in rets {
        units[i].features.ret = Some(Ret::Ok);
    }
}

/// A status set in each branch and checked once after them:
///
/// ```text
/// if (virt) { status = A(); } else { status = B(); }
/// if (status != ZX_OK) { return status; }
/// ```
///
/// is Rust's `if virt { a()? } else { b()? }`. The assignments propagate,
/// and the lone check is bookkeeping.
fn branch_status(units: &mut [crate::model::Unit], lines: &[String]) {
    for p in 0..units.len() {
        let u = &units[p];
        if u.kind != UnitKind::Stmt || !u.features.propagates || !u.features.calls.is_empty() {
            continue;
        }
        let text = lines.get(u.start_line - 1).map_or("", |l| l.trim());
        let Some(var) = text
            .strip_prefix("if")
            .and_then(|c| propagation_var(c.trim().trim_end_matches('{').trim()))
        else {
            continue;
        };
        let assign = Regex::new(&format!(r"^{}\s*=[^=].*\w\s*\(", regex::escape(&var))).unwrap();
        let depth = u.depth;
        let mut found = Vec::new();
        for k in (0..p).rev() {
            let w = &units[k];
            if w.depth < depth
                || (w.depth == depth
                    && !matches!(
                        w.kind,
                        UnitKind::If | UnitKind::ElseIf | UnitKind::Else | UnitKind::Comment
                    ))
            {
                break;
            }
            let t = lines.get(w.start_line - 1).map_or("", |l| l.trim());
            if w.depth > depth
                && w.kind == UnitKind::Stmt
                && assign.is_match(t)
                && !w.features.calls.is_empty()
            {
                found.push(k);
            }
        }
        if !found.is_empty() {
            for k in found {
                units[k].features.propagates = true;
            }
            units[p].features.plumbing = true;
        }
    }
}

/// The variable a failure check tests, e.g. `status` in `status != ZX_OK`.
fn propagation_var(cond: &str) -> Option<String> {
    static VAR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(\w+)\s*!=\s*ZX_OK|ZX_OK\s*!=\s*(\w+)|(\w+)\s*(?:\.|->)is_error\(\)|!\s*(\w+)\s*(?:\.|->)is_ok\(\)|(\w+)\s*<\s*0|^\(\s*(\w*status)\s*\)$")
            .unwrap()
    });
    let c = VAR.captures(cond)?;
    let v = (1..=6).find_map(|i| c.get(i))?;
    Some(v.as_str().to_string())
}

fn strip_parens(mut n: Node) -> Node {
    while n.kind() == "parenthesized_expression" {
        match ts::named_children(n).into_iter().next() {
            Some(c) => n = c,
            None => break,
        }
    }
    n
}

/// Outermost lambda bodies inside `n`.
fn find_bodies<'t>(n: Node<'t>, out: &mut Vec<Node<'t>>) {
    for c in ts::named_children(n) {
        if c.kind() == "lambda_expression" {
            if let Some(body) = c.child_by_field_name("body") {
                out.push(body);
            }
        } else {
            find_bodies(c, out);
        }
    }
}

fn find_function_declarator(n: Node) -> Option<Node> {
    if n.kind() == "function_declarator" {
        // `Foo(int x) TA_REQ(lock_)` parses as the macro "calling" `Foo(...)`.
        if let Some(inner) =
            n.child_by_field_name("declarator").filter(|d| d.kind() == "function_declarator")
        {
            return find_function_declarator(inner);
        }
        return Some(n);
    }
    if matches!(
        n.kind(),
        "pointer_declarator"
            | "reference_declarator"
            | "attributed_declarator"
            | "parenthesized_declarator"
    ) {
        if let Some(d) = n.child_by_field_name("declarator") {
            return find_function_declarator(d);
        }
        for c in ts::named_children(n) {
            if let Some(f) = find_function_declarator(c) {
                return Some(f);
            }
        }
    }
    None
}

/// Splits a possibly qualified function name into (class, base), using the
/// enclosing class path for inline definitions.
fn split_name(name: &str, class: &[String]) -> (Option<String>, String) {
    let name: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut parts: Vec<String> = class.to_vec();
    // Split on `::` outside template arguments.
    let mut depth = 0;
    let mut cur = String::new();
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '<' {
            depth += 1;
        } else if c == '>' && depth > 0 {
            depth -= 1;
        }
        if depth == 0 && c == ':' && chars.get(i + 1) == Some(&':') {
            parts.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    let base = cur;
    let cls = if parts.is_empty() { None } else { Some(parts.join("::")) };
    (cls, base)
}

/// Whether a type names a class with a constructor worth counting: `Foo`
/// or `ns::Foo`, but not `zx_status_t`, `size_t` or a template like
/// `fbl::RefPtr<T>`.
fn is_class_type(t: &str) -> bool {
    let t = t.trim();
    if t.contains('<') {
        return false;
    }
    let last = t.rsplit("::").next().unwrap_or(t);
    last.starts_with(|c: char| c.is_ascii_uppercase()) && !last.ends_with("_t")
}

/// An initializer with no effect of its own: a value read with no calls,
/// or a call of a zero-argument accessor.
fn is_pure_or_accessor(v: Node, src: &[u8]) -> bool {
    let v = strip_parens(v);
    let mut calls = Vec::new();
    collect_calls(v, &mut calls);
    match calls.as_slice() {
        [] => true,
        [c] if c.id() == v.id() => {
            let no_args = c
                .child_by_field_name("arguments")
                .is_some_and(|a| ts::named_children(a).iter().all(|x| ts::is_comment(*x)));
            let name = c.child_by_field_name("function").map(|f| ts::text(f, src)).unwrap_or("");
            no_args && !crate::normalize::is_mutating(name)
        }
        _ => false,
    }
}

fn collect_calls<'t>(n: Node<'t>, out: &mut Vec<Node<'t>>) {
    if n.kind() == "call_expression" {
        out.push(n);
    }
    for c in ts::named_children(n) {
        collect_calls(c, out);
    }
}

/// `ns::Foo<T, U>` -> `Foo`.
pub fn unqualified_type(t: &str) -> String {
    let t = t.split('<').next().unwrap_or(t);
    t.rsplit("::").next().unwrap_or(t).trim().to_string()
}

/// Removes the `break` that ends a switch case, which Rust match arms don't
/// need. Trailing comments stay.
fn drop_trailing_break(stmts: &mut Vec<Node>) {
    if let Some(i) = stmts.iter().rposition(|c| !ts::is_comment(*c)) {
        // A break inside `case A: { ...; break; }` is handled by `case_body`.
        if stmts[i].kind() == "break_statement" {
            stmts.remove(i);
        }
    }
}

/// The one statement in `n`, looking through braces.
fn single_statement(n: Node) -> Option<Node> {
    if n.kind() != "compound_statement" {
        return Some(n);
    }
    let stmts: Vec<Node> =
        ts::named_children(n).into_iter().filter(|c| !ts::is_comment(*c)).collect();
    (stmts.len() == 1).then(|| stmts[0])
}

/// Blanks out Clang thread-safety and inline annotations (`TA_REQ(lock_)`,
/// `FFI_ALWAYS_INLINE` and friends), which the parser can't place and which
/// otherwise split a method in two or swallow the return type. Lengths and
/// line breaks are kept, so positions don't move.
fn mask_annotations(src: &str) -> String {
    static TA: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(?:(?:__)?TA_[A-Z_]+\b(?:\s*\((?:[^()]|\([^()]*\))*\))?|FFI_ALWAYS_INLINE\b|__ALWAYS_INLINE\b|__NO_INLINE\b)",
        )
        .unwrap()
    });
    let masked = TA.replace_all(src, |c: &regex::Captures| {
        c[0].chars().map(|ch| if ch == '\n' { '\n' } else { ' ' }).collect::<String>()
    });
    rewrite_init_captures(&masked)
}

/// Rewrites lambda init-captures written with braces, `[x{a.begin()}]`, as
/// `[x=a.begin() ]`, which tree-sitter-cpp parses. Without this, a function
/// holding such a lambda swallows the rest of the file. Lengths and line
/// numbers are unchanged.
fn rewrite_init_captures(src: &str) -> String {
    static CAPTURES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\[((?:[^\[\]{}()]|\{[^{}]*\}|\([^()]*\))*)\]\s*(?:\(|mutable\b|\{)").unwrap()
    });
    static INIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\w)\{([^{}]*)\}").unwrap());
    CAPTURES
        .replace_all(src, |c: &regex::Captures| {
            let list = c.get(1).unwrap();
            if !INIT.is_match(list.as_str()) {
                return c[0].to_string();
            }
            let fixed = INIT.replace_all(list.as_str(), "${1}=${2} ");
            let start = list.start() - c.get(0).unwrap().start();
            let end = list.end() - c.get(0).unwrap().start();
            format!("{}{}{}", &c[0][..start], fixed, &c[0][end..])
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rewrites_brace_init_captures() {
        let src = "auto f = [this, it{v.begin()}, n{3}](int x) mutable { return x; };";
        let out = super::rewrite_init_captures(src);
        assert_eq!(out, "auto f = [this, it=v.begin() , n=3 ](int x) mutable { return x; };");
        assert_eq!(out.len(), src.len());
        // Array subscripts and plain captures are left alone.
        let plain = "a[i] = b[j]; auto g = [&x](int y) { return y; };";
        assert_eq!(super::rewrite_init_captures(plain), plain);
    }

    // Test functions take their name from a `UNITTEST` registration or a
    // `TEST` definition; other functions have none.
    #[test]
    fn extracts_test_names() {
        let src = r#"
bool NoGlowTest() {
  BEGIN_TEST;
  END_TEST;
}
bool Helper() { return true; }
UNITTEST_START_TESTCASE(lamp_tests)
UNITTEST("No glow", NoGlowTest)
UNITTEST_END_TESTCASE(lamp_tests, "lamp", "lamp tests")
TEST(LampTest, Glows) {
  EXPECT_TRUE(Glow());
}
"#;
        let file = super::extract("lamp_test.cc", src);
        let names: Vec<(&str, Option<&str>)> =
            file.functions.iter().map(|f| (f.base.as_str(), f.test_name.as_deref())).collect();
        assert_eq!(
            names,
            [("NoGlowTest", Some("No glow")), ("Helper", None), ("TEST", Some("Glows"))]
        );
    }

    #[test]
    fn extracts_multiline_ffi_always_inline_function() {
        let src = "extern \"C\" {\nFFI_ALWAYS_INLINE zx_status_t\ncpp_memory_watchdog_halt_token_wait_for_ack(const Deadline* deadline) {\n  return HaltToken::Get().WaitForAck(*deadline);\n}\n}\n";
        let file = super::extract("memory_watchdog.cc", src);
        assert_eq!(file.functions.len(), 1);
        assert_eq!(file.functions[0].base, "cpp_memory_watchdog_halt_token_wait_for_ack");
        assert_eq!(file.functions[0].name, "cpp_memory_watchdog_halt_token_wait_for_ack");
    }
}
