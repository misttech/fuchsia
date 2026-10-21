//! Extracts functions and their units from Rust source.

use crate::model::{Features, Function, Lang, Ret, Unit, UnitKind};
use crate::normalize;
use crate::ts::{self, FeatureAcc, UnitBuilder};
use regex::Regex;
use std::sync::LazyLock;
use tree_sitter::Node;

pub fn extract(path: &str, src: &str) -> Vec<Function> {
    let tree = ts::parse(Lang::Rust, src);
    let lines = crate::extract::split_lines(src);
    let ctx = Ctx { path, src: src.as_bytes(), lines: &lines };
    let mut out = Vec::new();
    ctx.walk_scope(tree.root_node(), None, &mut out);
    let tests = ctx.test_ranges(tree.root_node());
    for f in &mut out {
        f.test_only = tests.iter().any(|(a, b)| *a <= f.start_line && f.end_line <= *b);
    }
    out
}

struct Ctx<'a> {
    path: &'a str,
    src: &'a [u8],
    lines: &'a [String],
}

fn is_result_type(ty: &str) -> bool {
    static RESULT_TYPE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|::)\s*Result\b").unwrap());
    let ty = ty.strip_prefix("->").unwrap_or(ty).trim();
    RESULT_TYPE.is_match(ty)
}

/// Per-function state threaded through the statement walk.
#[derive(Clone, Copy)]
struct FnCtx {
    returns_value: bool,
    returns_result: bool,
}

