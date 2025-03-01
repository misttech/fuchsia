// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_BLOBFS_ITERATOR_NODE_POPULATOR_H_
#define SRC_STORAGE_BLOBFS_ITERATOR_NODE_POPULATOR_H_

#include <lib/fit/function.h>
#include <stdint.h>
#include <zircon/types.h>

#include <vector>

#include <fbl/macros.h>

#include "src/storage/blobfs/allocator/base_allocator.h"
#include "src/storage/blobfs/allocator/extent_reserver.h"
#include "src/storage/blobfs/allocator/node_reserver.h"

namespace blobfs {

// A helper class which utilizes the visitor pattern to chain together a group of extents and nodes.
//
// Precondition:
//      nodes.size() >= NodeCountForExtents(extents.size())
class NodePopulator {
 public:
  NodePopulator(BaseAllocator* allocator, std::vector<ReservedExtent> extents,
                std::vector<ReservedNode> nodes);

  DISALLOW_COPY_ASSIGN_AND_MOVE(NodePopulator);

  // Returns the maximum number of nodes necessary to hold |extent_count| extents.
  static uint64_t NodeCountForExtents(uint64_t extent_count);

  enum class IterationCommand {
    Continue,
    Stop,
  };

  using OnNodeCallback = fit::function<void(uint32_t node_index)>;
  using OnExtentCallback = fit::function<IterationCommand(ReservedExtent& extent)>;

  // Utilizes the |allocator| to locate all nodes provided by |nodes|, and allocate each node the
  // appropriate |extent|.
  //
  // Along the way, this methods sets the following fields on the blob inode: |next_node|,
  // |extents|, |extent_count|. This method sets all fields on the container nodes.
  //
  // Before each extent is accessed, |on_extent| is invoked. This allows a caller to modify how
  // much of the extent is actually used. If IterationCommand::Stop is returned from |on_extent|,
  // then extent-filling exits early, and no additional extents are used. This ability to "stop
  // short" when using extents is useful when less storage is needed to persist a blob than
  // originally allocated. This is common when using compression.
  //
  // After all extents are accessed, |on_node| is invoked on all nodes which are actually used to
  // represent the blob. This may be smaller than the number of nodes passed in the ReservedNode
  // vector.
  zx_status_t Walk(OnNodeCallback on_node, OnExtentCallback on_extent);

 private:
  BaseAllocator* allocator_;
  std::vector<ReservedExtent> extents_;
  std::vector<ReservedNode> nodes_;
};

}  // namespace blobfs

#endif  // SRC_STORAGE_BLOBFS_ITERATOR_NODE_POPULATOR_H_
