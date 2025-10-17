// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.dictionaryoffers.test/cpp/wire.h>
#include <fidl/fuchsia.driver.framework/cpp/common_types_format.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/component/cpp/node_add_args.h>

#include <bind/fuchsia/nodegroupbind/test/cpp/bind.h>

namespace ft = fuchsia_dictionaryoffers_test;
namespace bindlib = bind_fuchsia_nodegroupbind_test;

namespace {

fuchsia_driver_framework::CompositeNodeSpec NodeGroupOne() {
  auto bind_rules_left = std::vector{
      fdf::MakeAcceptBindRule(bindlib::TEST_BIND_PROPERTY, bindlib::TEST_BIND_PROPERTY_ONE_LEFT),
  };

  auto properties_left = std::vector{
      fdf::MakeProperty(bindlib::TEST_BIND_PROPERTY, bindlib::TEST_BIND_PROPERTY_DRIVER_LEFT),
  };

  auto bind_rules_right = std::vector{
      fdf::MakeAcceptBindRule(bindlib::TEST_BIND_PROPERTY, bindlib::TEST_BIND_PROPERTY_ONE_RIGHT),
  };

  auto properties_right = std::vector{
      fdf::MakeProperty(bindlib::TEST_BIND_PROPERTY, bindlib::TEST_BIND_PROPERTY_DRIVER_RIGHT),
  };

  auto parents = std::vector{
      fuchsia_driver_framework::ParentSpec{{
          .bind_rules = bind_rules_left,
          .properties = properties_left,
      }},
      fuchsia_driver_framework::ParentSpec{{
          .bind_rules = bind_rules_right,
          .properties = properties_right,
      }},
  };

  return {{.name = "test_group_1", .parents = parents}};
}

class RootDriver final : public fdf::DriverBase, public fidl::WireServer<ft::ControlPlane> {
 public:
  RootDriver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : fdf::DriverBase("root", std::move(start_args), std::move(driver_dispatcher)) {}

  zx::result<> Start() override {
    auto control = [this](fidl::ServerEnd<ft::ControlPlane> server_end) -> void {
      fidl::BindServer(dispatcher(), std::move(server_end), this);
    };

    ft::ControlService::InstanceHandler handler({.control = std::move(control)});

    auto result = outgoing()->AddService<ft::ControlService>(std::move(handler));
    if (result.is_error()) {
      fdf::error("Failed to add Device service: {}", result.status_string());
      return result.take_error();
    }

    auto dgm_client = incoming()->Connect<fuchsia_driver_framework::CompositeNodeManager>();
    if (dgm_client.is_error()) {
      fdf::error("Failed to connect to NodeGroupManager: {}",
                 zx_status_get_string(dgm_client.error_value()));
      return dgm_client.take_error();
    }

    fidl::Arena arena;
    fidl::WireResult add_spec_result =
        fidl::WireCall(*dgm_client)->AddSpec(fidl::ToWire(arena, NodeGroupOne()));
    if (!add_spec_result.ok()) {
      fdf::error("AddSpec call failed: {}", add_spec_result.FormatDescription());
      return zx::error(ZX_ERR_INTERNAL);
    }
    if (add_spec_result->is_error()) {
      fdf::error("AddSpec failed: {}", add_spec_result->error_value());
      return zx::error(ZX_ERR_INTERNAL);
    }

    return zx::ok();
  }

  void PrepareStop(fdf::PrepareStopCompleter completer) override {
    fdf::info("PrepareStop");
    completer(zx::ok());
  }

 private:
  // fidl::WireServer<ft::ControlPlane>
  void AddChild(fuchsia_dictionaryoffers_test::wire::ControlPlaneAddChildRequest* request,
                AddChildCompleter::Sync& completer) override {
    fdf::info("adding child...");
    auto [node_controller_client_end, node_controller_server_end] =
        fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
    fidl::WireResult result =
        fidl::WireCall(node())->AddChild(request->args, std::move(node_controller_server_end), {});

    if (!result.ok()) {
      completer.ReplyError(fuchsia_driver_framework::wire::NodeError::kInternal);
      return;
    }

    if (result->is_error()) {
      completer.ReplyError(result->error_value());
      return;
    }

    completer.ReplySuccess();
  }
};

}  // namespace

FUCHSIA_DRIVER_EXPORT(RootDriver);
