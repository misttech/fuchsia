// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "mailbox-visitor.h"

#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/devicetree/visitors/common-types.h>
#include <lib/driver/devicetree/visitors/registration.h>

#include <bind/fuchsia/hardware/mailbox/cpp/bind.h>
#include <bind/fuchsia/mailbox/cpp/bind.h>

namespace {

constexpr char kMailboxesProperty[] = "mboxes";
constexpr char kMailboxNamesProperty[] = "mbox-names";
constexpr char kMailboxCellsProperty[] = "#mbox-cells";

zx::result<uint32_t> ParseChannel(fdf_devicetree::Reference& reference) {
  const std::optional<uint32_t> channel =
      devicetree::PropertyValue(reference.property_cells()).AsUint32();
  if (!channel) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(*channel);
}

}  // namespace

namespace mailbox_dt {

MailboxVisitor::MailboxVisitor() {
  fdf_devicetree::Properties mailbox_properties = {};
  mailbox_properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kMailboxesProperty, 1u, /*required=*/false));
  mailbox_parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(mailbox_properties));
}

zx::result<> MailboxVisitor::Visit(fdf_devicetree::Node& node,
                                   const devicetree::PropertyDecoder& decoder) {
  auto properties = mailbox_parser_->Parse(node);
  if (properties.is_error()) {
    FDF_LOG(ERROR, "Failed to parse node \"%s\"", node.name().c_str());
    return properties.take_error();
  }
  auto channels = properties->Get<fdf_devicetree::References>(kMailboxesProperty);
  if (!channels) {
    return zx::ok();
  }

  std::vector<std::string_view> channel_names;
  auto channel_names_property = node.GetProperty<std::vector<std::string>>(kMailboxNamesProperty);
  if (channel_names_property.is_ok()) {
    channel_names.reserve(channel_names_property->size());
    for (const auto& name : *channel_names_property) {
      channel_names.push_back(name);
    }
  } else if (channel_names_property.status_value() != ZX_ERR_NOT_FOUND) {
    FDF_LOG(ERROR, "Failed to parse mbox-names property for node \"%s\": %s", node.name().c_str(),
            channel_names_property.status_string());
    return channel_names_property.take_error();
  }

  if (!channel_names.empty() && channels->size() != channel_names.size()) {
    FDF_LOG(ERROR, "mboxes and mbox-names mismatch for node \"%s\"", node.name().c_str());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  auto channel_name_it = channel_names.cbegin();

  std::map<uint32_t, uint32_t> controller_ids;
  uint32_t current_controller_id = 0;

  for (auto& reference : *channels) {
    zx::result<uint32_t> channel = ParseChannel(reference);
    if (channel.is_error()) {
      FDF_LOG(ERROR, "Failed to parse mailbox channel reference for node \"%s\"",
              node.name().c_str());
      return channel.take_error();
    }

    // Map the node ID to a controller index to be used only in the node properties. This way
    // clients with multiple mailbox parents don't need to know the actual controller ID.
    if (!controller_ids.contains(reference.reference_node().id())) {
      controller_ids[reference.reference_node().id()] = current_controller_id++;
    }
    const uint32_t local_controller_id = controller_ids[reference.reference_node().id()];

    controller_info_[reference.reference_node().id()].push_back({{.channel = *channel}});

    fuchsia_driver_framework::ParentSpec2 parent_spec{{
        .bind_rules =
            {
                fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_mailbox::SERVICE,
                                         bind_fuchsia_hardware_mailbox::SERVICE_ZIRCONTRANSPORT),
                fdf::MakeAcceptBindRule2(bind_fuchsia_mailbox::CONTROLLER_ID,
                                         reference.reference_node().id()),
                fdf::MakeAcceptBindRule2(bind_fuchsia_mailbox::CHANNEL, *channel),
            },
        .properties =
            {
                fdf::MakeProperty2(bind_fuchsia_hardware_mailbox::SERVICE,
                                   bind_fuchsia_hardware_mailbox::SERVICE_ZIRCONTRANSPORT),
                fdf::MakeProperty2(bind_fuchsia_mailbox::CONTROLLER_ID, local_controller_id),
                fdf::MakeProperty2(bind_fuchsia_mailbox::CHANNEL, *channel),
            },
    }};

    if (channel_name_it != channel_names.cend()) {
      parent_spec.properties().push_back(
          fdf::MakeProperty2(bind_fuchsia_mailbox::CHANNEL_NAME, *channel_name_it));
      *channel_name_it++;
    }

    node.AddNodeSpec(parent_spec);
  }

  return zx::ok();
}

zx::result<> MailboxVisitor::FinalizeNode(fdf_devicetree::Node& node) {
  const uint32_t controller_id = node.id();

  const auto channels = controller_info_.find(controller_id);
  if (channels == controller_info_.end()) {
    return zx::ok();  // Not a mailbox controller or no channels -- ignore.
  }

  auto mbox_cells = node.GetProperty<uint32_t>(kMailboxCellsProperty);
  if (mbox_cells.is_error()) {
    FDF_LOG(ERROR, "Missing #mbox-cells property for node \"%s\": %s", node.name().c_str(),
            mbox_cells.status_string());
    return mbox_cells.take_error();
  }

  if (*mbox_cells != 1) {
    FDF_LOG(ERROR, "Invalid #mbox-cells property for node \"%s\"", node.name().c_str());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fuchsia_hardware_mailbox::ControllerInfo controller{{.id = controller_id}};
  if (channels != controller_info_.end()) {
    controller.channels() = channels->second;
  }

  fit::result<fidl::Error, std::vector<uint8_t>> metadata = fidl::Persist(controller);
  if (metadata.is_error()) {
    FDF_LOG(ERROR, "Failed to persist mailbox controller metadata: %s",
            metadata.error_value().FormatDescription().c_str());
    return zx::error(metadata.error_value().status());
  }

  fuchsia_hardware_platform_bus::Metadata pbus_metadata{{
      .id = fuchsia_hardware_mailbox::ControllerInfo::kSerializableName,
      .data = *std::move(metadata),
  }};
  node.AddMetadata(std::move(pbus_metadata));

  return zx::ok();
}

}  // namespace mailbox_dt

REGISTER_DEVICETREE_VISITOR(mailbox_dt::MailboxVisitor);
