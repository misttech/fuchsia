// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/controller.h"

#include <lib/async/cpp/task.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/trace/event.h>
#include <lib/zbi-format/graphics.h>
#include <lib/zx/channel.h>
#include <lib/zx/clock.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>
#include <threads.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>
#include <zircon/threads.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <algorithm>
#include <cinttypes>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <optional>
#include <span>
#include <utility>
#include <vector>

#include <fbl/alloc_checker.h>
#include <fbl/array.h>
#include <fbl/auto_lock.h>
#include <fbl/ref_ptr.h>
#include <fbl/vector.h>

#include "src/graphics/display/drivers/coordinator/added-display-info.h"
#include "src/graphics/display/drivers/coordinator/client-id.h"
#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/coordinator/client-proxy.h"
#include "src/graphics/display/drivers/coordinator/display-info.h"
#include "src/graphics/display/drivers/coordinator/image.h"
#include "src/graphics/display/drivers/coordinator/layer.h"
#include "src/graphics/display/drivers/coordinator/post-display-task.h"
#include "src/graphics/display/drivers/coordinator/vsync-monitor.h"
#include "src/graphics/display/lib/api-types/cpp/config-stamp.h"
#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/display-timing.h"
#include "src/graphics/display/lib/api-types/cpp/driver-buffer-collection-id.h"
#include "src/graphics/display/lib/api-types/cpp/driver-capture-image-id.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"
#include "src/graphics/display/lib/edid/edid.h"

namespace fidl_display = fuchsia_hardware_display;

