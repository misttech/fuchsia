// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::templates::is_reserved;
use fidl_ir::Ident;

pub fn escape_compat(mut name: String, ident: &Ident) -> String {
    if is_compat_reserved(ident.non_canonical()) {
        name.push('_');
    }
    name
}

pub fn is_compat_reserved(name: &str) -> bool {
    is_reserved(name) || COMPAT_RESERVED_SUFFIX_LIST.iter().any(|suffix| name.ends_with(suffix))
}

const COMPAT_RESERVED_SUFFIX_LIST: &[&str] =
    &["Impl", "Marker", "Proxy", "ProxyProtocol", "ControlHandle", "Responder", "Server"];
