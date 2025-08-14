// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "regulator-visitor.h"

#include <fidl/fuchsia.hardware.vreg/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devicetree/visitors/registration.h>
#include <lib/driver/logging/cpp/logger.h>
#include <zircon/errors.h>

#include <memory>
#include <regex>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/hardware/vreg/cpp/bind.h>
#include <bind/fuchsia/regulator/cpp/bind.h>

namespace regulator_visitor_dt {

RegulatorVisitor::RegulatorVisitor() {
  fdf_devicetree::Properties reference_properties = {};
  reference_properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kRegulatorReference, kRegulatorCells, /* required */ false));
  reference_parser_ =
      std::make_unique<fdf_devicetree::PropertyParser>(std::move(reference_properties));

  fdf_devicetree::Properties regulator_properties = {};
  regulator_properties.emplace_back(
      std::make_unique<fdf_devicetree::StringProperty>(kRegulatorName));
  regulator_properties.emplace_back(std::make_unique<fdf_devicetree::Uint32Property>(
      kRegulatorMaxMicrovolt, /* required */ false));
  regulator_properties.emplace_back(std::make_unique<fdf_devicetree::Uint32Property>(
      kRegulatorMinMicrovolt, /* required */ false));
  regulator_properties.emplace_back(std::make_unique<fdf_devicetree::Uint32Property>(
      kRegulatorStepMicrovolt, /* required */ false));
  parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(regulator_properties));
}

bool RegulatorVisitor::is_match(const std::string& name) {
  std::regex name_regex(".*regulator(@.*)?$");
  return std::regex_match(name, name_regex);
}

zx::result<> RegulatorVisitor::Visit(fdf_devicetree::Node& node,
                                     const devicetree::PropertyDecoder& decoder) {
  if (is_match(node.name())) {
    auto parser_output = parser_->Parse(node);
    if (parser_output.is_error()) {
      FDF_LOG(ERROR, "Regulator visitor failed for node '%s' : %s", node.name().c_str(),
              parser_output.status_string());
      return parser_output.take_error();
    }

    auto status = AddRegulatorMetadata(node, *parser_output);
    if (status.is_error()) {
      FDF_LOG(ERROR, "Failed to add regulator metadata '%s' : %s", node.name().c_str(),
              status.status_string());
      return status.take_error();
    }
  }

  auto reference_output = reference_parser_->Parse(node);
  if (reference_output.is_error()) {
    FDF_LOG(ERROR, "Regulator visitor failed for node '%s' : %s", node.name().c_str(),
            reference_output.status_string());
    return reference_output.take_error();
  }

  auto references = reference_output->Get<fdf_devicetree::References>(kRegulatorReference);
  if (!references) {
    return zx::ok();
  }

  for (auto& reference : *references) {
    auto reference_node = reference.reference_node();

    uint32_t current_count = static_cast<uint32_t>(0);
    auto count_entry = regulator_client_count_.find(reference_node.phandle().value());
    if (count_entry == regulator_client_count_.end()) {
      regulator_client_count_.insert(std::pair<fdf_devicetree::Phandle, uint16_t>(
          reference_node.phandle().value(), static_cast<uint16_t>(current_count)));
    } else {
      count_entry->second++;
      current_count = static_cast<uint32_t>(count_entry->second);
    }

    auto status = AddChildNodeSpec(node, reference_node, current_count);
    if (status.is_error()) {
      FDF_LOG(ERROR, "Failed to add regulator '%s' node spec to '%s' : %s",
              reference.reference_node().name().c_str(), node.name().c_str(),
              status.status_string());
      return status.take_error();
    }
  }

  return zx::ok();
}