namespace display_coordinator {

void Controller::PopulateDisplayTimings(DisplayInfo& display_info) {
  if (!display_info.edid_info.has_value()) {
    return;
  }
  const edid::Edid& edid_info = display_info.edid_info.value();

  // Go through all the display mode timings and record whether or not
  // a basic layer configuration is acceptable.
  layer_t test_layers[] = {
      // The width and height will be replaced by the code below.
      layer_t{
          .display_destination = {.x = 0, .y = 0, .width = 0, .height = 0},
          .image_source = {.x = 0, .y = 0, .width = 0, .height = 0},
          .image_handle = INVALID_DISPLAY_ID,
          .image_metadata = {.dimensions = {.width = 0, .height = 0},
                             .tiling_type = IMAGE_TILING_TYPE_LINEAR},
          .fallback_color =
              {
                  .format = static_cast<uint32_t>(fuchsia_images2::PixelFormat::kR8G8B8A8),
                  .bytes = {0},
              },
          .alpha_mode = ALPHA_DISABLE,
          .alpha_layer_val = 0.0,
          .image_source_transformation = COORDINATE_TRANSFORMATION_IDENTITY,
      },
  };
  display_config_t test_config = {
      .display_id = display_info.id().ToBanjo(),
      .layers_list = test_layers,
      .layers_count = 1,
  };

  for (auto edid_timing_it = edid::timing_iterator(&edid_info); edid_timing_it.is_valid();
       ++edid_timing_it) {
    const display::DisplayTiming& edid_timing = *edid_timing_it;
    int32_t width = edid_timing.horizontal_active_px;
    int32_t height = edid_timing.vertical_active_lines;
    bool duplicate = false;
    for (const display::DisplayTiming& existing_timing : display_info.timings) {
      if (existing_timing.vertical_field_refresh_rate_millihertz() ==
              edid_timing.vertical_field_refresh_rate_millihertz() &&
          existing_timing.horizontal_active_px == width &&
          existing_timing.vertical_active_lines == height) {
        duplicate = true;
        break;
      }
    }
    if (duplicate) {
      continue;
    }

    layer_t& test_layer = test_layers[0];
    ZX_DEBUG_ASSERT_MSG(
        static_cast<const layer_t*>(&test_layer) == &test_config.layers_list[0],
        "test_layer should be a non-const alias for the first layer in test_configs");
    test_layer.image_metadata.dimensions.width = width;
    test_layer.image_metadata.dimensions.height = height;
    test_layer.image_source.width = width;
    test_layer.image_source.height = height;
    test_layer.display_destination.width = width;
    test_layer.display_destination.height = height;

    test_config.mode_id = INVALID_MODE_ID;
    test_config.timing = display::ToBanjoDisplayTiming(edid_timing);

    display::ConfigCheckResult config_check_result =
        engine_driver_client_->CheckConfiguration(&test_config);
    if (config_check_result != display::ConfigCheckResult::kOk) {
      continue;
    }

    fbl::AllocChecker alloc_checker;
    display_info.timings.push_back(edid_timing, &alloc_checker);
    if (!alloc_checker.check()) {
      fdf::warn("Failed to allocate memory for EDID timing. Skipping it.");
      break;
    }
  }
}

void Controller::AddDisplay(std::unique_ptr<AddedDisplayInfo> added_display_info) {
  ZX_DEBUG_ASSERT(IsRunningOnDriverDispatcher());

  zx::result<std::unique_ptr<DisplayInfo>> display_info_result =
      DisplayInfo::Create(std::move(*added_display_info));
  if (display_info_result.is_error()) {
    // DisplayInfo::Create() has already logged the error.
    return;
  }
  std::unique_ptr<DisplayInfo> display_info = std::move(display_info_result).value();

  if (display_info->edid_info.has_value()) {
    PopulateDisplayTimings(*display_info);
  }

  display::DisplayId display_id = display_info->id();
  const std::array<display::DisplayId, 1> added_id_candidates = {display_id};
  std::span<const display::DisplayId> added_ids(added_id_candidates);

  // TODO(https://fxbug.dev/339311596): Do not trigger the client's
  // `OnDisplaysChanged` if an added display is ignored.
  //
  // Dropping some add events can result in spurious removes, but
  // those are filtered out in the clients.
  if (!display_info->preferred_modes.is_empty() || !display_info->timings.is_empty()) {
    display_info->InitializeInspect(&root_);
  } else {
    fdf::warn("Ignoring display with no usable display preferred modes nor display timings");
    added_ids = {};
  }

  fbl::AutoLock<fbl::Mutex> lock(mtx());
  auto display_it = displays_.find(display_id);
  if (display_it != displays_.end()) {
    fdf::warn("Display {} is already created; add display request ignored", display_id.value());
    return;
  }
  displays_.insert(std::move(display_info));

  // TODO(https://fxbug.dev/317914671): Pass parsed display metadata to driver.

  if (virtcon_client_ready_) {
    ZX_DEBUG_ASSERT(virtcon_client_ != nullptr);
    virtcon_client_->OnDisplaysChanged(added_ids, /*removed_display_ids=*/{});
  }
  if (primary_client_ready_) {
    ZX_DEBUG_ASSERT(primary_client_ != nullptr);
    primary_client_->OnDisplaysChanged(added_ids, /*removed_display_ids=*/{});
  }
}

void Controller::RemoveDisplay(display::DisplayId removed_display_id) {
  ZX_DEBUG_ASSERT(IsRunningOnDriverDispatcher());

  fbl::AutoLock lock(mtx());
  std::unique_ptr<DisplayInfo> removed_display = displays_.erase(removed_display_id);
  if (!removed_display) {
    fdf::warn("Display removal references unknown display ID: {}", removed_display_id.value());
    return;
  }

  // Release references to all images on the display.
  while (removed_display->images.pop_front()) {
  }

  const std::array<display::DisplayId, 1> removed_display_ids = {
      removed_display_id,
  };
  if (virtcon_client_ready_) {
    ZX_DEBUG_ASSERT(virtcon_client_ != nullptr);
    virtcon_client_->OnDisplaysChanged(/*added_display_ids=*/{}, removed_display_ids);
  }
  if (primary_client_ready_) {
    ZX_DEBUG_ASSERT(primary_client_ != nullptr);
    primary_client_->OnDisplaysChanged(/*added_display_ids=*/{}, removed_display_ids);
  }
}

void Controller::OnDisplayAdded(std::unique_ptr<AddedDisplayInfo> added_display_info) {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->get() == engine_listener_dispatcher_->get());

  zx::result<> post_task_result = display::PostTask<kDisplayTaskTargetSize>(
      *driver_dispatcher()->async_dispatcher(),
      [this, added_display_info = std::move(added_display_info)]() mutable {
        AddDisplay(std::move(added_display_info));
      });
  if (post_task_result.is_error()) {
    fdf::error("Failed to dispatch AddDisplay task: {}", post_task_result);
  }
}

void Controller::OnDisplayRemoved(display::DisplayId removed_display_id) {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->get() == engine_listener_dispatcher_->get());

  zx::result<> post_task_result = display::PostTask<kDisplayTaskTargetSize>(
      *driver_dispatcher()->async_dispatcher(),
      [this, removed_display_id]() { RemoveDisplay(removed_display_id); });
  if (post_task_result.is_error()) {
    fdf::error("Failed to dispatch RemoveDisplay task: {}", post_task_result);
  }
}

