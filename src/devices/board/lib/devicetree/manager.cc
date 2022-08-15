// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/board/lib/devicetree/manager.h"

#include <fidl/fuchsia.boot/cpp/fidl.h>
#include <fidl/fuchsia.driver.framework/cpp/natural_types.h>
#include <lib/driver2/structured_logger.h>
#include <zircon/boot/image.h>

#include "lib/devicetree/devicetree.h"
#include "src/devices/board/lib/devicetree/node.h"

namespace fdf {
using namespace fuchsia_driver_framework;
}

namespace fdf_devicetree {
constexpr const char kPhandleProp[] = "phandle";
constexpr const char kCompatibleProp[] = "compatible";

Manager::Manager(std::vector<uint8_t> fdt_blob, driver::Logger& logger)
    : fdt_blob_(std::move(fdt_blob)),
      tree_(devicetree::ByteView{fdt_blob_.data(), fdt_blob_.size()}),
      logger_(logger) {
  property_callbacks_.emplace_back(fit::bind_member(this, &Manager::PhandlePropertyCallback));
  property_callbacks_.emplace_back(fit::bind_member(this, &Manager::BindRulePropertyCallback));
}

zx::status<Manager> Manager::CreateFromNamespace(driver::Namespace& ns, driver::Logger& logger) {
  auto client_end = ns.Connect<fuchsia_boot::Items>();
  if (client_end.is_error()) {
    FDF_LOGL(ERROR, logger, "Failed to connect to fuchsia.boot.Items: %s",
             client_end.status_string());
    return client_end.take_error();
  }

  fidl::WireSyncClient<fuchsia_boot::Items> client(std::move(client_end.value()));
  auto result = client->Get2(ZBI_TYPE_DEVICETREE, {});
  if (!result.ok()) {
    FDF_LOGL(ERROR, logger, "Failed to send get2 request: %s", result.FormatDescription().data());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    FDF_LOGL(ERROR, logger, "Failed to get2: %s", zx_status_get_string(result->error_value()));
    return zx::error(result->error_value());
  }

  auto items = result->value()->retrieved_items;
  if (items.count() != 1) {
    FDF_LOGL(ERROR, logger, "Found wrong number of devicetrees: wanted 1, got %zu", items.count());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  auto& dt = result->value()->retrieved_items[0];
  std::vector<uint8_t> data;
  data.resize(dt.length);

  zx_status_t status = dt.payload.read(data.data(), 0, dt.length);
  if (status != ZX_OK) {
    FDF_LOGL(ERROR, logger, "Failed to read %u bytes from the devicetree: %s", dt.length,
             zx_status_get_string(status));
    return zx::error(status);
  }

  return zx::ok(Manager(std::move(data), logger));
}

zx::status<> Manager::Discover() {
  Node* parent = nullptr;
  Node* prev = nullptr;
  size_t depth = 0;
  tree_.Walk([&, this](const devicetree::NodePath& path, devicetree::Properties properties) {
    size_t new_depth = path.size_slow();
    if (depth > new_depth) {
      // We've ascended.
      parent = parent->parent();
    } else if (depth < new_depth) {
      // We've descended.
      parent = prev;
    }
    depth = new_depth;

    // Create a node.
    auto node = std::make_unique<Node>(parent, path.back(), properties, node_id_++);
    Node* ptr = node.get();
    prev = ptr;
    nodes_publish_order_.emplace_back(std::move(node));

    // Call each property handler on each property of this node.
    for (auto prop : properties) {
      for (auto& handler : property_callbacks_) {
        handler(ptr, prop);
      }
    }

    return true;
  });
  return zx::ok();
}

zx::status<> Manager::PublishDevices(
    fdf::ClientEnd<fuchsia_hardware_platform_bus::PlatformBus> pbus,
    fidl::ClientEnd<fuchsia_driver_framework::Node> parent_node,
    fidl::ClientEnd<fuchsia_driver_framework::DeviceGroupManager> mgr) {
  auto pbus_client = fdf::WireSyncClient(std::move(pbus));
  auto parent_node_client = fidl::SyncClient(std::move(parent_node));
  auto mgr_client = fidl::SyncClient(std::move(mgr));

  for (auto& node : nodes_publish_order_) {
    auto status = node->Publish(logger_, pbus_client, parent_node_client, mgr_client);
    if (status.is_error()) {
      return status.take_error();
    }
  }

  return zx::ok();
}

// Record nodes with phandles.
void Manager::PhandlePropertyCallback(Node* node, devicetree::Property property) {
  if (property.name != kPhandleProp) {
    return;
  }

  if (property.value.AsUint32() != std::nullopt) {
    nodes_by_phandle_.emplace(property.value.AsUint32().value(), node);
  } else {
    FDF_SLOG(WARNING, "Node has invalid phandle property", KV("node_name", node->name()),
             KV("prop_len", property.value.AsBytes().size()));
  }
}

//
void Manager::BindRulePropertyCallback(Node* node, devicetree::Property property) {
  fdf::NodeProperty prop;
  if (property.name != kCompatibleProp) {
    // TODO(fxbug.dev/107029): support extra "bind,..." properties as bind properties.
    return;
  }
  // Make sure value is a string.
  if (property.value.AsStringList() == std::nullopt) {
    FDF_SLOG(WARNING, "Node has invalid compatible property", KV("node_name", node->name()),
             KV("prop_len", property.value.AsBytes().size()));
  }
  prop.key() = fdf::NodePropertyKey::WithStringValue("fuchsia.devicetree.first_compatible");

  prop.value() =
      fdf::NodePropertyValue::WithStringValue(std::string(*property.value.AsStringList()->begin()));
  node->AddBindProperty(std::move(prop));
}

}  // namespace fdf_devicetree