impl<'a> Ctx<'a> {
    fn text(&self, n: Node) -> &'a str {
        ts::text(n, self.src)
    }

    /// Line ranges of test modules: `#[cfg(test)]`, `#[cfg(ktest)]`, or a
    /// module named `tests`.
    fn test_ranges(&self, scope: Node) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for child in ts::named_children(scope) {
            if child.kind() != "mod_item" {
                continue;
            }
            let named_tests = child
                .child_by_field_name("name")
                .is_some_and(|n| matches!(self.text(n), "tests" | "test"));
            let mut cfg_test = false;
            let mut cur = child.prev_sibling();
            while let Some(p) = cur.filter(|p| p.kind() == "attribute_item" || ts::is_comment(*p)) {
                let t: String = self.text(p).split_whitespace().collect();
                cfg_test |= t.starts_with("#[cfg(") && (t.contains("test") || t.contains("ktest"));
                cur = p.prev_sibling();
            }
            if named_tests || cfg_test {
                out.push((ts::line(child), ts::end_line(child)));
            } else if let Some(body) = child.child_by_field_name("body") {
                out.extend(self.test_ranges(body));
            }
        }
        out
    }

    fn walk_scope(&self, scope: Node, class: Option<&str>, out: &mut Vec<Function>) {
        for child in ts::named_children(scope) {
            match child.kind() {
                "function_item" => {
                    let gap = child
                        .child_by_field_name("name")
                        .is_some_and(|n| self.text(n) == crate::patch::GAP_FN);
                    if gap {
                        continue;
                    }
                    if let Some(f) = self.function(child, class) {
                        out.push(f);
                    }
                }
                // Error recovery can wrap items; keep looking inside.
                "ERROR" => self.walk_scope(child, class, out),
                "mod_item" | "foreign_mod_item" => {
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk_scope(body, class, out);
                    }
                }
                "impl_item" => {
                    let ty = child
                        .child_by_field_name("type")
                        .map(|t| type_name(self.text(t)))
                        .filter(|t| t != crate::patch::GAP_TYPE);
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk_scope(body, ty.as_deref(), out);
                    }
                }
                "trait_item" => {
                    let name = child.child_by_field_name("name").map(|t| self.text(t).to_string());
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk_scope(body, name.as_deref(), out);
                    }
                }
                _ => {}
            }
        }
    }

    fn function(&self, n: Node, class: Option<&str>) -> Option<Function> {
        let base = self.text(n.child_by_field_name("name")?).to_string();
        let body = n.child_by_field_name("body")?;
        let mut b = UnitBuilder::new();

        // Leading doc comments and attributes.
        let mut leading = Vec::new();
        let mut first = ts::line(n);
        let mut is_ffi = false;
        let mut cur = n.prev_sibling();
        while let Some(p) = cur {
            let is_attr = p.kind() == "attribute_item";
            if !(is_attr || ts::is_comment(p)) || ts::end_line(p) + 1 < first {
                break;
            }
            if let Some(pp) = p.prev_sibling() {
                if ts::end_line(pp) == ts::line(p)
                    && !ts::is_comment(pp)
                    && pp.kind() != "attribute_item"
                {
                    break;
                }
            }
            if is_attr
                && (self.text(p).contains("no_mangle") || self.text(p).contains("export_name"))
            {
                is_ffi = true;
            }
            first = ts::line(p);
            leading.push(p);
            cur = p.prev_sibling();
        }
        for p in leading.iter().rev() {
            if ts::is_comment(*p) {
                b.comment(self.text(*p), ts::line(*p), ts::end_line(*p), 0);
            }
        }
        let is_pub = ts::children(n).iter().any(|c| c.kind() == "visibility_modifier");
        for c in ts::children(n) {
            if is_pub && c.kind() == "function_modifiers" && self.text(c).contains("extern") {
                is_ffi = true;
            }
        }

        let mut acc = FeatureAcc::default();
        acc.ident(&base);
        if let Some(params) = n.child_by_field_name("parameters") {
            self.collect(params, &mut acc, &[]);
        }
        let sig = acc.finish(Lang::Rust);
        b.push(UnitKind::Signature, ts::line(n), ts::line(body), 0, sig);

        let returns_value =
            n.child_by_field_name("return_type").is_some_and(|t| self.text(t).trim() != "()");
        let returns_result =
            n.child_by_field_name("return_type").is_some_and(|t| is_result_type(self.text(t)));
        let fcx = FnCtx { returns_value, returns_result };
        self.block(body, 1, true, fcx, &mut b);
        relocate_closures(&mut b.units, self.lines);
        let ends_ok_unit = b.units.last().is_some_and(|u| {
            let text: String = (u.start_line..=u.end_line)
                .filter_map(|l| self.lines.get(l - 1))
                .map(|l| l.trim())
                .collect();
            text.starts_with("Ok(())") || text.starts_with("returnOk(())")
        });
        if ends_ok_unit {
            fold_status_tail(&mut b.units, self.lines);
        }
        for u in &mut b.units {
            if u.kind == UnitKind::Stmt && u.file.is_none() {
                let text: String = (u.start_line..=u.end_line)
                    .filter_map(|l| self.lines.get(l - 1))
                    .map(|l| l.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                u.features.lock_plumbing = is_lock_plumbing(&text);
            }
        }

        let end = ts::end_line(n);
        let mut calls: Vec<String> =
            b.units.iter().flat_map(|u| u.features.calls.clone()).collect();
        calls.sort();
        calls.dedup();
        let mut qcalls: Vec<String> =
            b.units.iter().flat_map(|u| u.features.qcalls.clone()).collect();
        qcalls.sort();
        qcalls.dedup();
        let name = match class {
            Some(c) => format!("{c}::{base}"),
            None => base.clone(),
        };
        Some(Function {
            lang: Lang::Rust,
            path: self.path.to_string(),
            name,
            base,
            class: class.map(str::to_string),
            start_line: first,
            end_line: end,
            lines: self.lines[first - 1..end.min(self.lines.len())].to_vec(),
            units: b.units,
            calls,
            qcalls,
            is_ffi,
            is_pub,
            test_only: false,
            test_name: None,
        })
    }

    fn collect(&self, n: Node, acc: &mut FeatureAcc, skip: &[Node]) {
        static MACRO_CALL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"\b((?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*)\s*(?:::\s*<[^>()]*>)?\s*\(")
                .unwrap()
        });
        static STRING: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#""(?:[^"\\]|\\.)*""#).unwrap());
        if skip.iter().any(|s| s.id() == n.id()) {
            return;
        }
        match n.kind() {
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") {
                    let is_iter_take = f.kind() == "field_expression"
                        && f.child_by_field_name("field")
                            .is_some_and(|fld| self.text(fld) == "take")
                        && n.child_by_field_name("arguments")
                            .is_some_and(|a| a.named_child_count() > 0);
                    if !is_iter_take {
                        acc.call(self.text(f));
                    }
                }
            }
            "macro_invocation" => {
                if let Some(m) = n.child_by_field_name("macro") {
                    acc.call(self.text(m));
                }
                for tt in ts::named_children(n).into_iter().filter(|c| c.kind() == "token_tree") {
                    // Words in a message string (`"copy_(to|from)_user ..."`)
                    // are not calls.
                    let t = ts::text_without(tt, self.src, skip);
                    let t = STRING.replace_all(&t, "\"\"");
                    for c in MACRO_CALL.captures_iter(&t) {
                        acc.call(&c[1]);
                    }
                    self.collect(tt, acc, skip);
                }
                // The macro's own path is not an identifier of interest.
                return;
            }
            "try_expression" => acc.propagates = true,
            "identifier" | "field_identifier" => acc.ident(self.text(n)),
            _ => {}
        }
        for c in ts::named_children(n) {
            self.collect(c, acc, skip);
        }
    }

    fn features(&self, n: Node, skip: &[Node]) -> Features {
        let mut acc = FeatureAcc::default();
        self.collect(n, &mut acc, skip);
        acc.text = ts::text_without(n, self.src, skip);
        acc.finish(Lang::Rust)
    }

    /// Walks a block. `tail` says whether the block's value is the function's
    /// return value.
    fn block(&self, n: Node, depth: usize, tail: bool, fcx: FnCtx, b: &mut UnitBuilder) {
        let children = ts::named_children(n);
        let last = children.iter().rposition(|c| !ts::is_comment(*c));
        // A statement under `#[cfg(..)]` goes one level below the attribute.
        let mut cfg = false;
        for (i, c) in children.iter().enumerate() {
            if c.kind() == "attribute_item" {
                let t = self.text(*c);
                if t.trim_start().starts_with("#[cfg(") {
                    let f = Features { idents: normalize::cfg_words(t), ..Features::default() };
                    b.push(UnitKind::Cfg, ts::line(*c), ts::end_line(*c), depth, f);
                    cfg = true;
                }
                continue;
            }
            let is_tail = tail && Some(i) == last && self.is_value_position(*c);
            let d = if cfg && !ts::is_comment(*c) {
                cfg = false;
                depth + 1
            } else {
                depth
            };
            self.statement(*c, d, is_tail, fcx, b);
        }
    }

    /// Whether a block's last child produces the block's value (no trailing
    /// semicolon).
    fn is_value_position(&self, n: Node) -> bool {
        match n.kind() {
            "let_declaration" | "function_item" | "const_item" | "static_item"
            | "use_declaration" | "struct_item" | "enum_item" | "impl_item"
            | "macro_definition" => false,
            "expression_statement" => !self.text(n).trim_end().ends_with(';'),
            _ => true,
        }
    }

    fn statement(&self, n: Node, depth: usize, tail: bool, fcx: FnCtx, b: &mut UnitBuilder) {
        match n.kind() {
            "line_comment" | "block_comment" => {
                b.comment(self.text(n), ts::line(n), ts::end_line(n), depth)
            }
            "let_declaration" => self.let_declaration(n, depth, fcx, b),
            "expression_statement" => {
                let inner = ts::named_children(n).into_iter().find(|c| !ts::is_comment(*c));
                if let Some(e) = inner {
                    self.expression(e, n, depth, tail, fcx, b)
                }
            }
            "attribute_item" | "empty_statement" => {}
            _ => self.expression(n, n, depth, tail, fcx, b),
        }
    }

    /// `stmt` is the enclosing statement, whose lines the unit covers.
    fn expression(
        &self,
        e: Node,
        stmt: Node,
        depth: usize,
        tail: bool,
        fcx: FnCtx,
        b: &mut UnitBuilder,
    ) {
        let (line, end) = (ts::line(stmt), ts::end_line(stmt));
        match e.kind() {
            "if_expression" => self.if_expression(e, depth, tail, false, fcx, b),
            "match_expression" => self.match_expression(e, depth, tail, fcx, b),
            "loop_expression" | "while_expression" | "for_expression" => {
                let body = e.child_by_field_name("body");
                let header_end = body.map_or(end, |bd| ts::line(bd));
                let skip: Vec<Node> = body.into_iter().collect();
                b.push(UnitKind::Loop, line, header_end, depth, self.features(e, &skip));
                if let Some(bd) = body {
                    self.block(bd, depth + 1, false, fcx, b);
                }
            }
            "return_expression" => {
                let inner = ts::named_children(e).into_iter().next();
                self.return_unit(inner, line, end, depth, fcx, b);
            }
            "break_expression" => b.push(UnitKind::Break, line, end, depth, Features::default()),
            "continue_expression" => {
                b.push(UnitKind::Continue, line, end, depth, Features::default())
            }
            "unsafe_block" | "block" => {
                let inner = if e.kind() == "unsafe_block" {
                    ts::named_children(e).into_iter().find(|c| c.kind() == "block")
                } else {
                    Some(e)
                };
                if let Some(bl) = inner {
                    if ts::end_line(bl) == ts::line(bl) && !tail {
                        self.plain(stmt, depth, b);
                    } else {
                        self.block(bl, depth, tail, fcx, b);
                    }
                }
            }
            "call_expression" if self.lock_callback(e).is_some() => {
                // `with_chain_lock(t, |t| { ... })` holds the lock while the
                // closure runs, the way a C++ guard holds it to the end of
                // its scope: the call lines up with the guard, and the
                // closure's statements with the statements after it, at
                // the same depth.
                let body = self.lock_callback(e).unwrap();
                b.push(UnitKind::Stmt, line, ts::line(body), depth, self.features(e, &[body]));
                self.block(body, depth, tail, fcx, b);
            }
            _ if tail && fcx.returns_value => self.return_unit(Some(e), line, end, depth, fcx, b),
            _ => self.plain(stmt, depth, b),
        }
    }

    /// The block of the closure passed to a lock helper
    /// (`with_chain_lock(t, |t| { ... })`), when the closure spans lines.
    fn lock_callback<'t>(&self, call: Node<'t>) -> Option<Node<'t>> {
        let f = call.child_by_field_name("function")?;
        let name = self.text(f).rsplit("::").next()?.trim();
        if !(name.starts_with("with_") && name.contains("lock")) {
            return None;
        }
        let args = call.child_by_field_name("arguments")?;
        let closure =
            ts::named_children(args).into_iter().rfind(|c| c.kind() == "closure_expression")?;
        let body = closure.child_by_field_name("body")?;
        (body.kind() == "block" && ts::end_line(body) > ts::line(body)).then_some(body)
    }

    fn return_unit(
        &self,
        expr: Option<Node>,
        line: usize,
        end: usize,
        depth: usize,
        fcx: FnCtx,
        b: &mut UnitBuilder,
    ) {
        // `return Err(if c { A } else { B })` reads as
        // `if c { return Err(A) } else { return Err(B) }`, which is how C++
        // spells `return c ? A : B;`.
        if let Some(inner) = expr.and_then(|e| self.wrapped_if(e)) {
            if let (Some(c), Some(t), Some(a)) = (
                inner.child_by_field_name("condition"),
                inner.child_by_field_name("consequence"),
                inner.child_by_field_name("alternative"),
            ) {
                let alt_block = ts::named_children(a).into_iter().find(|x| x.kind() == "block");
                if let Some(e) = alt_block {
                    b.push(UnitKind::If, line, end, depth, self.features(c, &[]));
                    self.return_unit(Some(t), line, end, depth + 1, fcx, b);
                    b.push(UnitKind::Else, line, end, depth, Features::default());
                    self.return_unit(Some(e), line, end, depth + 1, fcx, b);
                    return;
                }
            }
        }
        if end > line
            && b.units.iter().all(|u| matches!(u.kind, UnitKind::Signature | UnitKind::Comment))
        {
            if let Some(e) = expr {
                if self.split_struct_init(e, depth, b) {
                    return;
                }
            }
        }
        let mut f = match expr {
            Some(e) => self.features(e, &[]),
            None => Features::default(),
        };
        f.ret = Some(match expr {
            Some(e) => ts::classify_return(self.text(e)),
            None => Ret::Value,
        });
        if fcx.returns_result
            && matches!(f.ret, Some(Ret::Value) | Some(Ret::Status))
            && !f.calls.is_empty()
        {
            f.ret = Some(Ret::Status);
        }
        b.push(UnitKind::Return, line, end, depth, f);
    }

    /// Splits a multi-field constructor return (`Self { ... }` or
    /// `pin_init!(Self { ... })`) into one `UnitKind::Stmt` per field
    /// initializer so it aligns field-by-field with C++ member initializers.
    fn split_struct_init(&self, e: Node, depth: usize, b: &mut UnitBuilder) -> bool {
        if e.kind() == "struct_expression" {
            let Some(body) = e.child_by_field_name("body") else {
                return false;
            };
            let fields: Vec<Node> = ts::named_children(body)
                .into_iter()
                .filter(|c| matches!(c.kind(), "field_initializer" | "shorthand_field_initializer"))
                .collect();
            if fields.len() < 2 {
                return false;
            }
            for fi in fields {
                let mut f = self.features(fi, &[]);
                f.plumbing = f.calls.is_empty()
                    && f.errors.is_empty()
                    && f.locks.is_empty()
                    && !f.propagates;
                b.push(UnitKind::Stmt, ts::line(fi), ts::end_line(fi), depth, f);
            }
            return true;
        }
        if e.kind() == "macro_invocation" {
            let is_pin_init = e
                .child_by_field_name("macro")
                .is_some_and(|m| self.text(m).trim().ends_with("pin_init"));
            if !is_pin_init {
                return false;
            }
            let Some(outer_tt) =
                ts::named_children(e).into_iter().find(|c| c.kind() == "token_tree")
            else {
                return false;
            };
            let Some(brace_tt) = ts::named_children(outer_tt)
                .into_iter()
                .find(|c| c.kind() == "token_tree" && self.text(*c).starts_with('{'))
            else {
                return false;
            };
            static MACRO_CALL: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r"\b((?:[A-Za-z_]\w*\s*::\s*)*[A-Za-z_]\w*)\s*(?:::\s*<[^>()]*>)?\s*\(")
                    .unwrap()
            });
            static IDENT: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"\b[A-Za-z_]\w*\b").unwrap());
            let mut items = Vec::new();
            for line_idx in ts::line(brace_tt)..=ts::end_line(brace_tt) {
                let Some(raw) = self.lines.get(line_idx - 1) else {
                    continue;
                };
                let code = raw.split("//").next().unwrap_or("").trim();
                if code.is_empty() || code == "{" || code.starts_with('}') {
                    continue;
                }
                if !(code.contains(": ") || code.contains("<-")) {
                    continue;
                }
                let mut acc = FeatureAcc::default();
                for c in MACRO_CALL.captures_iter(code) {
                    acc.call(&c[1]);
                }
                for m in IDENT.find_iter(code) {
                    let w = m.as_str();
                    if !matches!(w, "false" | "true" | "Self" | "let" | "mut" | "unsafe") {
                        acc.ident(w);
                    }
                }
                acc.text = code.to_string();
                let mut f = acc.finish(Lang::Rust);
                f.plumbing = f.calls.is_empty()
                    && f.errors.is_empty()
                    && f.locks.is_empty()
                    && !f.propagates;
                items.push((line_idx, f));
            }
            if items.len() < 2 {
                return false;
            }
            for (ln, f) in items {
                b.push(UnitKind::Stmt, ln, ln, depth, f);
            }
            return true;
        }
        false
    }

    /// A plain statement; multi-line closure and nested function bodies are split out.
    fn plain(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let mut bodies = Vec::new();
        find_closure_bodies(n, &mut bodies);
        let bodies: Vec<Node> =
            bodies.into_iter().filter(|x| ts::end_line(*x) > ts::line(*x)).collect();
        let mut f = self.features(n, &bodies);
        if matches!(n.kind(), "integer_literal" | "boolean_literal" | "unit_expression") {
            f.plumbing = true;
        }
        let end = bodies.first().map_or(ts::end_line(n), |x| ts::line(*x));
        b.push(UnitKind::Stmt, ts::line(n), end, depth, f);
        for body in bodies {
            let fcx = FnCtx { returns_value: false, returns_result: false };
            if body.kind() == "block" {
                self.block(body, depth + 1, false, fcx, b);
            } else {
                self.plain(body, depth + 1, b);
            }
        }
    }

    fn let_declaration(&self, n: Node, depth: usize, fcx: FnCtx, b: &mut UnitBuilder) {
        let line = ts::line(n);
        let value = n.child_by_field_name("value");
        if let Some(alt) = n.child_by_field_name("alternative") {
            // `let Some(x) = foo() else { return ... };` is a failure check,
            // like the C++ `if (!x) { return ...; }` it replaces.
            let skip = [alt];
            b.push(UnitKind::If, line, ts::line(alt), depth, self.features(n, &skip));
            self.block(alt, depth + 1, false, fcx, b);
            return;
        }
        if let Some((err_arm, err_val)) = value.and_then(|v| self.unwrap_match(v)) {
            // `let h = match foo() { Ok(h) => h, Err(status) => { ... } };`
            // is `let-else` when the `Err` payload is needed in the failure
            // block, matching C++ `status = foo(&h); if (status != ZX_OK) { ... }`.
            let skip = [err_val];
            let mut f = self.features(n, &skip);
            f.checks_error = true;
            b.push(UnitKind::If, line, ts::line(err_arm), depth, f);
            let inner_fcx = FnCtx { returns_value: false, returns_result: false };
            if err_val.kind() == "block" {
                self.block(err_val, depth + 1, false, inner_fcx, b);
            } else {
                self.expression(err_val, err_val, depth + 1, false, inner_fcx, b);
            }
            return;
        }
        if let Some(v) = value {
            let compound = matches!(
                v.kind(),
                "if_expression" | "match_expression" | "block" | "unsafe_block" | "loop_expression"
            );
            if compound && ts::end_line(v) > ts::line(v) {
                let skip = [v];
                let mut f = self.features(n, &skip);
                if v.kind() == "unsafe_block" || v.kind() == "block" {
                    // The block's statements become their own units.
                    f.propagates = false;
                }
                b.push(UnitKind::Stmt, line, ts::line(v), depth, f);
                let inner_fcx = FnCtx { returns_value: false, returns_result: false };
                match v.kind() {
                    "if_expression" => self.if_expression(v, depth + 1, false, false, inner_fcx, b),
                    "match_expression" => self.match_expression(v, depth + 1, false, inner_fcx, b),
                    _ => self.expression(v, v, depth + 1, false, inner_fcx, b),
                }
                return;
            }
        }
        self.plain(n, depth, b);
        if let Some(last) = b.units.last_mut() {
            if last.start_line == line
                && last.kind == UnitKind::Stmt
                && last.features.errors.is_empty()
                && last.features.locks.is_empty()
                && !last.features.propagates
                && value.is_none_or(|v| is_pure_or_accessor(v, self.src))
            {
                last.features.plumbing = true;
            }
        }
    }

    /// Detects `match expr { Ok(x) => x, Err(e) => { ... } }` used as the
    /// right-hand side of a `let` binding, returning `(err_arm, err_value)`.
    fn unwrap_match<'t>(&self, v: Node<'t>) -> Option<(Node<'t>, Node<'t>)> {
        if v.kind() != "match_expression" {
            return None;
        }
        let body = v.child_by_field_name("body")?;
        let arms: Vec<Node<'t>> =
            ts::named_children(body).into_iter().filter(|c| c.kind() == "match_arm").collect();
        if arms.len() != 2 {
            return None;
        }
        let is_ok_passthrough = |arm: Node<'t>| {
            let pat = arm.child_by_field_name("pattern")?;
            let val = arm.child_by_field_name("value")?;
            let pt = self.text(pat).trim();
            ((pt.starts_with("Ok(") || pt.starts_with("Some(")) && val.kind() == "identifier")
                .then_some(())
        };
        let (ok_idx, err_idx) = if is_ok_passthrough(arms[0]).is_some() {
            (0, 1)
        } else if is_ok_passthrough(arms[1]).is_some() {
            (1, 0)
        } else {
            return None;
        };
        let _ = ok_idx;
        let err_arm = arms[err_idx];
        let err_pat = self.text(err_arm.child_by_field_name("pattern")?).trim();
        if !(err_pat.starts_with("Err(") || err_pat == "None" || err_pat == "_") {
            return None;
        }
        let err_val = err_arm.child_by_field_name("value")?;
        Some((err_arm, err_val))
    }

    fn if_expression(
        &self,
        n: Node,
        depth: usize,
        tail: bool,
        is_else_if: bool,
        fcx: FnCtx,
        b: &mut UnitBuilder,
    ) {
        let line = ts::line(n);
        let cond = n.child_by_field_name("condition");
        let cons = n.child_by_field_name("consequence");
        let alt = n.child_by_field_name("alternative");
        let d = if is_else_if { depth - 1 } else { depth };

        // `if let Err(e) = foo() { return Err(e); }` is `foo()?`, and so is
        // `let r = foo(); if r.is_err() { return r; }`, which folds into the
        // statement that sets `r`, as the C++ `if (r.is_error()) return r;`
        // does.
        if let (Some(c), Some(t), None) = (cond, cons, alt) {
            if self.is_propagation(c, t) {
                if is_else_if {
                    b.push(UnitKind::Else, line, line, d, Features::default());
                } else if let Some(var) = is_err_var(self.text(c)) {
                    if let Some(prev) = b.units.last_mut() {
                        if prev.kind == UnitKind::Stmt
                            && prev.depth == d
                            && prev.file.is_none()
                            && !prev.features.propagates
                            && ts::mentions(self.lines, prev, var)
                        {
                            prev.features.propagates = true;
                            prev.end_line = ts::end_line(n);
                            return;
                        }
                    }
                }
                let mut f = self.features(c, &[]);
                f.propagates = true;
                b.push(
                    UnitKind::Stmt,
                    line,
                    ts::end_line(n),
                    if is_else_if { d + 1 } else { d },
                    f,
                );
                return;
            }
        }

        let header_end = cons.map_or(line, |c| ts::line(c));
        let mut f = match cond {
            Some(c) => self.features(c, &[]),
            None => Features::default(),
        };
        if let Some(c) = cond {
            let t = self.text(c);
            f.checks_error = t.contains(".is_err()") || t.trim_start().starts_with("let Err");
            self.conjuncts(c, &mut f.conjuncts);
        }
        // `if cfg!(..)` chooses at compile time, like C++ `#if`.
        let cfg = cond.is_some_and(|c| {
            c.kind() == "macro_invocation" && self.text(c).trim_start().starts_with("cfg!")
        });
        if let (true, Some(c)) = (cfg, cond) {
            f = Features { idents: normalize::cfg_words(self.text(c)), ..Features::default() };
        }
        let kind = if cfg {
            UnitKind::Cfg
        } else if is_else_if {
            UnitKind::ElseIf
        } else {
            UnitKind::If
        };
        let mut line = line;
        // `let status = foo(); if let Err(status) = status { ... }` checks
        // the call itself, just as C++ `status = foo(); if (status != ZX_OK)`
        // merges the call into the `if` unit.
        if let Some(var) = cond.and_then(|c| is_err_var(self.text(c))) {
            if let Some(prev) = b.units.last() {
                if !is_else_if
                    && kind == UnitKind::If
                    && prev.kind == UnitKind::Stmt
                    && prev.depth == d
                    && prev.file.is_none()
                    && !prev.features.propagates
                    && ts::mentions(self.lines, prev, var)
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
            self.block(c, d + 1, tail, fcx, b);
        }
        if let Some(a) = alt {
            let inner = ts::named_children(a).into_iter().find(|x| !ts::is_comment(*x));
            match inner {
                Some(i) if i.kind() == "if_expression" => {
                    self.if_expression(i, d + 1, tail, true, fcx, b)
                }
                Some(i) => {
                    // The `else` keyword sits on the line where the else clause starts.
                    b.push(UnitKind::Else, ts::line(a), ts::line(i), d, Features::default());
                    self.block(i, d + 1, tail, fcx, b);
                }
                None => {}
            }
        }
    }

    /// The names each top-level `&&` or `||` operand of a condition mentions.
    fn conjuncts(&self, n: Node, out: &mut Vec<crate::model::Conjunct>) {
        let mut n = n;
        while n.kind() == "parenthesized_expression"
            || (n.kind() == "unary_expression" && self.text(n).trim_start().starts_with("!("))
        {
            match ts::named_children(n).into_iter().next() {
                Some(c) => n = c,
                None => break,
            }
        }
        match n.kind() {
            "binary_expression" => {
                let op = n.child_by_field_name("operator").map_or("", |o| self.text(o));
                if matches!(op, "&&" | "||") {
                    if let (Some(l), Some(r)) =
                        (n.child_by_field_name("left"), n.child_by_field_name("right"))
                    {
                        self.conjuncts(l, out);
                        self.conjuncts(r, out);
                        return;
                    }
                }
            }
            "let_chain" => {
                for c in ts::named_children(n) {
                    self.conjuncts(c, out);
                }
                return;
            }
            _ => {}
        }
        out.push(crate::model::Conjunct {
            text: self.text(n).split_whitespace().collect::<Vec<_>>().join(" "),
            names: self.features(n, &[]).names,
        });
    }

    /// The `if` in `if ...`, `Err(if ...)` or `Ok(if ...)`.
    fn wrapped_if<'t>(&self, e: Node<'t>) -> Option<Node<'t>> {
        match e.kind() {
            "if_expression" => Some(e),
            "call_expression" => {
                let f = self.text(e.child_by_field_name("function")?);
                if !matches!(f, "Err" | "Ok") {
                    return None;
                }
                let args = ts::named_children(e.child_by_field_name("arguments")?);
                match args.as_slice() {
                    [a] if a.kind() == "if_expression" => Some(*a),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn is_propagation(&self, cond: Node, cons: Node) -> bool {
        static ERR_PAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^let\s+Err\s*\(").unwrap());
        let err_let = cond.kind() == "let_condition" && ERR_PAT.is_match(self.text(cond).trim());
        if !err_let && is_err_var(self.text(cond)).is_none() {
            return false;
        }
        let stmts: Vec<Node> =
            ts::named_children(cons).into_iter().filter(|c| !ts::is_comment(*c)).collect();
        if stmts.len() != 1 {
            return false;
        }
        let t = self.text(stmts[0]);
        t.trim_start().starts_with("return")
            && ts::classify_return(
                t.trim_start().trim_start_matches("return").trim().trim_end_matches(';'),
            ) == Ret::Status
    }

    fn match_expression(&self, n: Node, depth: usize, tail: bool, fcx: FnCtx, b: &mut UnitBuilder) {
        let body = n.child_by_field_name("body");
        let header_end = body.map_or(ts::line(n), |bd| ts::line(bd));
        let f = match n.child_by_field_name("value") {
            Some(v) => self.features(v, &[]),
            None => Features::default(),
        };
        b.push(UnitKind::Switch, ts::line(n), header_end, depth, f);
        let Some(body) = body else { return };
        for arm in ts::named_children(body) {
            if ts::is_comment(arm) {
                b.comment(self.text(arm), ts::line(arm), ts::end_line(arm), depth + 1);
                continue;
            }
            if arm.kind() != "match_arm" {
                continue;
            }
            let pattern = arm.child_by_field_name("pattern");
            let value = arm.child_by_field_name("value");
            let mut f = match pattern {
                Some(p) => self.features(p, &[]),
                None => Features::default(),
            };
            if pattern.is_some_and(|p| self.text(p).trim() == "_") {
                f.idents.push("default".to_string());
            }
            // `#[cfg(..)]` on an arm, like C++ `#if` around a `case`.
            for a in ts::named_children(arm) {
                let t = self.text(a);
                if a.kind() == "attribute_item" && t.trim_start().starts_with("#[cfg(") {
                    let cf = Features { idents: normalize::cfg_words(t), ..Features::default() };
                    b.push(UnitKind::Cfg, ts::line(a), ts::end_line(a), depth + 1, cf);
                }
            }
            let line = pattern.map_or(ts::line(arm), |p| ts::line(p));
            let header_end = value.map_or(line, |v| ts::line(v));
            b.push(UnitKind::Case, line, header_end, depth + 1, f);
            // Comments between the alternatives of a merged arm
            // (`0x4e /* Skylake */ | 0x5e /* Kaby Lake */ => ...`), which
            // C++ writes one per `case`.
            let mut notes = Vec::new();
            if let Some(p) = pattern {
                comments_within(p, &mut notes);
            }
            for c in notes {
                b.comment(self.text(c), ts::line(c), ts::end_line(c), depth + 1);
            }
            if let Some(v) = value {
                match v.kind() {
                    "block" => self.block(v, depth + 2, tail, fcx, b),
                    _ => self.expression(v, v, depth + 2, tail, fcx, b),
                }
            }
            for c in ts::named_children(arm) {
                if ts::is_comment(c) {
                    b.comment(self.text(c), ts::line(c), ts::end_line(c), depth + 2);
                }
            }
        }
    }
}

/// An initializer with no effect of its own: a value read with no calls
/// (`*fields.timestamp`, `unsafe { &*self.dispatcher }`), or a call of a
/// zero-argument accessor (`self.state()`).
fn is_pure_or_accessor(v: Node, src: &[u8]) -> bool {
    fn calls<'t>(n: Node<'t>, src: &[u8], out: &mut Vec<Node<'t>>) {
        match n.kind() {
            "call_expression" => {
                let name =
                    n.child_by_field_name("function").map(|f| ts::text(f, src)).unwrap_or("");
                let is_plumbing_conversion =
                    crate::normalize::call(name).is_none() && !crate::normalize::is_mutating(name);
                if !is_plumbing_conversion {
                    out.push(n);
                }
            }
            "macro_invocation" | "try_expression" => out.push(n),
            _ => {}
        }
        for c in ts::named_children(n) {
            calls(c, src, out);
        }
    }
    let mut v = v;
    loop {
        if v.kind() == "type_cast_expression" {
            match v.child_by_field_name("value") {
                Some(x) => {
                    v = x;
                    continue;
                }
                None => break,
            }
        }
        if !matches!(
            v.kind(),
            "parenthesized_expression"
                | "unsafe_block"
                | "block"
                | "reference_expression"
                | "unary_expression"
        ) {
            break;
        }
        let inner: Vec<Node> = ts::named_children(v)
            .into_iter()
            .filter(|c| !ts::is_comment(*c) && c.kind() != "mutable_specifier")
            .collect();
        match inner.as_slice() {
            [one]
                if v.kind() != "block"
                    || one.kind().ends_with("expression")
                    || one.kind() == "field_expression"
                    || one.kind() == "identifier" =>
            {
                v = *one
            }
            [one] if v.kind() == "block" && one.kind() == "expression_statement" => v = *one,
            _ => break,
        }
    }
    let mut found = Vec::new();
    calls(v, src, &mut found);
    match found.as_slice() {
        [] => true,
        [c] if c.id() == v.id() && c.kind() == "call_expression" => {
            // No arguments, or just the object it reads from
            // (`thread::get_arch(thread)` for C++ `thread->arch()`).
            let no_args = c.child_by_field_name("arguments").is_some_and(|a| {
                let args: Vec<Node> =
                    ts::named_children(a).into_iter().filter(|x| !ts::is_comment(*x)).collect();
                match args.as_slice() {
                    [] => true,
                    [one] => one.kind() == "identifier" || one.kind() == "self",
                    _ => false,
                }
            });
            let name = c.child_by_field_name("function").map(|f| ts::text(f, src)).unwrap_or("");
            no_args && !crate::normalize::is_mutating(name)
        }
        _ => false,
    }
}

/// A closure bound to a name (`let f = |t| { ... };`) and later passed to
/// a function that takes a lock around it (`with_chain_lock(t, f)`) runs
/// its body under that lock, where the C++ has a guard on the stack and
/// the same statements in place. The lock is credited to the closure's
/// `let`, so the guard lines up there and the body follows it.
/// The variable an `x.is_err()` or `let Err(..) = x` condition tests.
fn is_err_var(cond: &str) -> Option<&str> {
    static IS_ERR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\(?\s*(?:(\w+)\.is_err\(\)|let\s+Err\s*\([^)]*\)\s*=\s*(\w+))\s*\)?$")
            .unwrap()
    });
    IS_ERR.captures(cond.trim()).and_then(|c| c.get(1).or_else(|| c.get(2))).map(|m| m.as_str())
}