void Controller::OnCaptureComplete() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->get() == engine_listener_dispatcher_->get());

  ZX_DEBUG_ASSERT_MSG(engine_info_.has_value(),
                      "OnCaptureComplete() called before engine connection completed");

  if (!engine_info_->is_capture_supported()) {
    fdf::error("OnCaptureComplete() called by a display engine without display capture support");
    return;
  }

  zx::result<> post_task_result =
      display::PostTask<kDisplayTaskTargetSize>(*driver_dispatcher()->async_dispatcher(), [this]() {
        // Free an image that was previously used by the hardware.
        if (pending_release_capture_image_id_ != display::kInvalidDriverCaptureImageId) {
          ReleaseCaptureImage(pending_release_capture_image_id_);
          pending_release_capture_image_id_ = display::kInvalidDriverCaptureImageId;
        }

        fbl::AutoLock lock(mtx());
        if (virtcon_client_ready_) {
          ZX_DEBUG_ASSERT(virtcon_client_ != nullptr);
          virtcon_client_->OnCaptureComplete();
        }
        if (primary_client_ready_) {
          ZX_DEBUG_ASSERT(primary_client_ != nullptr);
          primary_client_->OnCaptureComplete();
        }
      });
  if (post_task_result.is_error()) {
    fdf::error("Failed to dispatch capture complete task: {}", post_task_result);
  }
}

void Controller::OnDisplayVsync(display::DisplayId display_id, zx::time_monotonic timestamp,
                                display::DriverConfigStamp driver_config_stamp) {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->get() == engine_listener_dispatcher_->get());

  zx::result<> post_task_result = display::PostTask<kDisplayTaskTargetSize>(
      *driver_dispatcher()->async_dispatcher(),
      [this, display_id, timestamp, driver_config_stamp]() {
        ProcessDisplayVsync(display_id, timestamp, driver_config_stamp);
      });
  if (post_task_result.is_error()) {
    fdf::error("Failed to dispatch ProcessVsync task: {}", post_task_result);
  }
}

