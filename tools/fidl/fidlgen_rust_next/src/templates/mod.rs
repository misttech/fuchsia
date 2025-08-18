// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Templates generate a lot of code which have tendencies to trip lints.
#![expect(clippy::diverging_sub_expression, dead_code, unreachable_code)]

mod alias;
mod bits;
mod compat;
mod compound_identifier;
mod r#const;
mod constant;
mod context;
mod denylist;
mod doc_string;
mod r#enum;
mod filters;
mod library;
mod natural_type;
mod prim;
mod protocol;
mod reserved;
mod service;
mod r#struct;
mod table;
mod r#union;
mod wire_type;

use askama::Template;

use crate::config::Config;
use fidl_ir::*;

use self::alias::*;
use self::bits::*;
use self::compat::*;
use self::compound_identifier::*;
use self::r#const::*;
use self::constant::*;
use self::context::*;
use self::denylist::*;
use self::doc_string::*;
use self::r#enum::*;
use self::library::*;
use self::natural_type::*;
use self::prim::*;
use self::protocol::*;
use self::reserved::*;
use self::service::*;
use self::r#struct::*;
use self::table::*;
use self::r#union::*;
use self::wire_type::*;

pub fn render_library(library: &Library, config: &Config) -> Result<String, askama::Error> {
    let context = Context::new(library, config);

    LibraryTemplate::new(context).render()
}
