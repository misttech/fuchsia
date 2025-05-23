// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_TEE_DRIVERS_OPTEE_OPTEE_CLIENT_H_
#define SRC_DEVICES_TEE_DRIVERS_OPTEE_OPTEE_CLIENT_H_

#include <fidl/fuchsia.hardware.rpmb/cpp/wire.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <fidl/fuchsia.tee.manager/cpp/wire.h>
#include <fidl/fuchsia.tee/cpp/wire.h>
#include <lib/zx/channel.h>

#include <atomic>
#include <filesystem>
#include <memory>
#include <optional>
#include <string_view>
#include <unordered_map>
#include <unordered_set>

#include <fbl/intrusive_double_list.h>

#include "optee-controller.h"

namespace optee {

// The Optee driver allows for simultaneous access from different processes. The OpteeClient object
// is a distinct device instance for each client connection. This allows for per-instance state to
// be managed together. For example, if a client closes the device, OpteeClient can free all of the
// allocated shared memory buffers and sessions that were created by that client without interfering
// with other active clients.

class OpteeClient final : public fidl::WireServer<fuchsia_tee::Application> {
 public:
  OpteeClient(OpteeControllerBase* controller,
              fidl::ClientEnd<fuchsia_tee_manager::Provider> provider, Uuid application_uuid)
      : controller_(controller), application_uuid_(application_uuid) {
    if (provider)
      provider_.Bind(std::move(provider));
  }

  ~OpteeClient() override;

  OpteeClient(const OpteeClient&) = delete;
  OpteeClient& operator=(const OpteeClient&) = delete;

  // `fuchsia.tee.Application` FIDL Handlers
  void OpenSession2(OpenSession2RequestView request,
                    OpenSession2Completer::Sync& completer) override;
  void InvokeCommand(InvokeCommandRequestView request,
                     InvokeCommandCompleter::Sync& completer) override;
  void CloseSession(CloseSessionRequestView request,
                    CloseSessionCompleter::Sync& completer) override;

 private:
  using SharedMemoryList = fbl::DoublyLinkedList<std::unique_ptr<SharedMemory>>;

  static constexpr zx::duration kSmcCallDurationThreshold = zx::msec(10);

  zx_status_t CloseSession(uint32_t session_id);

  // Attempts to allocate a block of SharedMemory from a designated memory pool.
  //
  // On success:
  //  * Tracks the allocated memory block in the allocated_shared_memory_ list.
  //  * Gives the physical address of the memory block in out_phys_addr
  //  * Gives an identifier for the memory block in out_mem_id. This identifier will later be
  //    used to free the memory block.
  //
  // On failure:
  //  * Sets the physical address of the memory block to 0.
  //  * Sets the identifier of the memory block to 0.
  template <typename SharedMemoryPoolTraits>
  zx_status_t AllocateSharedMemory(size_t size,
                                   SharedMemoryPool<SharedMemoryPoolTraits>* memory_pool,
                                   zx_paddr_t* out_phys_addr, uint64_t* out_mem_id);

  // Frees a block of SharedMemory that was previously allocated by the driver.
  //
  // Parameters:
  //  * mem_id:   The identifier for the memory block to free, given at allocation time.
  //
  // Returns:
  //  * ZX_OK:             Successfully freed the memory.
  //  * ZX_ERR_NOT_FOUND:  Could not find a block corresponding to the identifier given.
  zx_status_t FreeSharedMemory(uint64_t mem_id);

  // Attempts to find a previously allocated block of memory.
  //
  // Returns:
  //  * If the block was found, an iterator object pointing to the SharedMemory block.
  //  * Otherwise, an iterator object pointing to the end of allocated_shared_memory_.
  SharedMemoryList::iterator FindSharedMemory(uint64_t mem_id);

  // Attempts to get a slice of `SharedMemory` representing an OP-TEE memory reference.
  //
  // Parameters:
  //  * mem_iter:   The `SharedMemoryList::iterator` object pointing to the `SharedMemory`.
  //                This may point to the end of `allocated_shared_memory_`.
  //  * base_paddr: The starting base physical address of the slice.
  //  * size:       The size of the slice.
  //
  // Returns:
  //  * If `mem_iter` is valid and the slice bounds are valid, an initialized `std::optional` with
  //    the `SharedMemoryView`.
  //  * Otherwise, an uninitialized `std::optional`.
  static std::optional<SharedMemoryView> GetMemoryReference(SharedMemoryList::iterator mem_iter,
                                                            zx_paddr_t base_paddr, size_t size);