void Controller::ProcessDisplayVsync(display::DisplayId display_id, zx::time_monotonic timestamp,
                                     display::DriverConfigStamp driver_config_stamp) {
  ZX_DEBUG_ASSERT(IsRunningOnDriverDispatcher());

  // TODO(https://fxbug.dev/402445178): This trace event is load bearing for fps trace processor.
  // Remove it after changing the dependency.
  TRACE_INSTANT("gfx", "VSYNC", TRACE_SCOPE_THREAD, "display_id", display_id.value());
  // Emit a counter called "VSYNC" for visualization in the Trace Viewer. `vsync_edge_flag`
  // switching between 0 and 1 counts represents one vsync period.
  static bool vsync_edge_flag = false;
  TRACE_COUNTER("gfx", "VSYNC", display_id.value(), "",
                TA_UINT32(vsync_edge_flag = !vsync_edge_flag));
  TRACE_DURATION("gfx", "Display::Controller::OnDisplayVsync", "display_id", display_id.value());

  vsync_monitor_.OnVsync(timestamp, driver_config_stamp);

  fbl::AutoLock lock(mtx());
  auto displays_it = displays_.find(display_id);
  if (!displays_it.IsValid()) {
    fdf::error("Dropping VSync for unknown display ID: {}", display_id.value());
    return;
  }
  DisplayInfo& display_info = *displays_it;

  // See ::ApplyConfig for more explanation of how vsync image tracking works.
  //
  // If there's a pending layer change, don't process any present/retire actions
  // until the change is complete.
  if (display_info.pending_layer_change) {
    bool done = driver_config_stamp >= display_info.pending_layer_change_driver_config_stamp;
    if (done) {
      display_info.pending_layer_change = false;
      display_info.pending_layer_change_driver_config_stamp = display::kInvalidDriverConfigStamp;
      display_info.switching_client = false;
    }
  }

  // The display configuration associated with the VSync event can come
  // from one of the currently connected clients, or from a previously
  // connected client that is now disconnected.
  std::optional<ClientPriority> config_stamp_source = std::nullopt;
  ClientProxy* const client_proxies[] = {primary_client_, virtcon_client_};
  for (ClientProxy* client_proxy : client_proxies) {
    if (client_proxy == nullptr) {
      continue;
    }

    const std::list<ClientProxy::ConfigStampPair>& pending_stamps =
        client_proxy->pending_applied_config_stamps();
    auto it = std::ranges::find_if(pending_stamps,
                                   [&](const ClientProxy::ConfigStampPair& pending_stamp) {
                                     return pending_stamp.driver_stamp >= driver_config_stamp;
                                   });
    if (it != pending_stamps.end() && it->driver_stamp == driver_config_stamp) {
      config_stamp_source = std::make_optional(client_proxy->client_priority());
      // Obsolete stamps will be removed in `Client::OnDisplayVsync()`.
      break;
    }
  };

  if (!display_info.pending_layer_change) {
    // Each image in the `info->images` set can fall into one of the following
    // cases:
    // - being displayed (its `latest_controller_config_stamp` matches the
    //   incoming `controller_config_stamp` from display driver);
    // - older than the current displayed image (its
    //   `latest_controller_config_stamp` is less than the incoming
    //   `controller_config_stamp`) and should be retired;
    // - newer than the current displayed image (its
    //   `latest_controller_config_stamp` is greater than the incoming
    //   `controller_config_stamp`) and yet to be presented.
    for (auto it = display_info.images.begin(); it != display_info.images.end();) {
      bool should_retire = it->latest_driver_config_stamp() < driver_config_stamp;

      // Retire any images which are older than whatever is currently in their
      // layer.
      if (should_retire) {
        fbl::RefPtr<Image> image_to_retire = display_info.images.erase(it++);
        // Older images may not be presented. Ending their flows here
        // ensures the correctness of traces.
        //
        // NOTE: If changing this flow name or ID, please also do so in the
        // corresponding FLOW_BEGIN.
        TRACE_FLOW_END("gfx", "present_image", image_to_retire->id().value());
      } else {
        it++;
      }
    }
  }

  // Evict retired configurations from the queue.
  auto& config_image_queue = display_info.config_image_queue;
  while (!config_image_queue.empty() &&
         config_image_queue.front().config_stamp < driver_config_stamp) {
    config_image_queue.pop();
  }

  // Since the stamps sent from Controller to drivers are in chronological
  // order, the Vsync signals Controller receives should also be in
  // chronological order as well.
  //
  // Applying empty configs won't create entries in |config_image_queue|.
  // Otherwise, we'll get the list of images used at ApplyConfig() with
  // the given |config_stamp|.
  if (!config_image_queue.empty() &&
      config_image_queue.front().config_stamp == driver_config_stamp) {
    for (const auto& image : config_image_queue.front().images) {
      // End of the flow for the image going to be presented.
      //
      // NOTE: If changing this flow name or ID, please also do so in the
      // corresponding FLOW_BEGIN.
      TRACE_FLOW_END("gfx", "present_image", image.image_id.value());
    }
  }

  if (!config_stamp_source.has_value()) {
    // The config was applied by a client that is no longer connected.
    fdf::debug("VSync event dropped; the config owner disconnected");
    return;
  }

  switch (config_stamp_source.value()) {
    case ClientPriority::kPrimary:
      primary_client_->OnDisplayVsync(display_id, timestamp.get(), driver_config_stamp);
      break;
    case ClientPriority::kVirtcon:
      virtcon_client_->OnDisplayVsync(display_id, timestamp.get(), driver_config_stamp);
      break;
  }
}