fn relocate_closures(units: &mut [Unit], lines: &[String]) {
    static LET_CLOSURE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\blet\s+(?:mut\s+)?([A-Za-z_]\w*)\s*(?::[^=]*)?=\s*(?:move\s+)?\|").unwrap()
    });
    let text = |u: &Unit| -> String {
        (u.start_line..=u.end_line)
            .filter_map(|l| lines.get(l - 1))
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ")
    };
    for i in 0..units.len() {
        if units[i].kind != UnitKind::Stmt {
            continue;
        }
        let Some(name) = LET_CLOSURE.captures(&text(&units[i])).map(|c| c[1].to_string()) else {
            continue;
        };
        let depth = units[i].depth;
        let mut end = i + 1;
        while end < units.len() && units[end].depth > depth {
            end += 1;
        }
        if end == i + 1 {
            continue;
        }
        let used =
            Regex::new(&format!(r"[(,]\s*(?:&\s*(?:mut\s+)?)?{}\s*[),]", regex::escape(&name)))
                .unwrap();
        let user = (end..units.len()).find(|&k| {
            units[k].kind != UnitKind::Comment
                && !units[k].features.locks.is_empty()
                && used.is_match(&text(&units[k]))
        });
        if let Some(k) = user {
            let locks = std::mem::take(&mut units[k].features.locks);
            units[i].features.locks.extend(locks);
        }
    }
}

