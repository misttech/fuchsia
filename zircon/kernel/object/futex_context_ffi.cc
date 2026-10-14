// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <object/futex_context.h>

static_assert(sizeof(FutexContext) == kFutexContextStorageSize);
static_assert(alignof(FutexContext) == kFutexContextStorageAlign);

static_assert(static_cast<uint32_t>(FutexContext::OwnerAction::RELEASE) == 0);
static_assert(static_cast<uint32_t>(FutexContext::OwnerAction::ASSIGN_WOKEN) == 1);
static_assert(sizeof(FutexContext::OwnerAction) == sizeof(uint32_t));

static_assert(static_cast<uint32_t>(FutexContext::ProcessType::Regular) == 0);
static_assert(static_cast<uint32_t>(FutexContext::ProcessType::Shared) == 1);
static_assert(sizeof(FutexContext::ProcessType) == sizeof(uint32_t));