zx::result<> RegulatorVisitor::AddRegulatorMetadata(fdf_devicetree::Node& node,
                                                    fdf_devicetree::ParsedProperties& properties) {
  auto name = properties.Get<std::string>(kRegulatorName);
  if (!name) {
    FDF_LOG(ERROR, "Regulator node '%s' does not have a name.", node.name().c_str());
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  fuchsia_hardware_vreg::VregMetadata metadata;
  metadata.name() = *name;

  auto min_uv = properties.Get<uint32_t>(kRegulatorMinMicrovolt);
  if (min_uv) {
    metadata.min_voltage_uv() = *min_uv;
  }

  auto step_uv = properties.Get<uint32_t>(kRegulatorStepMicrovolt);
  if (step_uv) {
    metadata.voltage_step_uv() = *step_uv;
  }

  auto max_uv = properties.Get<uint32_t>(kRegulatorMaxMicrovolt);
  if (max_uv && metadata.voltage_step_uv() && metadata.min_voltage_uv()) {
    if (*max_uv < *metadata.min_voltage_uv()) {
      FDF_LOG(ERROR, "Regulator max voltage (%d) is not more than min voltage (%d) in node '%s'",
              *max_uv, *metadata.min_voltage_uv(), node.name().c_str());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    auto voltage_range = (*max_uv - *metadata.min_voltage_uv());

    if (voltage_range % (*metadata.voltage_step_uv()) != 0) {
      FDF_LOG(ERROR, "Voltage range (%d) is not a multiple of step size (%d) for node '%s'",
              voltage_range, (*metadata.voltage_step_uv()), node.name().c_str());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    metadata.num_steps() = (voltage_range / *metadata.voltage_step_uv()) + 1;
  }

  fit::result persisted_metadata = fidl::Persist(metadata);
  if (!persisted_metadata.is_ok()) {
    FDF_LOG(ERROR, "Failed to persist vreg metadata for node %s: %s", node.name().c_str(),
            persisted_metadata.error_value().FormatDescription().c_str());
    return zx::error(persisted_metadata.error_value().status());
  }
  fuchsia_hardware_platform_bus::Metadata vreg_metadata = {{
      .id = fuchsia_hardware_vreg::VregMetadata::kSerializableName,
      .data = std::move(persisted_metadata.value()),
  }};

  node.AddMetadata(std::move(vreg_metadata));
  FDF_LOG(DEBUG, "Added regulator metadata to node '%s'.", node.name().c_str());

  return zx::ok();
}

// Note that `instance_id` is zero-based.
zx::result<> RegulatorVisitor::AddChildNodeSpec(fdf_devicetree::Node& child,
                                                fdf_devicetree::ReferenceNode& parent,
                                                uint32_t instance_id) {
  auto regulator_name = parent.GetProperty<std::string>(kRegulatorName);
  if (regulator_name.is_error()) {
    FDF_LOG(ERROR, "Regulator node '%s' does not have a name: %s.", parent.name().c_str(),
            regulator_name.status_string());
    return regulator_name.take_error();
  }

  auto regulator_node = fuchsia_driver_framework::ParentSpec2{{
      .bind_rules =
          {
              fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_vreg::SERVICE,
                                       bind_fuchsia_hardware_vreg::SERVICE_ZIRCONTRANSPORT),
              fdf::MakeAcceptBindRule2(bind_fuchsia_regulator::NAME, *regulator_name),
              fdf::MakeAcceptBindRule2(bind_fuchsia::REGULATOR_NODE_ID, instance_id),
          },
      .properties =
          {
              fdf::MakeProperty2(bind_fuchsia_hardware_vreg::SERVICE,
                                 bind_fuchsia_hardware_vreg::SERVICE_ZIRCONTRANSPORT),
              fdf::MakeProperty2(bind_fuchsia_regulator::NAME, *regulator_name),
          },
  }};

  child.AddNodeSpec(regulator_node);
  FDF_LOG(DEBUG, "Added regulator node spec of '%s' to '%s'.", parent.name().c_str(),
          child.name().c_str());

  return zx::ok();
}

}  // namespace regulator_visitor_dt

REGISTER_DEVICETREE_VISITOR(regulator_visitor_dt::RegulatorVisitor);
