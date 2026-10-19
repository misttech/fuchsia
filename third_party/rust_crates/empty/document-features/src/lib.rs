// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stub for `document-features` to allow dependent crates to compile without
//! reading Cargo.toml at proc-macro expansion time.

extern crate proc_macro;

use proc_macro::{Literal, TokenStream, TokenTree};

#[proc_macro]
pub fn document_features(_tokens: TokenStream) -> TokenStream {
    TokenTree::from(Literal::string("")).into()
}
