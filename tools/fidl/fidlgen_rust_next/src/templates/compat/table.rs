// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;

use crate::templates::{Context, Contextual, Denylist, compat_camel, compat_snake};
use fidl_ir::Table;
use fidlgen::rust::RustIdent as _;

use super::CompatTemplate;

#[derive(Template)]
#[template(path = "compat/table.askama")]
pub struct TableCompatTemplate<'a> {
    table: &'a Table,
    compat: &'a CompatTemplate<'a>,

    name: String,
    compat_name: String,
    denylist: Denylist,
}

impl Contextual for TableCompatTemplate<'_> {
    fn context(&self) -> &Context {
        self.compat.context()
    }
}

impl<'a> TableCompatTemplate<'a> {
    pub fn new(table: &'a Table, compat: &'a CompatTemplate<'a>) -> Self {
        Self {
            table,
            compat,

            name: table.name.decl_name().camel(),
            compat_name: compat_camel(table.name.decl_name()),
            denylist: compat.rust_or_rust_next_denylist(&table.name),
        }
    }
}
