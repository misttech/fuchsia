// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_TESTS_DRIVER_RUNNER_TEST_FIXTURE_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_TESTS_DRIVER_RUNNER_TEST_FIXTURE_H_

#include <fidl/fuchsia.component.decl/cpp/test_base.h>
#include <fidl/fuchsia.component/cpp/test_base.h>
#include <fidl/fuchsia.driver.framework/cpp/test_base.h>
#include <fidl/fuchsia.driver.host/cpp/test_base.h>
#include <fidl/fuchsia.io/cpp/test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fit/defer.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/inspect/testing/cpp/inspect.h>

#include <bind/fuchsia/platform/cpp/bind.h>

#include "src/devices/bin/driver_manager/driver_runner.h"
#include "src/devices/bin/driver_manager/testing/fake_driver_index.h"
#include "src/devices/bin/driver_manager/tests/test_pkg.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

namespace driver_runner {

namespace fdfw = fuchsia_driver_framework;
namespace fdh = fuchsia_driver_host;
namespace fio = fuchsia_io;
namespace fprocess = fuchsia_process;
namespace fdecl = fuchsia_component_decl;

const std::string root_driver_url = "fuchsia-boot:///#meta/root-driver.cm";
const std::string root_driver_binary = "driver/root-driver.so";

const std::string second_driver_url = "fuchsia-boot:///#meta/second-driver.cm";
const std::string second_driver_binary = "driver/second-driver.so-";

using driver_manager::Devfs;
using driver_manager::DriverRunner;
using driver_manager::InspectManager;

static const test_utils::TestPkg::Config kDefaultDriverHostPkgConfig = {
    .module_test_pkg_path = "/pkg/bin/fake_driver_host_with_bootstrap",
    .module_open_path = "bin/driver_host2",
    .expected_libs =
        {
            "libdh-deps-a.so",
            "libdh-deps-b.so",
            "libdh-deps-c.so",
        },
};

static const test_utils::TestPkg::Config kDefaultDriverPkgConfig = {
    .module_test_pkg_path = "/pkg/lib/fake_root_driver.so",
    .module_open_path = "driver/fake_root_driver.so",
    .expected_libs =
        {
            "libfake_root_driver_deps.so",
        },
};

struct NodeChecker {
  std::vector<std::string> node_name;
  std::vector<std::string> child_names;
  std::map<std::string, std::string> str_properties;
};

struct CreatedChild {
  std::optional<fidl::Client<fdfw::Node>> node;
  std::optional<fidl::Client<fdfw::NodeController>> node_controller;
};

void CheckNode(const inspect::Hierarchy& hierarchy, const NodeChecker& checker);

class TestRealm : public fidl::testing::TestBase<fuchsia_component::Realm> {
 public:
  using CreateChildHandler = fit::function<void(fdecl::CollectionRef collection, fdecl::Child decl,
                                                std::vector<fdecl::Offer> offers)>;
  using OpenExposedDirHandler =
      fit::function<void(fdecl::ChildRef child, fidl::ServerEnd<fio::Directory> exposed_dir)>;

  void SetCreateChildHandler(CreateChildHandler create_child_handler) {
    create_child_handler_ = std::move(create_child_handler);
  }

  void SetOpenExposedDirHandler(OpenExposedDirHandler open_exposed_dir_handler) {
    open_exposed_dir_handler_ = std::move(open_exposed_dir_handler);
  }

  fidl::VectorView<fprocess::wire::HandleInfo> TakeHandles(fidl::AnyArena& arena);

  void AssertDestroyedChildren(const std::vector<fdecl::ChildRef>& expected);

 private:
  void CreateChild(CreateChildRequest& request, CreateChildCompleter::Sync& completer) override;

  void DestroyChild(DestroyChildRequest& request, DestroyChildCompleter::Sync& completer) override;

  void OpenExposedDir(OpenExposedDirRequest& request,
                      OpenExposedDirCompleter::Sync& completer) override;

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: Realm::%s\n", name.c_str());
  }

  CreateChildHandler create_child_handler_;
  OpenExposedDirHandler open_exposed_dir_handler_;
  std::optional<std::vector<fprocess::HandleInfo>> handles_;
  std::vector<fdecl::ChildRef> destroyed_children_;
};

