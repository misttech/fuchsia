// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ident_ext::IdentExt;
use fidl_ir::Ident;

use super::reserved::escape;

pub fn escape_camel(ident: &Ident) -> String {
    escape(ident.camel())
}

pub fn escape_snake(ident: &Ident) -> String {
    escape(ident.snake())
}

pub fn escape_screaming_snake(ident: &Ident) -> String {
    escape(ident.screaming_snake())
}

pub fn camel(ident: &Ident, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(escape_camel(ident))
}

pub fn snake(ident: &Ident, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(escape_snake(ident))
}

pub fn screaming_snake(ident: &Ident, _: &dyn askama::Values) -> askama::Result<String> {
    Ok(escape_screaming_snake(ident))
}
