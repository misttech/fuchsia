// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::types::environment::EnvironmentRef;
pub use cm_types::{AllowedOffers, Durability, Name};
use reference_doc::ReferenceDoc;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug, PartialEq, ReferenceDoc, Serialize)]
#[serde(deny_unknown_fields)]
#[reference_doc(fields_as = "list", top_level_doc_after_fields)]
/// Example:
///
/// ```json5
/// collections: [
///     {
///         name: "tests",
///         durability: "transient",
///     },
/// ],
/// ```
pub struct Collection {
    /// The name of the component collection, which is a string of one or
    /// more of the following characters: `a-z`, `0-9`, `_`, `.`, `-`. The name
    /// identifies this collection when used in a [reference](#references).
    pub name: Name,

    /// The duration of child component instances in the collection.
    /// - `transient`: The instance exists until its parent is stopped or it is
    ///     explicitly destroyed.
    /// - `single_run`: The instance is started when it is created, and destroyed
    ///     when it is stopped.
    pub durability: Durability,

    /// If present, the environment that will be
    /// assigned to instances in this collection, one of
    /// [`environments`](#environments). If omitted, instances in this collection
    /// will inherit the same environment assigned to this component.
    pub environment: Option<EnvironmentRef>,

    /// Constraints on the dynamic offers that target the components in this collection.
    /// Dynamic offers are specified when calling `fuchsia.component.Realm/CreateChild`.
    /// - `static_only`: Only those specified in this `.cml` file. No dynamic offers.
    ///     This is the default.
    /// - `static_and_dynamic`: Both static offers and those specified at runtime
    ///     with `CreateChild` are allowed.
    pub allowed_offers: Option<AllowedOffers>,

    /// Allow child names up to 1024 characters long instead of the usual 255 character limit.
    /// Default is false.
    pub allow_long_names: Option<bool>,

    /// If set to `true`, the data in isolated storage used by dynamic child instances and
    /// their descendants will persist after the instances are destroyed. A new child instance
    /// created with the same name will share the same storage path as the previous instance.
    pub persistent_storage: Option<bool>,
}