void Controller::ApplyConfig(DisplayConfig& display_config,
                             display::ConfigStamp client_config_stamp, ClientId client_id) {
  zx_instant_mono_t timestamp = zx_clock_get_monotonic();
  last_valid_apply_config_timestamp_ns_property_.Set(timestamp);
  last_valid_apply_config_interval_ns_property_.Set(timestamp - last_valid_apply_config_timestamp_);
  last_valid_apply_config_timestamp_ = timestamp;

  last_valid_apply_config_config_stamp_property_.Set(client_config_stamp.value());

  display_config_t banjo_display_config;

  // The applied configuration's stamp.
  //
  // Populated from `controller_stamp_` while the mutex is held.
  display::DriverConfigStamp driver_config_stamp = {};

  {
    fbl::AutoLock lock(mtx());
    bool switching_client = client_id != applied_client_id_;

    ++last_issued_driver_config_stamp_;
    driver_config_stamp = last_issued_driver_config_stamp_;

    auto displays_it = displays_.find(display_config.id());
    if (!displays_it.IsValid()) {
      fdf::warn("ApplyConfig(): Cannot find display with id {}", display_config.id());
      return;
    }
    DisplayInfo& display_info = *displays_it;

    display_info.config_image_queue.push({.config_stamp = driver_config_stamp, .images = {}});

    display_info.switching_client = switching_client;
    display_info.pending_layer_change = display_config.apply_layer_change();
    if (display_info.pending_layer_change) {
      display_info.pending_layer_change_driver_config_stamp = driver_config_stamp;
    }
    display_info.layer_count = display_config.applied_layer_count();

    if (display_info.layer_count == 0) {
      // TODO(https://fxbug.dev/336394440): Make this a fatal error.
      fdf::warn("ApplyConfig(): config doesn't have any valid layer; skipped");
      return;
    }

    banjo_display_config = *display_config.applied_config();

    for (const LayerNode& applied_layer_node : display_config.get_applied_layers()) {
      const Layer* applied_layer = applied_layer_node.layer;
      fbl::RefPtr<Image> applied_image = applied_layer->applied_image();

      if (applied_layer->is_skipped() || applied_image == nullptr) {
        continue;
      }

      // Set the image controller config stamp so vsync knows what config the
      // image was used at.
      AssertMtxAliasHeld(*applied_image->mtx());
      applied_image->set_latest_driver_config_stamp(driver_config_stamp);

      // NOTE: If changing this flow name or ID, please also do so in the
      // corresponding FLOW_END.
      TRACE_FLOW_BEGIN("gfx", "present_image", applied_image->id().value());

      // It's possible that the image's layer was moved between displays. The logic around
      // pending_layer_change guarantees that the old display will be done with the image
      // before the new display is, so deleting it from the old list is fine.
      //
      // Even if we're on the same display, the entry needs to be moved to the end of the
      // list to ensure that the last config->current.layer_count elements in the queue
      // are the current images.
      //
      // TODO(https://fxbug.dev/317914671): investigate whether storing Images in doubly-linked
      //                                    lists continues to be desirable.
      if (applied_image->InDoublyLinkedList()) {
        applied_image->RemoveFromDoublyLinkedList();
      }
      display_info.images.push_back(applied_image);
      display_info.config_image_queue.back().images.push_back(
          {.image_id = applied_image->id(), .client_id = applied_image->client_id()});
    }

    applied_client_id_ = client_id;

    if (client_owning_displays_ != nullptr) {
      if (switching_client) {
        client_owning_displays_->ReapplySpecialConfigs();
      }

      client_owning_displays_->UpdateConfigStampMapping({
          .driver_stamp = driver_config_stamp,
          .client_stamp = client_config_stamp,
      });
    }
  }

  engine_driver_client_->ApplyConfiguration(&banjo_display_config, driver_config_stamp);
}

void Controller::ReleaseImage(display::DriverImageId driver_image_id) {
  engine_driver_client_->ReleaseImage(driver_image_id);
}

void Controller::ReleaseCaptureImage(display::DriverCaptureImageId driver_capture_image_id) {
  ZX_DEBUG_ASSERT_MSG(engine_info_.has_value(),
                      "CaptureImage created before engine connection completed");
  ZX_DEBUG_ASSERT_MSG(engine_info_->is_capture_supported(),
                      "CaptureImage created by engine without capture support");

  if (driver_capture_image_id == display::kInvalidDriverCaptureImageId) {
    return;
  }

  const zx::result<> result = engine_driver_client_->ReleaseCapture(driver_capture_image_id);
  if (result.is_error() && result.error_value() == ZX_ERR_SHOULD_WAIT) {
    ZX_DEBUG_ASSERT_MSG(pending_release_capture_image_id_ == display::kInvalidDriverCaptureImageId,
                        "multiple pending releases for capture images");
    // Delay the image release until the hardware is done.
    pending_release_capture_image_id_ = driver_capture_image_id;
  }
}

void Controller::SetVirtconMode(fuchsia_hardware_display::wire::VirtconMode virtcon_mode) {
  fbl::AutoLock lock(mtx());
  virtcon_mode_ = virtcon_mode;
  HandleClientOwnershipChanges();
}

void Controller::HandleClientOwnershipChanges() {
  ClientProxy* new_client_owning_displays;
  if (virtcon_mode_ == fidl_display::wire::VirtconMode::kForced ||
      (virtcon_mode_ == fidl_display::wire::VirtconMode::kFallback && primary_client_ == nullptr)) {
    new_client_owning_displays = virtcon_client_;
  } else {
    new_client_owning_displays = primary_client_;
  }

  if (new_client_owning_displays != client_owning_displays_) {
    if (client_owning_displays_ != nullptr) {
      client_owning_displays_->SetOwnership(false);
    }
    if (new_client_owning_displays) {
      new_client_owning_displays->SetOwnership(true);
    }
    client_owning_displays_ = new_client_owning_displays;
  }
}

