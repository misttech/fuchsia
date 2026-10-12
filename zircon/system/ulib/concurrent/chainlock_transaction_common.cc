// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/concurrent/chainlock_transaction_common.h>

extern "C" std::atomic<uint64_t> concurrent_chainlock_transaction_token_generator{
    concurrent::ChainLockTransactionCommon::kInitialTokenValue};

static_assert(std::atomic<uint64_t>::is_always_lock_free);
static_assert(sizeof(concurrent_chainlock_transaction_token_generator) == sizeof(uint64_t));
static_assert(alignof(decltype(concurrent_chainlock_transaction_token_generator)) ==
              alignof(uint64_t));
