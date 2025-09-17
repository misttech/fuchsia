// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_NULL_LOCK_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_NULL_LOCK_H_

#include <fbl/null_lock.h>
#include <kernel/lockdep.h>

using NullLock = fbl::NullLock;

LOCK_DEP_TRAITS(NullLock, lockdep::LockFlagsNone);
LOCK_DEP_POLICY(NullLock, lockdep::DefaultLockPolicy);

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_NULL_LOCK_H_
