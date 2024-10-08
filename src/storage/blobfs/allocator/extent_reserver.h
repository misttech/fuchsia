// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_BLOBFS_ALLOCATOR_EXTENT_RESERVER_H_
#define SRC_STORAGE_BLOBFS_ALLOCATOR_EXTENT_RESERVER_H_

#include <stdint.h>
#include <zircon/compiler.h>

#include <mutex>

#include <bitmap/rle-bitmap.h>
#include <fbl/macros.h>

#include "src/storage/blobfs/format.h"

namespace blobfs {

class ReservedExtent;

// Allows extents to be reserved and unreserved. The purpose of reservation is to allow allocation
// of extents to occur without yet allocating structures which could be written out to durable
// storage.
//
// These extents may be observed by derived classes of ExtentReserver
class ExtentReserver {
 public:
  ReservedExtent Reserve(const Extent& extent);

  // Unreserves space for blocks in memory. Does not update disk.
  void Unreserve(const Extent& extent);

  // Returns the total number of reserved blocks.
  uint64_t ReservedBlockCount() const;

 protected:
  std::mutex& mutex() const __TA_RETURN_CAPABILITY(mutex_) { return mutex_; }

  // Reserves space for blocks in memory. Does not update disk.
  //
  // |extent.Length()| must be > 0.
  ReservedExtent ReserveLocked(const Extent& extent) __TA_REQUIRES(mutex());

  // Returns an iterator to the underlying reserved blocks.
  //
  // This iterator becomes invalid on the next call to either |ReserveExtent| or |UnreserveExtent|.
  bitmap::RleBitmap::const_iterator ReservedBlocksCbegin() const __TA_REQUIRES(mutex()) {
    return reserved_blocks_.cbegin();
  }

  bitmap::RleBitmap::const_iterator ReservedBlocksCend() const __TA_REQUIRES(mutex()) {
    return reserved_blocks_.end();
  }

 private:
  mutable std::mutex mutex_;
  bitmap::RleBitmap reserved_blocks_ __TA_GUARDED(mutex_);
};

// Wraps an extent reservation in RAII to hold the reservation active, and release it when it goes
// out of scope.
class ReservedExtent {
 public:
  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(ReservedExtent);

  ReservedExtent(ReservedExtent&& o) noexcept;
  ReservedExtent& operator=(ReservedExtent&& o) noexcept;
  ~ReservedExtent();

  // Access the underlying extent which has been reserved.
  //
  // Unsafe to call if this extent has not actually been reserved.
  const Extent& extent() const;

  // Split a reserved extent from [start, start + length) such that:
  // This retains [start, start + block_split),
  //  and returns [start + block_split, start + length)
  //
  // This function requires that |block_split| < |extent.block_count|.
  ReservedExtent SplitAt(uint64_t block_split);

  // Releases the underlying reservation, unreserving the extent and preventing continued access
  // to |extent()|.
  void Reset();

 private:
  friend ExtentReserver;

  // Creates a reserved extent.
  //
  // |extent.Length()| must be > 0.
  // The caller is responsible for actually reserving an extent.
  ReservedExtent(ExtentReserver* reserver, Extent extent) : reserver_(reserver), extent_(extent) {}

  // Update internal state such that future calls to |Reserved| return false.
  void Release();

  // Identify if the underlying extent is reserved, and able to be accessed.
  bool Reserved() const;

  ExtentReserver* reserver_;
  Extent extent_;
};

inline ReservedExtent ExtentReserver::Reserve(const Extent& extent) {
  std::scoped_lock lock(mutex());
  return ReserveLocked(extent);
}

}  // namespace blobfs

#endif  // SRC_STORAGE_BLOBFS_ALLOCATOR_EXTENT_RESERVER_H_
