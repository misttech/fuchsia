// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/blobfs/transaction.h"

#include <lib/fit/function.h>
#include <lib/fpromise/promise.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <utility>

#include "src/storage/lib/vfs/cpp/journal/journal.h"

namespace blobfs {

BlobTransaction::~BlobTransaction() {
  if (armed_ && die_on_mutation_failure_) {
    ZX_PANIC("BlobTransaction destroyed while armed with pending mutations!");
  }
}

void BlobTransaction::Commit(fs::Journal& journal, fpromise::promise<void, zx_status_t> data,
                             fit::callback<void()> callback) {
  Disarm();
  const bool die_on_failure = die_on_mutation_failure_;
  zx_status_t status = journal.CommitTransaction({
      .metadata_operations = operations_.TakeOperations(),
      .data_promise = std::move(data),
      .trim = std::move(trim_),
      // We reserve extents (by capturing in a lambda) until after the trim has completed.
      .commit_callback = [reserved_extents = std::move(reserved_extents_)] {},
      .complete_callback = std::move(callback),
      .error_callback =
          [die_on_failure](zx_status_t status) {
            if (die_on_failure) {
              ZX_PANIC("BlobTransaction asynchronous commit failed: %s",
                       zx_status_get_string(status));
            }
          },
  });
  if (status != ZX_OK) {
    if (die_on_failure) {
      ZX_PANIC("BlobTransaction synchronous commit failed: %s", zx_status_get_string(status));
    }
    FX_PLOGS(ERROR, status) << "BlobTransaction::Commit failed";
  }
}

}  // namespace blobfs
