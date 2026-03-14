// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/screen_capture2/tests/common.h"

#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.composition/cpp/hlcpp_conversion.h>
#include <lib/ui/scenic/cpp/buffer_collection_import_export_tokens.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/allocation/allocator.h"
#include "src/ui/scenic/lib/flatland/engine/engine.h"
#include "src/ui/scenic/lib/screen_capture/screen_capture_buffer_collection_importer.h"
#include "src/ui/scenic/lib/utils/helpers.h"

using testing::_;

using allocation::Allocator;
using allocation::BufferCollectionImporter;
using screen_capture::ScreenCaptureBufferCollectionImporter;

namespace screen_capture2 {
namespace test {

std::shared_ptr<Allocator> CreateAllocator(
    std::shared_ptr<screen_capture::ScreenCaptureBufferCollectionImporter> importer,
    sys::ComponentContext* app_context, async_dispatcher_t* dispatcher) {
  std::vector<std::shared_ptr<BufferCollectionImporter>> extra_importers;
  std::vector<std::shared_ptr<BufferCollectionImporter>> screenshot_importers;
  screenshot_importers.push_back(importer);
  return std::make_shared<Allocator>(app_context, extra_importers, screenshot_importers,
                                     utils::CreateSysmemAllocatorClient(dispatcher, "-allocator"));
}

void CreateBufferCollectionInfoWithConstraints(
    fuchsia::sysmem2::BufferCollectionConstraints constraints,
    fuchsia::ui::composition::BufferCollectionExportToken export_token,
    std::shared_ptr<Allocator> flatland_allocator,
    fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
    fit::function<void(fit::function<bool()>)> run_loop_until) {
  // Create Sysmem tokens.
  auto [local_token, dup_token] = utils::CreateSysmemTokensHlcpp(sysmem_allocator);

  fuchsia_ui_composition::RegisterBufferCollectionArgs rbc_args;
  rbc_args.export_token(fidl::HLCPPToNatural(std::move(export_token)));
  rbc_args.buffer_collection_token2(fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>(
      std::move(dup_token).Unbind().TakeChannel()));
  rbc_args.usages(fuchsia_ui_composition::RegisterBufferCollectionUsages::kScreenshot);

  fuchsia::sysmem2::BufferCollectionPtr buffer_collection;
  fidl::Arena arena;
  fidl::OneWayStatus result = sysmem_allocator->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>(
              local_token.Unbind().TakeChannel()))
          .buffer_collection_request(fidl::ServerEnd<fuchsia_sysmem2::BufferCollection>(
              buffer_collection.NewRequest().TakeChannel()))
          .Build());
  FX_DCHECK(result.ok());

  fuchsia::sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.set_constraints(std::move(constraints));
  buffer_collection->SetConstraints(std::move(set_constraints_request));

  bool processed_callback = false;
  flatland_allocator->RegisterBufferCollection(std::move(rbc_args),
                                               [&processed_callback](auto result) {
                                                 EXPECT_TRUE(result.is_ok());
                                                 processed_callback = true;
                                               });

  // Wait for allocation and registration.
  fuchsia::sysmem2::BufferCollectionInfo buffer_collection_info;
  bool allocation_complete = false;
  buffer_collection->WaitForAllBuffersAllocated(
      [&allocation_complete, &buffer_collection_info](
          fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result result) {
        ASSERT_TRUE(result.is_response());
        buffer_collection_info = std::move(*result.response().mutable_buffer_collection_info());
        allocation_complete = true;
      });

  run_loop_until([&]() { return processed_callback && allocation_complete; });

  ASSERT_TRUE(processed_callback);
  ASSERT_TRUE(allocation_complete);
  buffer_collection->Release();
}

}  // namespace test
}  // namespace screen_capture2
