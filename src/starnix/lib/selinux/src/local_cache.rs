// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::permission_check::PermissionCheckResult;
use crate::policy::{KernelAccessDecision, XpermsKind};
use crate::{KernelClass, KernelPermission, SecurityId};

/// A per-thread cache for permission checks. Currently empty.
#[derive(Debug, Default)]
pub struct PerThreadCache {}

impl PerThreadCache {
    /// Looks up a fd use decision in cache, or falls back to using `compute`.
    #[inline]
    pub fn lookup_fd_use<F>(
        &self,
        _source_sid: SecurityId,
        _target_sid: SecurityId,
        compute: F,
    ) -> PermissionCheckResult
    where
        F: FnOnce() -> PermissionCheckResult,
    {
        compute()
    }

    /// Looks up an xperms access decision in cache, or falls back to calling `compute`.
    #[inline]
    pub(crate) fn check_xperm<F>(
        &self,
        _kind: XpermsKind,
        _source_sid: SecurityId,
        _target_sid: SecurityId,
        _permission: KernelPermission,
        _xperm: u16,
        compute: F,
    ) -> PermissionCheckResult
    where
        F: FnOnce() -> PermissionCheckResult,
    {
        compute()
    }

    /// Looks up an access decision in cache, or falls back to calling `compute`. This caches the
    /// whole access vector instead of individual permissions so that multiple checks for different
    /// permissions on the same (source, target, class) triple can make use of the cache.
    #[inline]
    pub(crate) fn lookup_access_decision<F>(
        &self,
        _source_sid: SecurityId,
        _target_sid: SecurityId,
        _class: KernelClass,
        compute: F,
    ) -> KernelAccessDecision
    where
        F: FnOnce() -> KernelAccessDecision,
    {
        compute()
    }
}
