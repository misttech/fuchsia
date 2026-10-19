// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Starnix kernel object class and permission definitions, policy-to-kernel index, and access
//! vector cache (AVC).

mod avc;
mod index;
mod permissions;

pub use avc::{
    AccessCacheStorage, AccessQueryArgs, CacheStats, ConcurrentAccessCache, DEFAULT_SHARED_SIZE,
    PerThreadCache, QueryCacheCapacity,
};
pub(crate) use avc::{AccessVectorCache, Query};
pub(crate) use index::PolicyIndex;
pub use permissions::*;

use crate::policy::{AccessDecision, AccessVector, ClassId, XpermsBitmap};
use std::num::NonZeroU32;

/// Identifies a specific class by its policy-defined Id, or as a kernel object class enum Id.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum ObjectClass {
    /// Refers to a well-known SELinux [`KernelClass`] (e.g. "process", "file", "capability").
    Kernel(KernelClass),
    /// Refers to a policy-defined class by its policy-defined numeric [`ClassId`]. This is most
    /// commonly used when handling queries from userspace, which refer to classes by ID.
    ClassId(ClassId),
}

impl From<ClassId> for ObjectClass {
    fn from(id: ClassId) -> Self {
        Self::ClassId(id)
    }
}

impl<T: Into<KernelClass>> From<T> for ObjectClass {
    fn from(class: T) -> Self {
        Self::Kernel(class.into())
    }
}

/// Access decision translated into kernel permission bit positions for a [`KernelClass`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelAccessDecision {
    pub allow: AccessVector,
    pub audit: AccessVector,
    pub flags: u32,
    pub todo_bug: Option<NonZeroU32>,
}

/// Extended permission access decision as seen from the kernel.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct KernelXpermsAccessDecision {
    /// Set of xperms that are allowed.
    pub allow: XpermsBitmap,
    /// Set of xperms that should be audited (as allowed or denials depending on `allow`).
    pub audit: XpermsBitmap,
    /// Whether the domain is permissive.
    pub permissive: bool,
    /// Whether the entry has an associated todo.
    pub has_todo: bool,
}

/// Owner of policy information that can translate [`KernelPermission`] values into
/// [`AccessVector`] values that are consistent with the owned policy.
pub(crate) trait AccessVectorComputer {
    /// Translates the given [`AccessDecision`] to a [`KernelAccessDecision`].
    ///
    /// The loaded policy's "handle unknown" configuration determines how `permissions`
    /// entries not explicitly defined by the policy are handled. Allow-unknown will
    /// result in unknown `permissions` being allowed, while they are denied (and audited)
    /// if the policy uses deny-unknown.
    fn access_decision_to_kernel_access_decision(
        &self,
        class: KernelClass,
        access_decision: AccessDecision,
    ) -> KernelAccessDecision;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_class_permissions() {
        let test_class_id = ClassId::for_test(20);
        assert_eq!(ObjectClass::ClassId(test_class_id), test_class_id.into());
        for variant in ProcessPermission::PERMISSIONS {
            assert_eq!(KernelClass::Process, variant.class());
            assert_eq!("process", variant.class().name());
            assert_eq!(ObjectClass::Kernel(KernelClass::Process), variant.class().into());
        }
    }

    #[test]
    fn kernel_class_from_name() {
        assert_eq!(KernelClass::from_name("process"), Some(KernelClass::Process));
        assert_eq!(KernelClass::from_name("file"), Some(KernelClass::File));
        assert_eq!(KernelClass::from_name("socket"), Some(KernelClass::Socket));
        assert_eq!(KernelClass::from_name("non_existent_class"), None);
    }
}
