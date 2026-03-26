// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;

use crate::context::Context;

#[derive(Template)]
#[template(path = "library.askama")]
pub struct LibraryTemplate<'a> {
    context: &'a Context,
}

impl<'a> LibraryTemplate<'a> {
    pub fn new(context: &'a Context) -> Self {
        Self { context }
    }
}
