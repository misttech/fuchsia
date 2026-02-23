// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_BLOCK_SERVER_BLOCK_SERVER_H_
#define SRC_STORAGE_LIB_BLOCK_SERVER_BLOCK_SERVER_H_

#include <fidl/fuchsia.storage.block/cpp/wire.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/zx/result.h>

#include <atomic>
#include <memory>
#include <span>

#include "src/storage/lib/block_server/block_server_c.h"

namespace block_server {

using RequestId = internal::RequestId;
using TraceFlowId = uint64_t;
using Operation = internal::Operation;
using Request = internal::Request;
using PartitionInfo = internal::PartitionInfo;

// Represents a session.  New sessions appear via `OnNewSession`.
class Session {
 public:
  Session(Session&& other) : session_(other.session_) { other.session_ = nullptr; }
  Session& operator=(Session&& other);

  // NOTE: The `BlockServer` destructor will be unblocked before this returns, so take
  // care with any code that runs *after* this returns.
  ~Session();

  // Runs the session (blocking).
  void Run();

 private:
  friend class BlockServer;

  explicit Session(const internal::Session* session) : session_(session) {}

  const internal::Session* session_;
};

// Represents the thread that services all FIDL requests.  This appears via `StartThread`.
class Thread {
 public:
  Thread(Thread&& other) : arg_(other.arg_) { other.arg_ = nullptr; }
  Thread& operator=(Thread&& other) = delete;

  // NOTE: The `BlockServer` destructor will be unblocked before this returns, so take
  // care with any code that runs *after* this returns.
  ~Thread() {
    if (arg_)
      internal::block_server_thread_delete(arg_);
  }

  // Runs the thread (blocking).
  void Run() { internal::block_server_thread(arg_); }

 private:
  friend class BlockServer;

  explicit Thread(const void* arg) : arg_(arg) {}
  const void* arg_;
};

// The interface which all block servers must implement.
class Interface {
 public:
  virtual ~Interface() {}
  // Called to start the thread that processes all FIDL requests.  The implementation must start a
  // thread and then call `Thread::Run`.
  virtual void StartThread(Thread) = 0;

  // Called when a new session is started.  The implementation must start a thread and then call
  // `Session::Run`.  The callback takes ownership of `Session`.
  virtual void OnNewSession(Session) = 0;

  // Called when new requests arrive.  It is OK for this method to block so as to cause push back on
  // the fifo (which is recommended for effective flow control).  Each request must eventually be
  // completed by calling BlockServer::SendReply; failure to do so will result in resource leaks
  // until the block server terminates.
  virtual void OnRequests(std::span<Request>) = 0;

  // Called for log messages.
  virtual void Log(std::string_view msg) const {}
};

// Helper class for drivers to use when implementing the block server interface. Simplifies
// integration with the Fuchsia driver framework for logging and async dispatchers.
// TODO(https://fxbug.dev/42085539): Each session runs in a blocking manner on a dedicated
// dispatcher, however the driver framework uses a fixed-size thread pool for running these tasks.
// Once this limit is hit (currently 10), new sessions will be blocked from running until existing
// ones are closed.
class DriverInterface : public Interface {
 public:
  DriverInterface() = default;

  // The logger to use for log messages. By default uses the global logger instance.
  virtual fdf::Logger& logger() const {
    ZX_ASSERT(fdf::Logger::HasGlobalInstance());
    return *fdf::Logger::GlobalInstance();
  }

  // The scheduler role name to use for session worker threads.
  virtual std::string_view SessionSchedulerRole() const { return {}; }

 protected:
  using ShutdownHandler = fdf::Dispatcher::ShutdownHandler;

  // Callback registered when creating dispatchers. The callback will run asynchronously after the
  // dispatcher has been shutdown and is required to destroy the dispatcher instance with
  // `fdf_dispatcher_destroy`.
  virtual ShutdownHandler OnDispatcherShutdown() const {
    return [](fdf_dispatcher_t* dispatcher) { fdf_dispatcher_destroy(dispatcher); };
  }

 private:
  void StartThread(Thread thread) final;
  void OnNewSession(Session session) final;
  void Log(std::string_view msg) const final {
    FDF_LOGL(INFO, logger(), "%.*s", static_cast<int>(msg.size()), msg.data());
  }
};

class BlockServer {
 public:
  // Constructs a new server.
  BlockServer(const PartitionInfo&, Interface*);
  BlockServer(const BlockServer&) = delete;
  BlockServer& operator=(const BlockServer&) = delete;
  BlockServer(BlockServer&&);
  BlockServer& operator=(BlockServer&&) = delete;

  // Destroys the server.  This will trigger termination and then block until:
  //
  //   1. `Thread::Run()` returns.
  //   2. All `Session` objects have been destroyed i.e. `Session::Run` has returned
  //      *and* `Session` has been destroyed.
  //
  // Once this returns, there will be no subsequent calls via `Interface`.
  ~BlockServer();

  // Destroys the server asynchronously and calls `callback` when complete.
  template <typename Callback>
  void DestroyAsync(Callback callback) && {
    if (server_) {
      auto owned_callback = std::make_unique<Callback>(std::move(callback));
      internal::BlockServer* server = server_;
      server_ = nullptr;
      block_server_delete_async(
          server,
          [](void* arg) {
            auto owned_callback = std::unique_ptr<Callback>(reinterpret_cast<Callback*>(arg));
            (*owned_callback)();
          },
          owned_callback.release());
    } else {
      callback();
    }
  }

  // Serves a new connection.  The FIDL handling is multiplexed onto a single per-server thread.
  void Serve(fidl::ServerEnd<fuchsia_storage_block::Block>);

  void SendReply(RequestId, zx::result<>) const;

 private:
  Interface* interface_ = nullptr;
  internal::BlockServer* server_ = nullptr;
};

// Splits the request at `block_offset` returning the head and leaving the tail in `request`.
Request SplitRequest(Request& request, uint32_t block_offset, uint32_t block_size);

zx_status_t CheckIoRange(const Request& request, uint64_t total_block_count);

}  // namespace block_server

#endif  // SRC_STORAGE_LIB_BLOCK_SERVER_BLOCK_SERVER_H_
