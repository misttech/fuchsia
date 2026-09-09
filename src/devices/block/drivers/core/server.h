// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_CORE_SERVER_H_
#define SRC_DEVICES_BLOCK_DRIVERS_CORE_SERVER_H_

#include <fidl/fuchsia.storage.block/cpp/wire.h>
#include <fuchsia/hardware/block/driver/cpp/banjo.h>
#include <lib/fzl/fifo.h>
#include <lib/sync/completion.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/vmo.h>
#include <stdio.h>
#include <stdlib.h>
#include <zircon/types.h>

#include <atomic>
#include <new>
#include <optional>
#include <span>
#include <utility>
#include <vector>

#include <ddktl/device.h>
#include <fbl/condition_variable.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/intrusive_wavl_tree.h>
#include <fbl/mutex.h>
#include <fbl/ref_ptr.h>

#include "iobuffer.h"
#include "message-group.h"
#include "message.h"
#include "src/storage/lib/block_protocol/block-fifo.h"

// Remaps the dev_offset of block requests based on an internal map.
class OffsetMap {
 public:
  // Creates an OffsetMap from one or more mappings.  `block_count` is the total size of the device,
  // used to bounds-check the mappings.
  static zx::result<std::unique_ptr<OffsetMap>> Create(
      std::span<const fuchsia_storage_block::wire::BlockOffsetMapping> mappings,
      uint64_t block_count);

  // Adjusts `request` by applying the map to dev_offset.
  // Returns false if the request would exceed the range known to OffsetMap.
  bool AdjustRequest(BlockFifoRequest& request) const;

  // Maps logical_offset to physical device offset and max remaining contiguous length (up to
  // length).
  // Returns zx::error(ZX_ERR_OUT_OF_RANGE) if logical_offset is not within any mapping.
  zx::result<std::pair<uint64_t, uint32_t>> Map(uint64_t logical_offset, uint32_t length) const;

 private:
  explicit OffsetMap(std::vector<fuchsia_storage_block::wire::BlockOffsetMapping> mappings);

  std::vector<fuchsia_storage_block::wire::BlockOffsetMapping> mappings_;
};

class Server : public fidl::WireServer<fuchsia_storage_block::Session> {
 public:
  // Creates a new Server without offset mappings (full device access).
  static zx::result<std::unique_ptr<Server>> Create(ddk::BlockProtocolClient* bp);

  // Creates a new Server with offset mappings. Fails with ZX_ERR_INVALID_ARGS if mappings is empty.
  static zx::result<std::unique_ptr<Server>> Create(
      ddk::BlockProtocolClient* bp,
      std::span<const fuchsia_storage_block::wire::BlockOffsetMapping> mappings);

  // This will block until all outstanding messages have been processed.
  ~Server() override;

  // Starts the Server using the current thread
  zx_status_t Serve() TA_EXCL(server_lock_);

  zx::result<zx::fifo> GetFifo();
  zx::result<vmoid_t> AttachVmo(zx::vmo vmo) TA_EXCL(server_lock_) TA_EXCL(server_lock_);
  void Close();

  void GetFifo(GetFifoCompleter::Sync& completer) override;
  void AttachVmo(AttachVmoRequestView request, AttachVmoCompleter::Sync& completer) override;
  void Close(CloseCompleter::Sync& completer) override;

  // Updates the total number of pending requests.
  void TxnEnd();

  // Wrapper around "SendResponse", as a convenience
  // for finishing both one-shot and group-based transactions.
  void FinishTransaction(zx_status_t status, reqid_t reqid, groupid_t group);

  // Send the given response to the client.
  void SendResponse(const BlockFifoResponse& response);

  // Initiates a shutdown of the server.  When this finishes, the server might still be running, but
  // it should terminate shortly.
  void Shutdown();

  // Returns true if the server is about to terminate.
  bool WillTerminate() const;

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(Server);
  Server(ddk::BlockProtocolClient* bp, block_info_t block_info, size_t block_op_size,
         std::unique_ptr<OffsetMap> offset_map);

  // Helper for processing a single message read from the FIFO.
  void ProcessRequest(BlockFifoRequest* request);
  zx_status_t ProcessReadWriteRequest(BlockFifoRequest* request);
  zx_status_t ProcessCloseVmoRequest(BlockFifoRequest* request);
  zx_status_t ProcessFlushRequest(BlockFifoRequest* request);
  zx_status_t ProcessTrimRequest(BlockFifoRequest* request);

  using CreateMessageFn = fit::function<zx::result<std::unique_ptr<Message>>(
      uint64_t dev_offset, uint32_t length, MessageCompleter completer)>;
  // Splits `request` based on the stored offset mappings (if set), and the max_transfer_blocks (if
  // set).  Validates the range of the request.
  zx::result<std::vector<std::pair<uint64_t, uint32_t>>> SplitRequest(
      const BlockFifoRequest& request,
      std::optional<uint32_t> max_transfer_blocks = std::nullopt) const;
  zx_status_t SubmitSplitRequest(BlockFifoRequest* request,
                                 const std::vector<std::pair<uint64_t, uint32_t>>& chunks,
                                 bool do_postflush, CreateMessageFn create_message_fn);

  zx_status_t IssueFlushCommand(BlockFifoRequest* request, MessageCompleter completer,
                                bool internal_cmd);

  zx_status_t Read(BlockFifoRequest* requests, size_t* count);

  zx::result<vmoid_t> FindVmoIdLocked() TA_REQ(server_lock_);

  // Sends the request embedded in the message down to the lower layers.
  void Enqueue(std::unique_ptr<Message> message) TA_EXCL(server_lock_);

  static zx::result<std::unique_ptr<Server>> Create(ddk::BlockProtocolClient* bp,
                                                    std::unique_ptr<OffsetMap> map);

  fzl::fifo<BlockFifoResponse, BlockFifoRequest> fifo_;
  fzl::fifo<BlockFifoRequest, BlockFifoResponse> fifo_peer_;
  block_info_t info_;
  std::unique_ptr<OffsetMap> offset_map_;
  ddk::BlockProtocolClient* bp_;
  size_t block_op_size_;

  // Used to wait for pending_count_ to drop to zero at shutdown time.
  fbl::ConditionVariable condition_;

  // The number of outstanding requests that have been sent down the stack.
  size_t pending_count_ TA_GUARDED(server_lock_);

  std::unique_ptr<MessageGroup> groups_[MAX_TXN_GROUP_COUNT];

  fbl::Mutex server_lock_;
  fbl::WAVLTree<vmoid_t, fbl::RefPtr<IoBuffer>> tree_ TA_GUARDED(server_lock_);
  vmoid_t last_id_ TA_GUARDED(server_lock_);
};

#endif  // SRC_DEVICES_BLOCK_DRIVERS_CORE_SERVER_H_
