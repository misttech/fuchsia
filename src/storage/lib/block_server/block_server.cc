// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/lib/block_server/block_server.h"

#include <lib/async/cpp/task.h>
#include <lib/driver/logging/cpp/logger.h>
#include <zircon/assert.h>

#include "src/storage/lib/block_server/block_server_c.h"

namespace block_server {

BlockServer::BlockServer(const PartitionInfo& info, Interface* interface)
    : interface_(interface),
      server_(block_server_new(
          &info, internal::Callbacks{
                     .context = this,
                     .start_thread =
                         [](void* context, const void* arg) {
                           reinterpret_cast<BlockServer*>(context)->interface_->StartThread(
                               Thread(arg));
                         },
                     .on_new_session =
                         [](void* context, const internal::Session* session) {
                           reinterpret_cast<BlockServer*>(context)->interface_->OnNewSession(
                               Session(session));
                         },
                     .on_requests =
                         [](void* context, Request* requests, uintptr_t request_count) {
                           reinterpret_cast<BlockServer*>(context)->interface_->OnRequests(
                               std::span<Request>(requests, request_count));
                         },
                     .log =
                         [](void* context, const char* msg, size_t len) {
                           reinterpret_cast<BlockServer*>(context)->interface_->Log(
                               std::string_view(msg, len));
                         },
                 })) {}

Session& Session::operator=(Session&& other) {
  if (this == &other)
    return *this;
  if (session_)
    block_server_session_release(session_);
  session_ = other.session_;
  other.session_ = nullptr;
  return *this;
}

Session::~Session() {
  if (session_) {
    block_server_session_release(session_);
  }
}

void Session::Run() { block_server_session_run(session_); }

BlockServer::BlockServer(BlockServer&& other)
    : interface_(other.interface_), server_(other.server_) {
  other.interface_ = nullptr;
  other.server_ = nullptr;
}

BlockServer::~BlockServer() {
  if (server_) {
    block_server_delete(server_);
  }
}

void BlockServer::Serve(fidl::ServerEnd<fuchsia_storage_block::Block> server_end) {
  block_server_serve(server_, server_end.TakeChannel().release());
}

void BlockServer::SendReply(RequestId request_id, zx::result<> result) const {
  block_server_send_reply(server_, request_id, result.status_value());
}

Request SplitRequest(Request& request, uint32_t block_offset, uint32_t block_size) {
  Request head = request;
  switch (request.operation.tag) {
    case Operation::Tag::Read:
    case Operation::Tag::Write:
      request.operation.read.vmo_offset += static_cast<uint64_t>(block_offset) * block_size;
      request.operation.read.options.inline_crypto.dun += block_offset;
      break;
    case Operation::Tag::Trim:
      break;
    case Operation::Tag::Flush:
    case Operation::Tag::CloseVmo:
      ZX_PANIC("Can't split Flush or CloseVmo operations");
  }
  head.operation.read.block_count = block_offset;
  request.operation.read.device_block_offset += block_offset;
  request.operation.read.block_count -= block_offset;
  return head;
}

zx_status_t CheckIoRange(const Request& request, uint64_t total_block_count) {
  uint64_t start;
  uint64_t length;
  switch (request.operation.tag) {
    case Operation::Tag::Read:
      start = request.operation.read.device_block_offset;
      length = request.operation.read.block_count;
      break;
    case Operation::Tag::Write:
      start = request.operation.write.device_block_offset;
      length = request.operation.write.block_count;
      break;
    case Operation::Tag::Trim:
      start = request.operation.trim.device_block_offset;
      length = request.operation.trim.block_count;
      break;
    case Operation::Tag::Flush:
    case Operation::Tag::CloseVmo:
      return ZX_OK;
  }
  if (length == 0 || length > total_block_count) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  if (start >= total_block_count || start > total_block_count - length) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  return ZX_OK;
}

namespace {

// Shuts down `dispatcher` and schedules it for asynchronous destruction. The shutdown handler
// registered with the dispatcher is responsible for destroying the dispatcher instance.
void ShutdownDestroyAsync(fdf::SynchronizedDispatcher dispatcher) {
  // Shutdown the dispatcher, after which the shutdown handler will be invoked asynchronously.
  dispatcher.ShutdownAsync();
  // Release the underlying dispatcher instance; the shutdown handler is responsible for destroying
  // the dispatcher instance.
  dispatcher.release();
}

}  // namespace

void DriverInterface::StartThread(Thread thread) {
  // Create a new dispatcher to run `thread` on.
  zx::result dispatcher =
      fdf::SynchronizedDispatcher::Create(fdf::SynchronizedDispatcher::Options::kAllowSyncCalls,
                                          "Block Server", OnDispatcherShutdown());
  if (dispatcher.is_error()) {
    FDF_LOGL(ERROR, logger(), "Failed to create dispatcher for block server thread: %s",
             dispatcher.status_string());
    return;
  }

  async_dispatcher_t* const async_dispatcher = dispatcher->async_dispatcher();
  const zx_status_t status = async::PostTask(
      async_dispatcher,
      [active_thread = std::move(thread), dispatcher = *std::move(dispatcher)]() mutable {
        {
          // *NOTE*: Separate scope ensures `thread` is destroyed before we shutdown `dispatcher`.
          Thread thread = std::move(active_thread);
          thread.Run();
        }
        ShutdownDestroyAsync(std::move(dispatcher));
      });

  // Make sure we destroy the dispatcher if we fail to post the task.
  if (status != ZX_OK) {
    FDF_LOGL(ERROR, logger(), "Failed to post task to run block server thread: %s",
             zx_status_get_string(status));
    ShutdownDestroyAsync(*std::move(dispatcher));
  }
}

void DriverInterface::OnNewSession(Session session) {
  // Create a new dispatcher to run `session` on.
  zx::result dispatcher = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "Block Server", OnDispatcherShutdown(),
      SessionSchedulerRole());
  if (dispatcher.is_error()) {
    FDF_LOGL(ERROR, logger(), "Failed to create dispatcher for block server session: %s",
             dispatcher.status_string());
    return;
  }

  async_dispatcher_t* const async_dispatcher = dispatcher->async_dispatcher();
  const zx_status_t status = async::PostTask(
      async_dispatcher,
      [active_session = std::move(session), dispatcher = *std::move(dispatcher)]() mutable {
        {
          // *NOTE*: Separate scope ensures `session` is destroyed before we shutdown `dispatcher`.
          Session session = std::move(active_session);
          session.Run();
        }
        ShutdownDestroyAsync(std::move(dispatcher));
      });

  // Make sure we destroy the dispatcher if we fail to post the task.
  if (status != ZX_OK) {
    FDF_LOGL(ERROR, logger(), "Failed to post task to run block server session: %s",
             zx_status_get_string(status));
    ShutdownDestroyAsync(*std::move(dispatcher));
  }
}

}  // namespace block_server