class TestDirectory final : public fidl::testing::TestBase<fio::Directory> {
 public:
  using OpenHandler =
      fit::function<void(const std::string& path, fidl::ServerEnd<fio::Node> object)>;

  explicit TestDirectory(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  void Bind(fidl::ServerEnd<fio::Directory> request);

  void SetOpenHandler(OpenHandler open_handler) { open_handler_ = std::move(open_handler); }

 private:
  void Open(OpenRequest& request, OpenCompleter::Sync& completer) override;

  void Open3(Open3Request& request, Open3Completer::Sync& completer) override;

  void handle_unknown_method(fidl::UnknownMethodMetadata<fio::Directory>,
                             fidl::UnknownMethodCompleter::Sync&) override;

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: Directory::%s\n", name.c_str());
  }

  async_dispatcher_t* dispatcher_;
  fidl::ServerBindingGroup<fio::Directory> bindings_;
  OpenHandler open_handler_;
};

struct Driver {
  std::string url;
  std::string binary;
  bool colocate = false;
  bool close = false;
  bool host_restart_on_crash = false;
  bool use_next_vdso = false;
  bool use_dynamic_linker = false;
};

class TestDriver : public fidl::testing::TestBase<fdh::Driver> {
 public:
  explicit TestDriver(async_dispatcher_t* dispatcher, fidl::ClientEnd<fdfw::Node> node,
                      fidl::ServerEnd<fdh::Driver> server)
      : dispatcher_(dispatcher),
        stop_handler_([]() {}),
        node_(std::move(node), dispatcher),
        driver_binding_(dispatcher, std::move(server), this, fidl::kIgnoreBindingClosure) {}

  fidl::Client<fdfw::Node>& node() { return node_; }

  using StopHandler = fit::function<void()>;
  void SetStopHandler(StopHandler handler) { stop_handler_ = std::move(handler); }

  void SetDontCloseBindingInStop() { dont_close_binding_in_stop_ = true; }

  void Stop(StopCompleter::Sync& completer) override;

  void DropNode() { node_ = {}; }
  void CloseBinding() { driver_binding_.Close(ZX_OK); }

  std::shared_ptr<CreatedChild> AddChild(std::string_view child_name, bool owned, bool expect_error,
                                         const std::string& class_name = "driver_runner_test");

  std::shared_ptr<CreatedChild> AddChild(
      fdfw::NodeAddArgs child_args, bool owned, bool expect_error,
      fit::function<void()> on_bind = []() {});

 private:
  async_dispatcher_t* dispatcher_;
  StopHandler stop_handler_;
  fidl::Client<fdfw::Node> node_;
  fidl::ServerBinding<fdh::Driver> driver_binding_;
  bool dont_close_binding_in_stop_ = false;

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: Driver::%s\n", name.c_str());
  }
};

class TestDriverHost : public fidl::testing::TestBase<fdh::DriverHost> {
 public:
  using StartHandler =
      fit::function<void(fdfw::DriverStartArgs start_args, fidl::ServerEnd<fdh::Driver> driver)>;

  void SetStartHandler(StartHandler start_handler) { start_handler_ = std::move(start_handler); }

 private:
  void Start(StartRequest& request, StartCompleter::Sync& completer) override {
    start_handler_(std::move(request.start_args()), std::move(request.driver()));
    completer.Reply(zx::ok());
  }

  void InstallLoader(InstallLoaderRequest& request,
                     InstallLoaderCompleter::Sync& completer) override {}

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: DriverHost::%s\n", name.data());
  }

  StartHandler start_handler_;
};

// Calls the driver host runner's component Start implementation.
void DriverHostComponentStart(driver_runner::TestRealm& realm,
                              driver_manager::DriverHostRunner& driver_host_runner,
                              fidl::ClientEnd<fuchsia_io::Directory> driver_host_pkg);

fidl::AnyTeardownObserver TeardownWatcher(size_t index, std::vector<size_t>& indices);
fdecl::ChildRef CreateChildRef(std::string name, std::string collection);

struct Driver;

class DriverRunnerTest : public gtest::TestLoopFixture {
 public:
  void TearDown() override { Unbind(); }

