// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use proc_macro::{Delimiter, TokenStream, TokenTree};

/// A procedural macro attribute that generates the C++ mangled symbol name for the
/// given string literal, and attaches it as `#[unsafe(export_name = "...")]` to the
/// target static variable.
///
/// Expects a single double-quoted string literal as the attribute argument, e.g.
/// `#[interned_string_export_name("hello")]`.
#[proc_macro_attribute]
pub fn interned_string_export_name(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut iter = attr.into_iter();

    let str_lit_tt = match iter.next() {
        Some(tt) => unwrap_token_tree(tt),
        _ => panic!("Expected string literal as attribute argument"),
    };
    let str_lit = match str_lit_tt {
        TokenTree::Literal(lit) => lit,
        _ => panic!("Expected string literal as attribute argument, got: {}", str_lit_tt),
    };

    let str_val = str_lit.to_string();
    if !str_val.starts_with('"') || !str_val.ends_with('"') {
        panic!("Attribute argument must be a double-quoted string literal");
    }
    let actual_str = &str_val[1..str_val.len() - 1];

    let cpp_symbol = mangle_cpp_interned_string(actual_str);

    let mut result = TokenStream::new();
    let export_name_attr = format!("#[unsafe(export_name = \"{}\")]", cpp_symbol);
    result.extend(export_name_attr.parse::<TokenStream>().unwrap());
    result.extend(item);
    result
}

// Recursively unwraps `TokenTree::Group`s with a `None` delimiter containing a single token.
//
// This is necessary because when `macro_rules!` macros pass arguments (like `$str_lit` or
// `$var_name`) to a procedural macro, the compiler wraps them in an invisible group to
// preserve span and parsing boundaries. Unwrapping them allows direct matching on the
// underlying `Literal` or `Ident` tokens.
fn unwrap_token_tree(mut tt: TokenTree) -> TokenTree {
    while let TokenTree::Group(group) = &tt {
        if group.delimiter() == Delimiter::None {
            let mut iter = group.stream().into_iter();
            if let Some(first) = iter.next() {
                if iter.next().is_none() {
                    tt = first;
                    continue;
                }
            }
        }
        break;
    }
    tt
}

// Generates the Itanium C++ ABI mangled name for the C++ template instantiation
// `fxt::internal::InternedStringStorage<chars...>::interned_string`.
//
// Under the Itanium C++ ABI:
// - `_ZN` starts a nested name sequence.
// - `3fxt` is namespace `fxt`.
// - `8internal` is namespace `internal`.
// - `21InternedStringStorage` is the class name.
// - `IJ` starts the template parameter pack.
// - Each character `c` of the string is mangled as an integral non-type template
//   parameter of type `char` (mangled type code `c`): `Lc` + [decimal value] + `E`.
// - `EE` closes the template parameters list.
// - `15interned_string` is the static member variable name.
// - `E` closes the nested name sequence.
//
// For example, the string `"foo"` is mangled to:
// `_ZN3fxt8internal21InternedStringStorageIJLc102ELc111ELc111EEE15interned_stringE`
fn mangle_cpp_interned_string(s: &str) -> String {
    let mut result = String::from("_ZN3fxt8internal21InternedStringStorageIJ");
    for c in s.chars() {
        result.push_str(&format!("Lc{}E", c as u32));
    }
    result.push_str("EE15interned_stringE");
    result
}
