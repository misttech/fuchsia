// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// These constants need to be kept in sync with:
// sdk/lib/magma_common/include/lib/magma/magma_common_defs.h

pub const MAGMA_QUERY_MAXIMUM_INFLIGHT_PARAMS: u64 = 5;
pub const MAX_INFLIGHT_MESSAGES: u64 = 1000;
pub const MAX_INFLIGHT_MEMORY_MB: u64 = 100;
pub const MAX_INFLIGHT_BYTES: u64 = MAX_INFLIGHT_MEMORY_MB * 1024 * 1024;

pub const MAGMA_STATUS_OK: i32 = 0;
pub const MAGMA_STATUS_INTERNAL_ERROR: i32 = -1;
pub const MAGMA_STATUS_INVALID_ARGS: i32 = -2;
pub const MAGMA_STATUS_ACCESS_DENIED: i32 = -3;
pub const MAGMA_STATUS_MEMORY_ERROR: i32 = -4;
pub const MAGMA_STATUS_CONTEXT_KILLED: i32 = -5;
pub const MAGMA_STATUS_CONNECTION_LOST: i32 = -6;
pub const MAGMA_STATUS_TIMED_OUT: i32 = -7;
pub const MAGMA_STATUS_UNIMPLEMENTED: i32 = -8;
// This error means that an object was not in the right state for an operation on it.
pub const MAGMA_STATUS_BAD_STATE: i32 = -9;
// Corresponds to fuchsia.sysmem2/Error.ConstraintsIntersectionEmpty
pub const MAGMA_STATUS_CONSTRAINTS_INTERSECTION_EMPTY: i32 = -10;
// Corresponds to fuchsia.sysmem2/Error.TooManyGroupChildCombinations
pub const MAGMA_STATUS_TOO_MANY_GROUP_CHILD_COMBINATIONS: i32 = -11;