  // Requests the root storage channel from the `Provider` and caches it in `root_storage_`.
  //
  // Subsequent calls to the function will return the cached channel.
  //
  // Returns:
  //  * ZX_OK:                The operation was successful.
  //  * ZX_ERR_UNAVAILABLE:   The current client does not have access to a `Provider`.
  //  * `zx_status_t` codes from `zx::channel::create` or requesting the `Provider` over
  //    FIDL.
  zx::result<fidl::UnownedClientEnd<fuchsia_io::Directory>> GetRootStorage();

  // Requests a connection to the storage directory pointed to by the path.
  //
  // Parameters:
  //  * path:                 The path of the directory, relative to the root storage directory.
  zx::result<fidl::ClientEnd<fuchsia_io::Directory>> GetStorageDirectory(
      const std::filesystem::path& path);

  // Creates a new storage directory pointed to by the path and returns a connection to it.
  // Does not fail if the directory already exists.
  //
  // Parameters:
  //  * path:                 The path of the directory, relative to the root storage directory.
  zx::result<fidl::ClientEnd<fuchsia_io::Directory>> CreateStorageDirectory(
      const std::filesystem::path& path);

  // Inits the Rpmb client from `OpteeController` and caches it in `rpmb_client_`.
  //
  // Returns:
  //  * ZX_OK:                The operation was successful.
  //  * ZX_ERR_UNAVAILABLE:   `OpteeController` does not have access to a Rpmb.
  //  * `zx_status_t` codes from `zx::channel::create`
  zx_status_t InitRpmbClient();

  // Tracks a new file system object associated with the current client.
  //
  // This occurs when the trusted world creates or opens a file system object.
  //
  // Parameters:
  //  * file:  A client end to the `fuchsia.io.File` file system object.
  //
  // Returns:
  //  * The identifier for the trusted world to refer to the object.
  [[nodiscard]] uint64_t TrackFileSystemObject(fidl::ClientEnd<fuchsia_io::File> file);

  // Gets the channel to the file system object associated with the given identifier.
  //
  // Parameters:
  //  * identifier: The identifier to find the file system object by.
  //
  // Returns:
  //  * A `std::optional` containing an unowned fuchsia.io.File if it was found.
  std::optional<fidl::UnownedClientEnd<fuchsia_io::File>> GetFileSystemObject(uint64_t identifier);

  // Untracks a file system object associated with the current client.
  //
  // This occurs when the trusted world closes a previously open file system object.
  //
  // Parameters:
  //  * identifier:  The identifier to refer to the object.
  //
  // Returns:
  //  * Whether a file system object associated with the identifier was untracked.
  bool UntrackFileSystemObject(uint64_t identifier);

  //
  // OP-TEE RPC Function Handlers
  //
  // The section below outlines the functions that are used to parse and fulfill RPC commands from
  // the OP-TEE secure world.
  //
  // There are two main "types" of functions defined and can be identified by their naming
  // convention:
  //  * "HandleRpc" functions handle the first layer of commands. These are basic, fundamental
  //    commands used for critical tasks like setting up shared memory, notifying the normal world
  //    of interrupts, and accessing the second layer of commands.
  //  * "HandleRpcCommand" functions handle the second layer of commands. These are more advanced
  //    commands, like loading trusted applications and accessing the file system. These make up
  //    the bulk of RPC commands once a session is open.
  //      * HandleRpcCommand is actually a specific command in the first layer that can be invoked
  //        once initial shared memory is set up for the command message.
  //
  // Because these RPCs are the primary channel through which the normal and secure worlds mediate
  // shared resources, it is important that handlers in the normal world are resilient to errors
  // from the trusted world. While we don't expect that the trusted world is actively malicious in
  // any way, we do want handlers to be cautious against buggy or unexpected behaviors, as we do
  // not want errors propagating into the normal world (especially with resources like memory).

  // Identifies and dispatches the first layer of RPC command requests.
  zx_status_t HandleRpc(const RpcFunctionArgs& args, RpcFunctionResult* out_result);
  zx_status_t HandleRpcAllocateMemory(const RpcFunctionAllocateMemoryArgs& args,
                                      RpcFunctionAllocateMemoryResult* out_result);
  zx_status_t HandleRpcFreeMemory(const RpcFunctionFreeMemoryArgs& args,
                                  RpcFunctionFreeMemoryResult* out_result);

