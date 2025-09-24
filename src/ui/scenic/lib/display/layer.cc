// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/layer.h"

namespace display::internal {

void Layer::SetPrimaryConfig(const Extent2& image_dimensions, uint32_t image_tiling_type) {
  if (!std::holds_alternative<ImageLayerSpec>(draft_spec_.config)) {
    draft_spec_ = ImageLayerSpec{};
  }
  auto& spec = std::get<ImageLayerSpec>(draft_spec_.config);
  spec.image_dimensions = image_dimensions;
  spec.image_tiling_type = image_tiling_type;

  draft_image_ = kInvalidImageId;
  draft_wait_event_ = kInvalidEventId;
}

void Layer::SetPrimaryPosition(const RotateFlip& transform, const Rectangle& src,
                               const Rectangle& dst) {
  FX_DCHECK(std::holds_alternative<ImageLayerSpec>(draft_spec_.config));
  auto& spec = std::get<ImageLayerSpec>(draft_spec_.config);
  spec.image_source_transformation = transform;
  spec.display_destination = dst;
  spec.image_source = src;
}

void Layer::SetPrimaryAlpha(const BlendMode& blend_mode, float alpha_value) {
  FX_DCHECK(std::holds_alternative<ImageLayerSpec>(draft_spec_.config));
  auto& spec = std::get<ImageLayerSpec>(draft_spec_.config);
  spec.blend_mode = blend_mode;
  spec.alpha_value = alpha_value;
}

void Layer::SetLayerImage(const ImageId& image_id, const EventId& wait_event_id) {
  FX_DCHECK(image_id != kInvalidImageId);
  draft_image_ = image_id;
  draft_wait_event_ = wait_event_id;
}

void Layer::SetColorConfig(const WireColor& color, const Rectangle& display_destination) {
  draft_spec_ = ColorLayerSpec{
      .color = color,
      .display_destination = display_destination,
  };
  draft_image_ = kInvalidImageId;
  draft_wait_event_ = kInvalidEventId;
}

size_t Layer::SendDiffsToCoordinator(
    const LayerId& layer_id,
    fidl::WireSharedClient<fuchsia_hardware_display::Coordinator>& shared_coordinator) {
  if (std::holds_alternative<ImageLayerSpec>(draft_spec_.config)) {
    return SetImageLayerDiffsToCoordinator(layer_id, shared_coordinator);
  }
  if (std::holds_alternative<ColorLayerSpec>(draft_spec_.config)) {
    return SetColorLayerDiffsToCoordinator(layer_id, shared_coordinator);
  }
  // Uninitialized layer, no diffs to send.
  FX_DCHECK(std::holds_alternative<UninitializedLayerSpec>(draft_spec_.config));
  return 0;
}

void Layer::ResetDraftState() {
  draft_spec_ = applied_spec_;
  draft_image_ = applied_image_;
  draft_wait_event_ = applied_wait_event_;
}

void Layer::AcceptDraftState() {
  applied_spec_ = draft_spec_;
  applied_image_ = draft_image_;
  applied_wait_event_ = draft_wait_event_;
}

size_t Layer::SetImageLayerDiffsToCoordinator(
    const LayerId& layer_id,
    fidl::WireSharedClient<fuchsia_hardware_display::Coordinator>& shared_coordinator) {
  FX_DCHECK(std::holds_alternative<ImageLayerSpec>(draft_spec_.config));
  const auto& draft_spec = std::get<ImageLayerSpec>(draft_spec_.config);
  const bool applied_spec_is_image_spec =
      std::holds_alternative<ImageLayerSpec>(applied_spec_.config);

  const WireLayerId wire_layer_id = layer_id.ToFidl();
  auto sync = shared_coordinator.sync();

  size_t api_calls_sent = 0;

  bool must_set_config = false;
  bool must_set_position = false;
  bool must_set_alpha = false;
  bool must_set_image = false;

  // Determine which values must be set by calling Coordinator methods.
  if (!applied_spec_is_image_spec) {
    // Transitioning to image config from some other type of config (either color or uninitialized),
    // therefore must set all draft values.
    must_set_config = true;
    must_set_position = true;
    must_set_alpha = true;
    must_set_image = true;
  } else {
    const auto& applied_spec = std::get<ImageLayerSpec>(applied_spec_.config);

    must_set_config = draft_spec.image_dimensions != applied_spec.image_dimensions ||
                      draft_spec.image_tiling_type != applied_spec.image_tiling_type;
    must_set_position =
        draft_spec.image_source_transformation != applied_spec.image_source_transformation ||
        draft_spec.image_source != applied_spec.image_source ||
        draft_spec.display_destination != applied_spec.display_destination;
    must_set_alpha = draft_spec.blend_mode != applied_spec.blend_mode ||
                     draft_spec.alpha_value != applied_spec.alpha_value;
    // Setting the config clears the image in the Coordinator impl, so `must_set_config == true`
    // means that we must set the image even if it matches the already-applied image.
    must_set_image =
        (draft_image_ != kInvalidImageId) && (must_set_config || draft_image_ != applied_image_ ||
                                              draft_wait_event_ != applied_wait_event_);
  }

  if (must_set_config) {
    ++api_calls_sent;
    const fidl::OneWayStatus status = sync->SetLayerPrimaryConfig(
        wire_layer_id, WireImageMetadata{.dimensions = draft_spec.image_dimensions.ToWire(),
                                         .tiling_type = draft_spec.image_tiling_type});
    FX_DCHECK(status.ok()) << "Failed to call FIDL SetLayerPrimaryConfig method: "
                           << status.status_string();
  }

  if (must_set_position) {
    ++api_calls_sent;
    const fidl::OneWayStatus status = sync->SetLayerPrimaryPosition(
        wire_layer_id, draft_spec.image_source_transformation.ToDisplayCoordinateTransformation(),
        draft_spec.image_source.ToWireRectU(), draft_spec.display_destination.ToWireRectU());

    FX_DCHECK(status.ok()) << "Failed to call FIDL SetLayerPrimaryPosition method: "
                           << status.status_string();
  }

  if (must_set_alpha) {
    ++api_calls_sent;
    const fidl::OneWayStatus status = sync->SetLayerPrimaryAlpha(
        wire_layer_id, draft_spec.blend_mode.ToDisplayAlphaMode(), draft_spec.alpha_value);

    FX_DCHECK(status.ok()) << "Failed to call FIDL SetLayerPrimaryAlpha method: "
                           << status.status_string();
  }

  if (must_set_image) {
    ++api_calls_sent;
    const fidl::OneWayStatus status =
        sync->SetLayerImage2(wire_layer_id, draft_image_.ToFidl(), draft_wait_event_.ToFidl());

    FX_DCHECK(status.ok()) << "Failed to call FIDL SetLayerImage2 method: "
                           << status.status_string();
  }

  return api_calls_sent;
}

size_t Layer::SetColorLayerDiffsToCoordinator(
    const LayerId& layer_id,
    fidl::WireSharedClient<fuchsia_hardware_display::Coordinator>& shared_coordinator) {
  FX_DCHECK(std::holds_alternative<ColorLayerSpec>(draft_spec_.config));

  if (draft_spec_ == applied_spec_) {
    return 0;  // zero FIDL calls sent
  }
  const auto& draft_spec = std::get<ColorLayerSpec>(draft_spec_.config);
  const fidl::OneWayStatus status = shared_coordinator.sync()->SetLayerColorConfig(
      layer_id.ToFidl(), draft_spec.color, draft_spec.display_destination.ToWireRectU());
  FX_DCHECK(status.ok()) << "Failed to call FIDL SetLayerImage2 method: " << status.status_string();
  return 1;  // one FIDL call sent
}

}  // namespace display::internal