/// Every comment inside a node.
fn comments_within<'t>(n: Node<'t>, out: &mut Vec<Node<'t>>) {
    for c in ts::children(n) {
        if ts::is_comment(c) {
            out.push(c);
        } else {
            comments_within(c, out);
        }
    }
}

fn find_closure_bodies<'t>(n: Node<'t>, out: &mut Vec<Node<'t>>) {
    if matches!(n.kind(), "closure_expression" | "function_item") {
        if let Some(body) = n.child_by_field_name("body") {
            out.push(body);
        }
        return;
    }
    for c in ts::named_children(n) {
        find_closure_bodies(c, out);
    }
}

/// `Policy<'a>` -> `Policy`, `&mut Foo` -> `Foo`.
fn type_name(t: &str) -> String {
    let t = t.trim_start_matches('&').trim_start_matches("mut ").trim();
    let t = t.split('<').next().unwrap_or(t);
    t.rsplit("::").next().unwrap_or(t).trim().to_string()
}

/// ksync separates holding a lock from using it: a guard hands out a
/// `LockToken`, and fields marked `#[guarded_by(lock)]` are reached with
/// `field.get(token)`. C++ has no counterpart for these statements; a
/// `Guard<>` on the lock is all it needs.
fn is_lock_plumbing(stmt: &str) -> bool {
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^let\s+(?:mut\s+)?\w+\s*(?::[^=]*)?=.*(?:\.token(?:_mut)?\(\)|LockToken\s*(?:::\s*<[^>]*>\s*)?::\s*new\s*\(|\.fields(?:_mut)?\(\)|\.guard_(?:read|write)_lock\s*\()",
        )
        .unwrap()
    });
    static FIELD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^let\s+(?:mut\s+)?\w+\s*(?::[^=]*)?=\s*(?:unsafe\s*\{\s*)?[*&]?\s*(?:mut\s+)?self\.\w+\.get(?:_mut)?\(\s*&?\s*(?:mut\s+)?\w*token\s*\)\s*\}?\s*;$",
        )
        .unwrap()
    });
    TOKEN.is_match(stmt) || FIELD.is_match(stmt)
}

