// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/examples/screen_recording/screen_capture_helper.h"

#include "fuchsia/sysmem2/cpp/fidl.h"
#include "src/ui/scenic/lib/flatland/buffers/util.h"
#include "src/ui/scenic/lib/utils/helpers.h"
#include "zircon/system/ulib/fbl/include/fbl/algorithm.h"

namespace screen_recording_example {

using flatland::MapHostPointer;
using fuchsia::ui::composition::RegisterBufferCollectionArgs;
using fuchsia::ui::composition::RegisterBufferCollectionUsages;

fuchsia::sysmem2::BufferCollectionInfo CreateBufferCollectionInfoWithConstraints(
    fuchsia::sysmem2::BufferCollectionConstraints constraints,
    fuchsia::ui::composition::BufferCollectionExportToken export_token,
    fuchsia::ui::composition::Allocator_Sync* flatland_allocator,
    fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
    RegisterBufferCollectionUsages usage) {
  FX_DCHECK(flatland_allocator);
  FX_DCHECK(sysmem_allocator);

  RegisterBufferCollectionArgs rbc_args = {};

  // Create Sysmem tokens.
  auto [local_token, dup_token] = utils::CreateSysmemTokensHlcpp(sysmem_allocator);

  rbc_args.set_export_token(std::move(export_token));
  // BufferCollectionToken zircon handles are interchangeable between fuchsia::sysmem2
  // and fuchsia::sysmem(1).
  rbc_args.set_buffer_collection_token2(std::move(dup_token));
  rbc_args.set_usages(usage);

  fuchsia::sysmem2::BufferCollectionSyncPtr buffer_collection;
  fidl::Arena arena;
  fidl::OneWayStatus result = sysmem_allocator->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>(
              local_token.Unbind().TakeChannel()))
          .buffer_collection_request(fidl::ServerEnd<fuchsia_sysmem2::BufferCollection>(
              buffer_collection.NewRequest().TakeChannel()))
          .Build());
  FX_DCHECK(result.ok());

  fuchsia::sysmem2::BufferCollectionSetConstraintsRequest constraints_request;
  constraints_request.set_constraints(std::move(constraints));
  zx_status_t status = buffer_collection->SetConstraints(std::move(constraints_request));
  FX_DCHECK(status == ZX_OK);

  fuchsia::ui::composition::Allocator_RegisterBufferCollection_Result register_result;
  flatland_allocator->RegisterBufferCollection(std::move(rbc_args), &register_result);
  FX_DCHECK(!register_result.is_err());

  // Wait for allocation.
  zx_status_t allocation_status = ZX_OK;
  fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result wait_result;
  status = buffer_collection->WaitForAllBuffersAllocated(&wait_result);
  FX_DCHECK(ZX_OK == status);
  FX_DCHECK(ZX_OK == allocation_status);
  FX_DCHECK(wait_result.is_response());
  FX_DCHECK(constraints.min_buffer_count() ==
            wait_result.response().buffer_collection_info().buffers().size());

  buffer_collection->Release();
  return std::move(*wait_result.response().mutable_buffer_collection_info());
}

}  // namespace screen_recording_example
