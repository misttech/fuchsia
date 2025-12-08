// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::types::common::*;
use crate::types::environment::EnvironmentRef;
pub use cm_types::{Name, OnTerminate, StartupMode, Url};
use json_spanned_value::Spanned;
use reference_doc::ReferenceDoc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Example:
///
/// ```json5
/// children: [
///     {
///         name: "logger",
///         url: "fuchsia-pkg://fuchsia.com/logger#logger.cm",
///     },
///     {
///         name: "pkg_cache",
///         url: "fuchsia-pkg://fuchsia.com/pkg_cache#meta/pkg_cache.cm",
///         startup: "eager",
///     },
///     {
///         name: "child",
///         url: "#meta/child.cm",
///     }
/// ],
/// ```
///
/// [component-url]: /docs/reference/components/url.md
/// [doc-eager]: /docs/development/components/connect.md#eager
/// [doc-reboot-on-terminate]: /docs/development/components/connect.md#reboot-on-terminate
#[derive(ReferenceDoc, Deserialize, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list", top_level_doc_after_fields)]
pub struct Child {
    /// The name of the child component instance, which is a string of one
    /// or more of the following characters: `a-z`, `0-9`, `_`, `.`, `-`. The name
    /// identifies this component when used in a [reference](#references).
    pub name: Name,

    /// The [component URL][component-url] for the child component instance.
    pub url: Url,

    /// The component instance's startup mode. One of:
    /// - `lazy` _(default)_: Start the component instance only if another
    ///     component instance binds to it.
    /// - [`eager`][doc-eager]: Start the component instance as soon as its parent
    ///     starts.
    #[serde(default)]
    #[serde(skip_serializing_if = "StartupMode::is_lazy")]
    pub startup: StartupMode,

    /// Determines the fault recovery policy to apply if this component terminates.
    /// - `none` _(default)_: Do nothing.
    /// - `reboot`: Gracefully reboot the system if the component terminates for
    ///     any reason other than graceful exit. This is a special feature for use only by a narrow
    ///     set of components; see [Termination policies][doc-reboot-on-terminate] for more
    ///     information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_terminate: Option<OnTerminate>,

    /// If present, the name of the environment to be assigned to the child component instance, one
    /// of [`environments`](#environments). If omitted, the child will inherit the same environment
    /// assigned to this component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentRef>,
}

#[derive(Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ParsedChild {
    pub name: Spanned<Name>,
    pub url: Spanned<Url>,
    pub startup: Option<Spanned<StartupMode>>,
    pub on_terminate: Option<Spanned<OnTerminate>>,
    pub environment: Option<Spanned<EnvironmentRef>>,
}

#[derive(Debug, Clone)]
pub struct ContextChild {
    pub name: ContextSpanned<Name>,
    pub url: ContextSpanned<Url>,
    pub startup: Option<ContextSpanned<StartupMode>>,
    pub on_terminate: Option<ContextSpanned<OnTerminate>>,
    pub environment: Option<ContextSpanned<EnvironmentRef>>,
}

impl PartialEq for ContextChild {
    fn eq(&self, other: &Self) -> bool {
        self.name.value == other.name.value
    }
}
impl Eq for ContextChild {}

impl Hydrate for ParsedChild {
    type Output = ContextChild;

    fn hydrate(self, file: &Arc<PathBuf>, buffer: &String) -> Self::Output {
        ContextChild {
            name: hydrate_simple(self.name, file, buffer),
            url: hydrate_simple(self.url, file, buffer),
            startup: hydrate_opt_simple(self.startup, file, buffer),
            on_terminate: hydrate_opt_simple(self.on_terminate, file, buffer),
            environment: hydrate_opt_simple(self.environment, file, buffer),
        }
    }
}