void Controller::OnClientDead(ClientProxy* client) {
  fdf::info("Client {} dead", client->client_id().value());
  fbl::AutoLock lock(mtx());
  if (unbinding_) {
    return;
  }
  if (client == virtcon_client_) {
    virtcon_client_ = nullptr;
    virtcon_mode_ = fidl_display::wire::VirtconMode::kFallback;
    virtcon_client_ready_ = false;
  } else if (client == primary_client_) {
    primary_client_ = nullptr;
    primary_client_ready_ = false;
  } else {
    ZX_DEBUG_ASSERT_MSG(false, "Dead client is neither Virtcon nor Primary\n");
  }
  HandleClientOwnershipChanges();

  clients_.remove_if(
      [client](std::unique_ptr<ClientProxy>& list_client) { return list_client.get() == client; });
}

zx::result<std::span<const display::ModeAndId>> Controller::GetDisplayPreferredModes(
    display::DisplayId display_id) {
  if (unbinding_) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  auto displays_it = displays_.find(display_id);
  if (!displays_it.IsValid()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  const DisplayInfo& display_info = *displays_it;
  return zx::ok(std::span(display_info.preferred_modes));
}

zx::result<std::span<const display::DisplayTiming>> Controller::GetDisplayTimings(
    display::DisplayId display_id) {
  if (unbinding_) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  auto displays_it = displays_.find(display_id);
  if (!displays_it.IsValid()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  const DisplayInfo& display_info = *displays_it;
  return zx::ok(std::span(display_info.timings));
}

zx::result<fbl::Vector<display::PixelFormat>> Controller::GetSupportedPixelFormats(
    display::DisplayId display_id) {
  auto displays_it = displays_.find(display_id);
  if (!displays_it.IsValid()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  const DisplayInfo& display_info = *displays_it;

  fbl::AllocChecker alloc_checker;
  fbl::Vector<display::PixelFormat> pixel_formats;
  pixel_formats.reserve(display_info.pixel_formats.size(), &alloc_checker);
  if (!alloc_checker.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  std::ranges::copy(display_info.pixel_formats, std::back_inserter(pixel_formats));
  ZX_DEBUG_ASSERT(pixel_formats.size() == display_info.pixel_formats.size());

  return zx::ok(std::move(pixel_formats));
}

namespace {

void PrintChannelKoids(ClientPriority client_priority, const zx::channel& channel) {
  zx_info_handle_basic_t info{};
  size_t actual, avail;
  zx_status_t status = channel.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), &actual, &avail);
  if (status != ZX_OK || info.type != ZX_OBJ_TYPE_CHANNEL) {
    fdf::debug("Could not get koids for handle(type={}): {}", info.type, status);
    return;
  }
  ZX_DEBUG_ASSERT(actual == avail);
  fdf::info("{} client connecting on channel (c=0x{:x}, s=0x{:x})",
            DebugStringFromClientPriority(client_priority), info.related_koid, info.koid);
}

}  // namespace

zx_status_t Controller::CreateClient(
    ClientPriority client_priority,
    fidl::ServerEnd<fidl_display::Coordinator> coordinator_server_end,
    fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener> coordinator_listener_client_end,
    fit::function<void()> on_client_disconnected) {
  ZX_DEBUG_ASSERT(on_client_disconnected);

  PrintChannelKoids(client_priority, coordinator_server_end.channel());

  fbl::AllocChecker alloc_checker;
  auto post_task_state = fbl::make_unique_checked<DisplayTaskState>(&alloc_checker);
  if (!alloc_checker.check()) {
    fdf::debug("Failed to alloc client task");
    return ZX_ERR_NO_MEMORY;
  }

  fbl::AutoLock lock(mtx());
  if (unbinding_) {
    fdf::debug("Client connected during unbind");
    return ZX_ERR_UNAVAILABLE;
  }

  if ((client_priority == ClientPriority::kVirtcon && virtcon_client_ != nullptr) ||
      (client_priority == ClientPriority::kPrimary && primary_client_ != nullptr)) {
    fdf::debug("{} client already bound", DebugStringFromClientPriority(client_priority));
    return ZX_ERR_ALREADY_BOUND;
  }

  ClientId client_id = next_client_id_;
  ++next_client_id_;
  auto client = std::make_unique<ClientProxy>(this, client_priority, client_id,
                                              std::move(on_client_disconnected));

  zx_status_t status = client->Init(&root_, std::move(coordinator_server_end),
                                    std::move(coordinator_listener_client_end));
  if (status != ZX_OK) {
    fdf::debug("Failed to init client {}", status);
    return status;
  }

  ClientProxy* client_ptr = client.get();
  clients_.push_back(std::move(client));

  fdf::debug("New {} client [{}] connected.", DebugStringFromClientPriority(client_priority),
             client_ptr->client_id().value());

  switch (client_priority) {
    case ClientPriority::kVirtcon:
      ZX_DEBUG_ASSERT(virtcon_client_ == nullptr);
      ZX_DEBUG_ASSERT(!virtcon_client_ready_);
      virtcon_client_ = client_ptr;
      break;
    case ClientPriority::kPrimary:
      ZX_DEBUG_ASSERT(primary_client_ == nullptr);
      ZX_DEBUG_ASSERT(!primary_client_ready_);
      primary_client_ = client_ptr;
  }
  HandleClientOwnershipChanges();

  zx::result<> post_task_result = display::PostTask(
      std::move(post_task_state), *driver_dispatcher()->async_dispatcher(), [this, client_id]() {
        fbl::AutoLock lock(mtx());
        if (unbinding_) {
          return;
        }

        ClientProxy* client_proxy;
        if (virtcon_client_ != nullptr && virtcon_client_->client_id() == client_id) {
          client_proxy = virtcon_client_;
        } else if (primary_client_ != nullptr && primary_client_->client_id() == client_id) {
          client_proxy = primary_client_;
        } else {
          return;
        }

        // Add all existing displays to the client
        if (displays_.size() > 0) {
          display::DisplayId current_displays[displays_.size()];
          int initialized_display_count = 0;
          for (const DisplayInfo& display : displays_) {
            current_displays[initialized_display_count] = display.id();
            ++initialized_display_count;
          }
          std::span<display::DisplayId> removed_display_ids = {};
          client_proxy->OnDisplaysChanged(
              std::span<display::DisplayId>(current_displays, initialized_display_count),
              removed_display_ids);
        }

        if (virtcon_client_ == client_proxy) {
          ZX_DEBUG_ASSERT(!virtcon_client_ready_);
          virtcon_client_ready_ = true;
        } else {
          ZX_DEBUG_ASSERT(primary_client_ == client_proxy);
          ZX_DEBUG_ASSERT(!primary_client_ready_);
          primary_client_ready_ = true;
        }
      });
  return post_task_result.status_value();
}

display::DriverBufferCollectionId Controller::GetNextDriverBufferCollectionId() {
  fbl::AutoLock lock(mtx());
  return next_driver_buffer_collection_id_++;
}

void Controller::OpenCoordinatorWithListenerForVirtcon(
    OpenCoordinatorWithListenerForVirtconRequestView request,
    OpenCoordinatorWithListenerForVirtconCompleter::Sync& completer) {
  ZX_DEBUG_ASSERT(request->has_coordinator());
  ZX_DEBUG_ASSERT(request->has_coordinator_listener());
  zx_status_t create_status =
      CreateClient(ClientPriority::kVirtcon, std::move(request->coordinator()),
                   std::move(request->coordinator_listener()),
                   /*on_client_disconnected=*/[] {});
  if (create_status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(create_status);
  }
}

void Controller::OpenCoordinatorWithListenerForPrimary(
    OpenCoordinatorWithListenerForPrimaryRequestView request,
    OpenCoordinatorWithListenerForPrimaryCompleter::Sync& completer) {
  ZX_DEBUG_ASSERT(request->has_coordinator());
  ZX_DEBUG_ASSERT(request->has_coordinator_listener());
  zx_status_t create_status =
      CreateClient(ClientPriority::kPrimary, std::move(request->coordinator()),
                   std::move(request->coordinator_listener()),
                   /*on_client_disconnected=*/[] {});
  if (create_status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(create_status);
  }
}

// static
zx::result<std::unique_ptr<Controller>> Controller::Create(
    std::unique_ptr<EngineDriverClient> engine_driver_client,
    fdf::UnownedSynchronizedDispatcher driver_dispatcher,
    fdf::UnownedSynchronizedDispatcher engine_listener_dispatcher) {
  fbl::AllocChecker alloc_checker;

  auto controller = fbl::make_unique_checked<Controller>(
      &alloc_checker, std::move(engine_driver_client), std::move(driver_dispatcher),
      std::move(engine_listener_dispatcher));
  if (!alloc_checker.check()) {
    fdf::error("Failed to allocate memory for Controller");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  zx::result<> initialize_result = controller->Initialize();
  if (initialize_result.is_error()) {
    fdf::error("Failed to initialize the Controller device: {}", initialize_result);
    return initialize_result.take_error();
  }

  return zx::ok(std::move(controller));
}

zx::result<> Controller::Initialize() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->get() != engine_listener_dispatcher_->get());

  zx::result<> vsync_monitor_init_result = vsync_monitor_.Initialize();
  if (vsync_monitor_init_result.is_error()) {
    // VsyncMonitor::Init() logged the error.
    return vsync_monitor_init_result.take_error();
  }

  auto [fidl_listener_client, fidl_listener_server] =
      fdf::Endpoints<fuchsia_hardware_display_engine::EngineListener>::Create();

  // This binds `fidl_listener_server` to the EngineListenerFidlAdapter
  // instance synchronously. This is to avoid the case where
  // `engine_listener_dispatcher_` was shut down while the task is still
  // running, causing the Bind call to fail and crash the coordinator.
  libsync::Completion engine_listener_fidl_binding_completion;
  zx::result<> post_task_result = display::PostTask<kDisplayTaskTargetSize>(
      *engine_listener_dispatcher_->async_dispatcher(),
      [&, fidl_listener_server = std::move(fidl_listener_server)]() mutable {
        engine_listener_fidl_adapter_.CreateHandler()(std::move(fidl_listener_server));
        engine_listener_fidl_binding_completion.Signal();
      });
  if (post_task_result.is_error()) {
    fdf::error("Failed to dispatch EngineListener FIDL server binding task: {}", post_task_result);
    return post_task_result.take_error();
  }
  engine_listener_fidl_binding_completion.Wait();

  engine_info_ =
      engine_driver_client_->CompleteCoordinatorConnection(std::move(fidl_listener_client));
  fdf::info("Engine capabilities - max layers: {}, max displays: {}, display capture: {}",
            engine_info_->max_layer_count(), engine_info_->max_connected_display_count(),
            engine_info_->is_capture_supported() ? "yes" : "no");

  return zx::ok();
}

void Controller::PrepareStop() {
  fdf::info("Controller::PrepareStop started");

  {
    fbl::AutoLock lock(mtx());
    unbinding_ = true;

    // Tear down all existing clients. This ensures that all clients will not
    // send `ImportImage()` and `ApplyConfiguration()` requests.
    for (auto& client : clients_) {
      client->TearDown();
    }

    vsync_monitor_.Deinitialize();

    // Once this call completes, the engine driver will no longer send events.
    // This means it's safe to stop keeping track of imported resources.
    engine_driver_client_->UnsetListener();

    // Dispose of all images without calling ReleaseImage().
    for (DisplayInfo& display : displays_) {
      while (fbl::RefPtr<Image> displayed_image = display.images.pop_front()) {
        displayed_image->MarkDisposed();
      }
    }
  }

  fdf::info("Controller::PrepareStop finished");
}

void Controller::Stop() { fdf::info("Controller::Stop"); }

Controller::Controller(std::unique_ptr<EngineDriverClient> engine_driver_client,
                       fdf::UnownedSynchronizedDispatcher driver_dispatcher,
                       fdf::UnownedSynchronizedDispatcher engine_listener_dispatcher)
    : root_(inspector_.GetRoot().CreateChild("display")),
      driver_dispatcher_(std::move(driver_dispatcher)),
      engine_listener_dispatcher_(std::move(engine_listener_dispatcher)),
      engine_listener_fidl_adapter_(this, engine_listener_dispatcher_->borrow()),
      vsync_monitor_(root_.CreateChild("vsync_monitor"), driver_dispatcher_->async_dispatcher()),
      engine_driver_client_(std::move(engine_driver_client)) {
  ZX_DEBUG_ASSERT(engine_driver_client_ != nullptr);

  last_valid_apply_config_timestamp_ns_property_ =
      root_.CreateUint("last_valid_apply_config_timestamp_ns", 0);
  last_valid_apply_config_interval_ns_property_ =
      root_.CreateUint("last_valid_apply_config_interval_ns", 0);
  last_valid_apply_config_config_stamp_property_ =
      root_.CreateUint("last_valid_apply_config_stamp", display::kInvalidConfigStamp.value());
}

Controller::~Controller() { fdf::info("Controller::~Controller"); }

}  // namespace display_coordinator