/// `foo()?; Ok(())` at the end of a function passes on foo's status, which
/// C++ writes `return Foo();`. The caller checks that the last unit is
/// `Ok(())`. An explicit `if r.is_err() { return r; }` before the `Ok(())`
/// stays as it is, since C++ spells it the same way.
fn fold_status_tail(units: &mut Vec<Unit>, lines: &[String]) {
    let n = units.len();
    if n < 2 {
        return;
    }
    let (prev, last) = (&units[n - 2], &units[n - 1]);
    let is_ok = last.kind == UnitKind::Return
        && last.features.ret == Some(Ret::Ok)
        && last.features.calls.is_empty();
    if !(is_ok
        && prev.kind == UnitKind::Stmt
        && prev.features.propagates
        && prev.features.errors.is_empty()
        && prev.depth == last.depth
        && prev.depth == 1
        && !(prev.start_line..=prev.end_line).any(|l| {
            lines.get(l - 1).is_some_and(|t| {
                is_err_var(t.trim_end_matches('{').trim().trim_start_matches("if ")).is_some()
            })
        }))
    {
        return;
    }
    let last = units.pop().unwrap();
    let prev = units.last_mut().unwrap();
    prev.kind = UnitKind::Return;
    prev.end_line = last.end_line;
    prev.features.propagates = false;
    prev.features.ret = Some(Ret::Status);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_result_types() {
        assert!(is_result_type("Result<(), Status>"));
        assert!(is_result_type("Result<u32, Status>"));
        assert!(is_result_type("zx::Result<u32>"));
        assert!(is_result_type("zx_status::Result"));
        assert!(is_result_type("core::result::Result<(), Status>"));
        assert!(is_result_type("Result"));
        assert!(!is_result_type("()"));
        assert!(!is_result_type("u32"));
        assert!(!is_result_type("Option<u32>"));
        assert!(!is_result_type("Status"));
    }

    #[test]
    fn classifies_tail_call_in_result_fn_as_status_return() {
        let src = "pub fn sys_test(handle: HandleValue) -> Result<(), Status> {\n    user_packet.copy_to_user(&packet)\n}\n";
        let fns = extract("test.rs", src);
        assert_eq!(fns.len(), 1);
        let ret_units: Vec<_> =
            fns[0].units.iter().filter(|u| u.kind == UnitKind::Return).collect();
        assert_eq!(ret_units.len(), 1);
        assert_eq!(ret_units[0].features.ret, Some(Ret::Status));
    }
}