  // Identifies and dispatches the second layer of RPC command requests.
  //
  // This dispatcher is actually a specific command in the first layer of RPC requests.
  zx_status_t HandleRpcCommand(const RpcFunctionExecuteCommandsArgs& args,
                               RpcFunctionExecuteCommandsResult* out_result);
  zx_status_t HandleRpcCommandLoadTa(LoadTaRpcMessage* message);
  zx_status_t HandleRpcCommandAccessRpmb(RpmbRpcMessage* message);
  zx_status_t HandleRpcCommandWaitQueue(WaitQueueRpcMessage* message);
  static zx_status_t HandleRpcCommandGetTime(GetTimeRpcMessage* message);
  zx_status_t HandleRpcCommandAllocateMemory(AllocateMemoryRpcMessage* message);
  zx_status_t HandleRpcCommandFreeMemory(FreeMemoryRpcMessage* message);

  // Move in the FileSystemRpcMessage since it'll be moved into a sub-type in this function.
  zx_status_t HandleRpcCommandFileSystem(FileSystemRpcMessage&& message);
  zx_status_t HandleRpcCommandFileSystemOpenFile(OpenFileFileSystemRpcMessage* message);
  zx_status_t HandleRpcCommandFileSystemCreateFile(CreateFileFileSystemRpcMessage* message);
  zx_status_t HandleRpcCommandFileSystemCloseFile(CloseFileFileSystemRpcMessage* message);
  zx_status_t HandleRpcCommandFileSystemReadFile(ReadFileFileSystemRpcMessage* message);
  zx_status_t HandleRpcCommandFileSystemWriteFile(WriteFileFileSystemRpcMessage* message);
  zx_status_t HandleRpcCommandFileSystemTruncateFile(TruncateFileFileSystemRpcMessage* message);
  zx_status_t HandleRpcCommandFileSystemRemoveFile(RemoveFileFileSystemRpcMessage* message);
  zx_status_t HandleRpcCommandFileSystemRenameFile(RenameFileFileSystemRpcMessage* message);

  zx_status_t RpmbGetDevInfo(std::optional<SharedMemoryView> tx_frames,
                             std::optional<SharedMemoryView> rx_frames);
  zx_status_t RpmbRouteFrames(std::optional<SharedMemoryView> tx_frames,
                              std::optional<SharedMemoryView> rx_frames);
  zx_status_t RpmbReadRequest(std::optional<SharedMemoryView> tx_frames,
                              std::optional<SharedMemoryView> rx_frames);
  zx_status_t RpmbWriteRequest(std::optional<SharedMemoryView> tx_frames,
                               std::optional<SharedMemoryView> rx_frames);
  zx_status_t RpmbSendRequest(std::optional<SharedMemoryView>& req,
                              std::optional<SharedMemoryView>& resp);

  OpteeControllerBase* controller_;
  SharedMemoryList allocated_shared_memory_;
  std::atomic<uint64_t> next_file_system_object_id_{1};

  // Currently the only supported filesystem objects are files. In the future when support for
  // directories is added, this data structure will need to be generalized.
  std::unordered_map<uint64_t, fidl::ClientEnd<fuchsia_io::File>> open_file_system_objects_;
  std::unordered_set<uint32_t> open_sessions_;

  // A client implementing the `fuchsia.tee.manager.Provider` protocol. The client may be
  // uninitialized which indicates the optee client has no provider support.
  fidl::WireSyncClient<fuchsia_tee_manager::Provider> provider_;

  // A lazily-initialized, cached channel to the root storage channel.
  // This may be an invalid channel, which indicates it has not been initialized yet.
  fidl::ClientEnd<fuchsia_io::Directory> root_storage_;

  // A lazily-initialized, cached the Rpmb client.
  fidl::WireSyncClient<fuchsia_hardware_rpmb::Rpmb> rpmb_client_;

  // The (only) trusted application UUID this client is allowed to use.
  const Uuid application_uuid_;
};

}  // namespace optee

#endif  // SRC_DEVICES_TEE_DRIVERS_OPTEE_OPTEE_CLIENT_H_
