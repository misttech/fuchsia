// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "clockimpl-visitor.h"

#include <fidl/fuchsia.hardware.clockimpl/cpp/fidl.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devicetree/visitors/registration.h>
#include <lib/driver/logging/cpp/logger.h>
#include <zircon/assert.h>
#include <zircon/errors.h>

#include <cstdint>
#include <memory>
#include <set>
#include <utility>

#include <bind/fuchsia/clock/cpp/bind.h>
#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/hardware/clock/cpp/bind.h>

namespace clock_impl_dt {

namespace {
using fuchsia_hardware_clockimpl::InitCall;
using fuchsia_hardware_clockimpl::InitStep;

class ClockCells {
 public:
  explicit ClockCells(fdf_devicetree::PropertyCells cells) : clock_cells_(cells, 1) {}

  // 1st cell denotes the clock ID.
  uint32_t id() { return static_cast<uint32_t>(*clock_cells_[0][0]); }

 private:
  using ClockElement = devicetree::PropEncodedArrayElement<1>;
  devicetree::PropEncodedArray<ClockElement> clock_cells_;
};

}  // namespace

ClockImplVisitor::ClockImplVisitor() {
  fdf_devicetree::Properties properties = {};
  properties.emplace_back(
      std::make_unique<fdf_devicetree::StringListProperty>(kClockNames, /* required */ false));
  properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kClockReference, kClockCells, /* required */ false));
  properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kAssignedClocks, kClockCells, /* required */ false));
  properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kAssignedClockParents, kClockCells, /* required */ false));
  properties.emplace_back(
      std::make_unique<fdf_devicetree::Uint32ArrayProperty>(kAssignedClockRates,
                                                            /* required */ false));
  clock_parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(properties));
}

bool ClockImplVisitor::is_match(const fdf_devicetree::Node& node) {
  auto clock_cells = node.GetProperty<uint32_t>(kClockCells);
  if (clock_cells.is_error()) {
    return false;
  }

  return *clock_cells == 1;
}

uint32_t ClockImplVisitor::GetNextUniqueId() { return next_unique_id_++; }

