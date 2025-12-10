// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashSet;
use std::sync::LazyLock;

use fidl_ir::Ident;

use crate::SplitIdent;

pub trait RustIdent: SplitIdent {
    fn camel(&self) -> String {
        let mut result = String::new();

        for piece in self.split() {
            let mut chars = piece.chars();
            result.push(chars.next().unwrap().to_ascii_uppercase());
            result.extend(chars.map(|c| c.to_ascii_lowercase()));
        }

        if is_rust_keyword(&result) {
            result.push('_');
        }

        result
    }

    fn snake(&self) -> String {
        let mut result = String::new();

        for piece in self.split() {
            if !result.is_empty() {
                result.push('_');
            }
            result.extend(piece.chars().map(|c| c.to_ascii_lowercase()));
        }

        if is_rust_keyword(&result) {
            result.push('_');
        }

        result
    }

    fn screaming_snake(&self) -> String {
        let mut result = String::new();

        for piece in self.split() {
            if !result.is_empty() {
                result.push('_');
            }
            result.extend(piece.chars().map(|c| c.to_ascii_uppercase()));
        }

        if is_rust_keyword(&result) {
            result.push('_');
        }

        result
    }
}

impl RustIdent for Ident {}

pub fn is_rust_keyword(name: &str) -> bool {
    RUST_KEYWORDS.contains(name)
}

static RUST_KEYWORDS: LazyLock<HashSet<String>> =
    LazyLock::new(|| RUST_KEYWORDS_LIST.iter().map(|k| k.to_string()).collect());

const RUST_KEYWORDS_LIST: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "become",
    "box",
    "break",
    "const",
    "continue",
    "crate",
    "do",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "final",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "macro",
    "macro_rules",
    "match",
    "mod",
    "move",
    "mut",
    "override",
    "pub",
    "priv",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "unsafe",
    "unsized",
    "use",
    "virtual",
    "where",
    "while",
    "yield",
];