 protected:
  InspectManager& inspect() { return inspect_; }
  TestRealm& realm() { return realm_; }
  TestDirectory& driver_dir() { return driver_dir_; }
  TestDriverHost& driver_host() { return driver_host_; }

  fidl::WireClient<fuchsia_device::Controller> ConnectToDeviceController(
      std::string_view child_name);

  fidl::ClientEnd<fuchsia_component::Realm> ConnectToRealm();

  FakeDriverIndex CreateDriverIndex();

  void SetupDriverRunner(FakeDriverIndex driver_index);

  void SetupDriverRunnerWithDynamicLinker(
      async_dispatcher_t* loader_dispatcher,
      std::unique_ptr<driver_manager::DriverHostRunner> driver_host_runner);

  void SetupDriverRunner();

  void PrepareRealmForDriverComponentStart(const std::string& name, const std::string& url);

  void PrepareRealmForSecondDriverComponentStart();

  void PrepareRealmForStartDriverHost(bool use_next_vdso);

  void PrepareRealmForStartDriverHostDynamicLinker();

  void StopDriverComponent(
      fidl::ClientEnd<fuchsia_component_runner::ComponentController> component);

  struct StartDriverResult {
    std::unique_ptr<TestDriver> driver;
    fidl::ClientEnd<fuchsia_component_runner::ComponentController> controller;
  };

  using StartDriverHandler = fit::function<void(TestDriver*, fdfw::DriverStartArgs)>;

  // If |ns_pkg| is set, it will be provided as the /pkg directory in the driver component's
  // namespace.
  // If a new driver host is required to be started (i.e. the driver is not colocated),
  // and dynamic linking is enabled, |driver_host_pkg| will be provided as the /pkg directory in the
  // driver host component's namespace.
  StartDriverResult StartDriver(
      Driver driver, std::optional<StartDriverHandler> start_handler = std::nullopt,
      fidl::ClientEnd<fuchsia_io::Directory> ns_pkg = fidl::ClientEnd<fuchsia_io::Directory>(),
      fidl::ClientEnd<fuchsia_io::Directory> driver_host_pkg =
          fidl::ClientEnd<fuchsia_io::Directory>());

  zx::result<StartDriverResult> StartRootDriver();
  zx::result<StartDriverResult> StartRootDriverDynamicLinking(
      test_utils::TestPkg::Config driver_host_config = kDefaultDriverHostPkgConfig,
      test_utils::TestPkg::Config driver_config = kDefaultDriverPkgConfig);

  StartDriverResult StartSecondDriver(bool colocate = false, bool host_restart_on_crash = false,
                                      bool use_next_vdso = false);

  void Unbind();

  static void ValidateProgram(std::optional<::fuchsia_data::Dictionary>& program,
                              std::string_view binary, std::string_view colocate,
                              std::string_view host_restart_on_crash,
                              std::string_view use_next_vdso,
                              std::string_view use_dynamic_linker = "false");

  static void AssertNodeBound(const std::shared_ptr<CreatedChild>& child);

  static void AssertNodeNotBound(const std::shared_ptr<CreatedChild>& child);

  static void AssertNodeControllerBound(const std::shared_ptr<CreatedChild>& child);

  static void AssertNodeControllerNotBound(const std::shared_ptr<CreatedChild>& child);

  inspect::Hierarchy Inspect();

  void SetupDevfs();

  Devfs& devfs() {
    ZX_ASSERT(devfs_.has_value());
    return devfs_.value();
  }

  DriverRunner& driver_runner() { return driver_runner_.value(); }

  FakeDriverIndex& driver_index() { return driver_index_.value(); }

 private:
  TestRealm realm_;
  TestDirectory driver_host_dir_{dispatcher()};
  TestDirectory driver_dir_{dispatcher()};
  TestDriverHost driver_host_;
  fidl::ServerBindingGroup<fuchsia_component::Realm> realm_bindings_;
  fidl::ServerBindingGroup<fdh::DriverHost> driver_host_bindings_;

  std::optional<Devfs> devfs_;
  InspectManager inspect_{dispatcher()};
  std::optional<FakeDriverIndex> driver_index_;
  std::optional<DriverRunner> driver_runner_;
};

}  // namespace driver_runner

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_TESTS_DRIVER_RUNNER_TEST_FIXTURE_H_