zx::result<> ClockImplVisitor::Visit(fdf_devicetree::Node& node,
                                     const devicetree::PropertyDecoder& decoder) {
  zx::result parser_output = clock_parser_->Parse(node);
  if (parser_output.is_error()) {
    FDF_LOG(ERROR, "Clock visitor failed for node '%s' : %s", node.name().c_str(),
            parser_output.status_string());
    return parser_output.take_error();
  }

  // Parse clocks and clock-names
  if (auto clocks = parser_output->Get<fdf_devicetree::References>(kClockReference)) {
    auto clock_names = parser_output->Get<std::vector<std::string>>(kClockNames);
    if (!clock_names && clocks->size() != 1u) {
      FDF_LOG(
          ERROR,
          "Clock reference '%s' does not have valid clock names property. Name is required to generate bind rules, especially when more than one clock is referenced.",
          node.name().c_str());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    for (uint32_t index = 0; index < clocks->size(); index++) {
      auto& reference = (*clocks)[index];
      if (is_match(*reference.reference_node().GetNode())) {
        std::optional<std::string_view> name;
        if (clock_names) {
          name = (*clock_names)[index];
        }
        auto result =
            ParseReferenceChild(node, reference.reference_node(), reference.property_cells(), name);
        if (result.is_error()) {
          return result.take_error();
        }
      }
    }
  }

  // Parse assigned-clocks and related properties.
  if (auto assigned_clocks = parser_output->Get<fdf_devicetree::References>(kAssignedClocks)) {
    auto clock_parents = parser_output->Get<fdf_devicetree::References>(kAssignedClockParents);
    if (clock_parents && clock_parents->size() > assigned_clocks->size()) {
      FDF_LOG(ERROR, "Assigned clock parents in '%s' has more entries than assigned clocks.",
              node.name().c_str());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    auto clock_rates = parser_output->Get<std::vector<uint32_t>>(kAssignedClockRates);
    if (clock_rates && clock_rates->size() > assigned_clocks->size()) {
      FDF_LOG(ERROR, "Assigned clock rates in '%s' has more entries than assigned clocks.",
              node.name().c_str());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    // Track the clock controllers referenced so that we can add bind rule only once per controller.
    std::set<uint32_t> init_controllers;
    for (uint32_t index = 0; index < assigned_clocks->size(); index++) {
      auto& reference = (*assigned_clocks)[index];
      if (is_match(*reference.reference_node().GetNode())) {
        std::optional<fdf_devicetree::Reference> parent;
        if (clock_parents && index < clock_parents->size()) {
          parent = (*clock_parents)[index];
        }
        std::optional<uint32_t> rate;
        if (clock_rates && index < clock_rates->size()) {
          rate = (*clock_rates)[index];
        }
        auto result = ParseInitChild(node, reference.reference_node(), reference.property_cells(),
                                     rate, parent);
        if (result.is_error()) {
          return result.take_error();
        }

        if (init_controllers.find(reference.reference_node().id()) == init_controllers.end()) {
          result = AddInitChildNodeSpec(node);
          if (result.is_error()) {
            return result.take_error();
          }
          init_controllers.insert(reference.reference_node().id());
        }
      }
    }
  }

  return zx::ok();
}

zx::result<> ClockImplVisitor::AddChildNodeSpec(fdf_devicetree::Node& child, uint32_t clock_id,
                                                uint32_t node_id,
                                                std::optional<std::string_view> clock_name) {
  auto clock_node = fuchsia_driver_framework::ParentSpec2{{
      .bind_rules =
          {
              fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_clock::SERVICE,
                                       bind_fuchsia_hardware_clock::SERVICE_ZIRCONTRANSPORT),
              fdf::MakeAcceptBindRule2(bind_fuchsia::CLOCK_ID, clock_id),
              fdf::MakeAcceptBindRule2(bind_fuchsia::CLOCK_NODE_ID, node_id),
          },
      .properties =
          {
              fdf::MakeProperty2(bind_fuchsia_hardware_clock::SERVICE,
                                 bind_fuchsia_hardware_clock::SERVICE_ZIRCONTRANSPORT),
          },
  }};

  if (clock_name) {
    clock_node.properties().push_back(fdf::MakeProperty2(
        bind_fuchsia_clock::FUNCTION, "fuchsia.clock.FUNCTION." + std::string(*clock_name)));
    clock_node.properties().push_back(
        fdf::MakeProperty2(bind_fuchsia_clock::NAME, std::string(*clock_name)));
  }

  child.AddNodeSpec(clock_node);
  return zx::ok();
}

zx::result<> ClockImplVisitor::AddInitChildNodeSpec(fdf_devicetree::Node& child) {
  auto clock_init_node = fuchsia_driver_framework::ParentSpec2{{
      .bind_rules = {fdf::MakeAcceptBindRule2(bind_fuchsia::INIT_STEP,
                                              bind_fuchsia_clock::BIND_INIT_STEP_CLOCK)},
      .properties =
          {
              fdf::MakeProperty2(bind_fuchsia::INIT_STEP, bind_fuchsia_clock::BIND_INIT_STEP_CLOCK),
          },
  }};
  child.AddNodeSpec(clock_init_node);
  return zx::ok();
}

ClockImplVisitor::ClockController& ClockImplVisitor::GetController(
    fdf_devicetree::Phandle phandle) {
  if (!clock_controllers_.contains(phandle)) {
    clock_controllers_[phandle] = ClockController();
  }
  return clock_controllers_[phandle];
}

zx::result<> ClockImplVisitor::ParseReferenceChild(fdf_devicetree::Node& child,
                                                   fdf_devicetree::ReferenceNode& parent,
                                                   fdf_devicetree::PropertyCells specifiers,
                                                   std::optional<std::string_view> clock_name) {
  auto& controller = GetController(*parent.phandle());

  if (specifiers.size_bytes() != 1 * sizeof(uint32_t)) {
    FDF_LOG(ERROR,
            "Clock reference '%s' has incorrect number of clock specifiers (%lu) - expected 1.",
            child.name().c_str(), specifiers.size_bytes() / sizeof(uint32_t));
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  auto cells = ClockCells(specifiers);
  const uint32_t clock_id = cells.id();
  const uint32_t node_id = GetNextUniqueId();

  FDF_LOG(DEBUG, "Clock ID added - Unique ID %u, Clock ID 0x%x name '%s' to controller '%s'",
          node_id, clock_id, clock_name ? std::string(*clock_name).c_str() : "<anonymous>",
          parent.name().c_str());

  auto& clock_nodes = controller.clock_nodes_metadata.clock_nodes();
  if (!clock_nodes.has_value()) {
    clock_nodes.emplace(std::vector<fuchsia_hardware_clockimpl::ClockNodeDescriptor>{});
  }
  clock_nodes.value().emplace_back(fuchsia_hardware_clockimpl::ClockNodeDescriptor{{
      .clock_id = clock_id,
      .node_id = node_id,
  }});

  return AddChildNodeSpec(child, clock_id, node_id, clock_name);
}

zx::result<> ClockImplVisitor::ParseInitChild(
    fdf_devicetree::Node& child, fdf_devicetree::ReferenceNode& parent,
    fdf_devicetree::PropertyCells specifiers, std::optional<uint32_t> clock_rate,
    std::optional<fdf_devicetree::Reference> clock_parent) {
  auto& controller = GetController(*parent.phandle());
  auto clock = ClockCells(specifiers);

  if ((clock_rate && *clock_rate != 0) || clock_parent) {
    controller.init_metadata.steps().push_back({{clock.id(), InitCall::WithDisable({})}});
  }

  if (clock_parent) {
    auto parent_clock = ClockCells(clock_parent->property_cells());
    controller.init_metadata.steps().push_back(
        {{clock.id(), InitCall::WithInputIdx(parent_clock.id())}});
    FDF_LOG(DEBUG, "Clock parent set to %d for clock ID %d by '%s'.", parent_clock.id(), clock.id(),
            child.name().c_str());
  }

  if (clock_rate) {
    // Skip setting rates for 0 as per the clock bindings.
    if (*clock_rate != 0) {
      controller.init_metadata.steps().push_back({{clock.id(), InitCall::WithRateHz(*clock_rate)}});

      FDF_LOG(DEBUG, "Clock initial rate set to %d for clock ID %d by '%s'.", *clock_rate,
              clock.id(), child.name().c_str());
    }
  }

  controller.init_metadata.steps().push_back({{clock.id(), InitCall::WithEnable({})}});

  return zx::ok();
}

zx::result<> ClockImplVisitor::FinalizeNode(fdf_devicetree::Node& node) {
  // Check that it is indeed a clock-controller that we support.
  if (!is_match(node)) {
    return zx::ok();
  }

  if (node.phandle()) {
    auto controller = clock_controllers_.find(*node.phandle());
    if (controller == clock_controllers_.end()) {
      FDF_LOG(INFO, "Clock controller '%s' is not being used. Not adding any metadata for it.",
              node.name().c_str());
      return zx::ok();
    }

    if (!controller->second.init_metadata.steps().empty()) {
      const fit::result encoded_metadata = fidl::Persist(controller->second.init_metadata);
      if (!encoded_metadata.is_ok()) {
        FDF_LOG(ERROR, "Failed to encode clock init metadata: %s",
                encoded_metadata.error_value().FormatDescription().c_str());
        return zx::error(encoded_metadata.error_value().status());
      }

      node.AddMetadata({{
          .id = fuchsia_hardware_clockimpl::InitMetadata::kSerializableName,
          .data = std::move(encoded_metadata.value()),
      }});

      FDF_LOG(DEBUG, "Clock init steps metadata added to node '%s'", node.name().c_str());
    }

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
    const auto& clock_nodes = controller->second.clock_nodes_metadata.clock_nodes();
    if (clock_nodes.has_value() && !clock_nodes.value().empty()) {
      const fit::result encoded_clock_id_metadata =
          fidl::Persist(controller->second.clock_nodes_metadata);
      if (!encoded_clock_id_metadata.is_ok()) {
        FDF_LOG(ERROR, "Failed to encode clock ID's: %s",
                encoded_clock_id_metadata.error_value().FormatDescription().c_str());
        return zx::error(encoded_clock_id_metadata.error_value().status());
      }
      fuchsia_hardware_platform_bus::Metadata metadata = {{
          .id = fuchsia_hardware_clockimpl::wire::ClockIdsMetadata::kSerializableName,
          .data = encoded_clock_id_metadata.value(),
      }};
      node.AddMetadata(std::move(metadata));

      FDF_LOG(DEBUG, "Clock ID's metadata added to node '%s'", node.name().c_str());
    }
#endif
  }

  return zx::ok();
}

}  // namespace clock_impl_dt

REGISTER_DEVICETREE_VISITOR(clock_impl_dt::ClockImplVisitor);
