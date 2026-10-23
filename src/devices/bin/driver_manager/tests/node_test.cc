// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/node.h"

#include <fidl/fuchsia.component/cpp/wire_test_base.h>
#include <fidl/fuchsia.driver.framework/cpp/common_types_format.h>
#include <fidl/fuchsia.driver.host/cpp/test_base.h>
#include <fidl/fuchsia.power.broker/cpp/wire_test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/sync/cpp/completion.h>

#include <bind/fuchsia/platform/cpp/bind.h>

#include "src/devices/bin/driver_manager/driver_host.h"
#include "src/devices/bin/driver_manager/resource.h"
#include "src/devices/bin/driver_manager/tests/driver_manager_test_base.h"
#include "src/devices/bin/driver_manager/tests/driver_runner_test_fixture.h"

namespace fdf {
using namespace fuchsia_driver_framework;
}  // namespace fdf

class FakeNodeManager;
class TestController final : public fidl::testing::WireTestBase<fuchsia_component::Controller> {
 public:
  explicit TestController(FakeNodeManager* parent, std::string_view name,
                          fidl::ServerEnd<fuchsia_component::Controller> controller,
                          async_dispatcher_t* dispatcher)
      : parent_(parent),
        name_(name),
        dispatcher_(dispatcher),
        binding_(dispatcher_, std::move(controller), this, fidl::kIgnoreBindingClosure) {}

  void Destroy(DestroyCompleter::Sync& completer) override;

  void Start(StartRequestView request, StartCompleter::Sync& completer) override;

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_component::Controller> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  void NotImplemented_(const std::string& name, ::fidl::CompleterBase& completer) override {
    printf("Not implemented: TestController::%s\n", name.c_str());
  }

 private:
  FakeNodeManager* parent_;
  std::string name_;
  async_dispatcher_t* dispatcher_;
  fidl::ServerBinding<fuchsia_component::Controller> binding_;
};

class TestRealm final : public fidl::testing::WireTestBase<fuchsia_component::Realm> {
 public:
  explicit TestRealm(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  fidl::ClientEnd<fuchsia_component::Realm> Connect() {
    auto [client_end, server_end] = fidl::Endpoints<fuchsia_component::Realm>::Create();
    fidl::BindServer(dispatcher_, std::move(server_end), this);
    return std::move(client_end);
  }

  void CreateChild(CreateChildRequestView request, CreateChildCompleter::Sync& completer) override {
  }

  void DestroyChild(DestroyChildRequestView request,
                    DestroyChildCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }

 private:
  void NotImplemented_(const std::string& name, ::fidl::CompleterBase& completer) override {
    ZX_PANIC("Unimplemented %s", name.c_str());
  }

  async_dispatcher_t* dispatcher_;
  std::unordered_map<std::string, TestController> controllers_;
};

class FakeDriverHost : public driver_manager::DriverHost {
 public:
  using StartCallback = fit::callback<void(zx::result<>)>;
  void Start(fidl::ClientEnd<fuchsia_driver_framework::Node> client_end, std::string node_name,
             fuchsia_driver_framework::wire::NodePropertyDictionary2 node_properties,
             fidl::VectorView<fuchsia_driver_framework::wire::NodeSymbol> symbols,
             fidl::VectorView<fuchsia_driver_framework::wire::Offer> offers,
             fuchsia_component_runner::wire::ComponentStartInfo start_info, zx::event node_token,
             fidl::ServerEnd<fuchsia_driver_host::Driver> driver,
             driver_manager::PowerElementStartArgs power_element_args, StartCallback cb) override {
    drivers_[node_name] = std::move(driver);
    clients_[node_name] = std::move(client_end);
    cb(zx::ok());
  }

  zx::result<uint64_t> GetMainThreadKoid() const override {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  zx::result<uint64_t> GetProcessKoid() const override { return zx::error(ZX_ERR_NOT_SUPPORTED); }

  void GetProcessKoidAsync(fit::callback<void(zx::result<uint64_t>)> cb) const override {
    cb(zx::error(ZX_ERR_NOT_SUPPORTED));
  }

  void CloseDriver(std::string node_name, zx_status_t status = ZX_OK) {
    drivers_[node_name].Close(status);
    clients_[node_name].reset();
  }

  std::tuple<fidl::ServerEnd<fuchsia_driver_host::Driver>,
             fidl::ClientEnd<fuchsia_driver_framework::Node>>
  TakeDriver(const std::string& node_name) {
    auto driver = std::move(drivers_[node_name]);
    auto client = std::move(clients_[node_name]);
    drivers_.erase(node_name);
    clients_.erase(node_name);
    return std::make_tuple(std::move(driver), std::move(client));
  }

 private:
  std::unordered_map<std::string, fidl::ServerEnd<fuchsia_driver_host::Driver>> drivers_;
  std::unordered_map<std::string, fidl::ClientEnd<fuchsia_driver_framework::Node>> clients_;
};

class StopListener final
    : public fidl::WireAsyncEventHandler<fuchsia_component_runner::ComponentController> {
 public:
  explicit StopListener(async_dispatcher_t* dispatcher,
                        fidl::ClientEnd<fuchsia_component_runner::ComponentController> client)
      : client_(std::move(client), dispatcher, this) {}

  bool is_stopped() const { return stopped_; }

  fidl::WireClient<fuchsia_component_runner::ComponentController>& client() { return client_; }

  void on_fidl_error(::fidl::UnbindInfo error) override { stopped_ = true; }

  void OnStop(
      fidl::WireEvent<fuchsia_component_runner::ComponentController::OnStop>* event) override {
    stopped_ = true;
    auto _ = client_.UnbindMaybeGetEndpoint();
  }
  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_component_runner::ComponentController> metadata) override {
  }

 private:
  bool stopped_ = false;
  fidl::WireClient<fuchsia_component_runner::ComponentController> client_;
};

class FakeDictionaryUtil : public driver_manager::DictionaryUtil {
 public:
  FakeDictionaryUtil(async_dispatcher_t* dispatcher)
      : driver_manager::DictionaryUtil(
            fidl::Endpoints<fuchsia_component_sandbox::CapabilityStore>::Create().client,
            dispatcher) {}

  void DictionaryDirConnectorOpen(
      fuchsia_component_sandbox::CapabilityId dictionary, std::string_view key,
      fit::callback<void(zx::result<fidl::ClientEnd<fuchsia_io::Directory>>)> callback) override {
    auto [client, server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
    callback(zx::ok(std::move(client)));
  }

  void CreateDictionaryWith(
      std::unordered_map<std::string, fidl::ClientEnd<fuchsia_component_sandbox::DirReceiver>>
          receivers,
      fit::callback<void(zx::result<fuchsia_component_sandbox::CapabilityId>)> callback) override {
    receivers_ = std::move(receivers);
    if (defer_create_dictionary_) {
      pending_create_dictionary_ = std::move(callback);
      return;
    }
    callback(zx::ok(1234));
  }

  void ImportDictionary(fuchsia_component_sandbox::DictionaryRef dictionary,
                        fit::callback<void(zx::result<fuchsia_component_sandbox::NewCapabilityId>)>
                            callback) override {
    if (defer_import_) {
      pending_import_ = std::move(callback);
      return;
    }
    callback(zx::ok(1234));
  }

  // When true, ImportDictionary stashes the continuation instead of invoking it
  // synchronously, modelling the in-flight CapabilityStore.Import IPC window
  // that exists in production (see DictionaryUtil::ImportDictionaryWire).
  bool defer_import_ = false;
  fit::callback<void(zx::result<fuchsia_component_sandbox::NewCapabilityId>)> pending_import_;

  void ImportDictionaryWire(
      fuchsia_component_sandbox::wire::DictionaryRef dictionary,
      fit::callback<void(zx::result<fuchsia_component_sandbox::wire::NewCapabilityId>)> callback)
      override {
    callback(zx::ok(1234));
  }

  void CopyExportDictionary(
      fuchsia_component_sandbox::CapabilityId dictionary,
      fit::callback<void(zx::result<fuchsia_component_sandbox::DictionaryRef>)> callback) override {
    if (defer_copy_export_) {
      pending_copy_export_ = std::move(callback);
      return;
    }
    callback(zx::ok(fuchsia_component_sandbox::DictionaryRef(zx::eventpair{})));
  }

  // Defer invoking the create-dictionary callback instead of invoking it immediately in
  // `CreateDictionaryWith()`. Set to true by tests.
  bool defer_create_dictionary_ = false;

  // Stores the deferred create-dictionary callback. See `defer_create_dictionary_`.
  fit::callback<void(zx::result<fuchsia_component_sandbox::CapabilityId>)>
      pending_create_dictionary_;

  // Defer invoking the copy-export callback instead of invoking it immediately in
  // `CopyExportDictionary()`. Set to true by tests.
  bool defer_copy_export_ = false;

  // Stores the deferred copy-export callback. See `defer_copy_export_`.
  fit::callback<void(zx::result<fuchsia_component_sandbox::DictionaryRef>)> pending_copy_export_;

  std::unordered_map<std::string, fidl::ClientEnd<fuchsia_component_sandbox::DirReceiver>>
      receivers_;
};

class FakeElementControl final
    : public fidl::testing::WireTestBase<fuchsia_power_broker::ElementControl> {
 public:
  void RegisterDependencyToken(RegisterDependencyTokenRequestView request,
                               RegisterDependencyTokenCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }
  void NotImplemented_(const std::string& name, ::fidl::CompleterBase& completer) override {
    ZX_PANIC("Unimplemented %s", name.c_str());
  }
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementControl> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}
};

class FakeNodeManager : public TestNodeManagerBase {
 public:
  FakeNodeManager(async_dispatcher_t* dispatcher, fidl::WireClient<fuchsia_component::Realm> realm)
      : dispatcher_(dispatcher), realm_(std::move(realm)), dictionary_util_(dispatcher) {}

  zx::result<driver_manager::DriverHost*> CreateDriverHost(
      bool use_next_vdso, std::string_view driver_host_name_for_colocation) override {
    create_driver_host_calls_++;
    hosts_[std::string(driver_host_name_for_colocation)] = &driver_host_;
    return zx::ok(&driver_host_);
  }

  driver_manager::DriverHost* GetDriverHost(std::string_view name) override {
    if (hosts_.count(std::string(name))) {
      return hosts_[std::string(name)];
    }
    return nullptr;
  }

  void CreatePowerElement(
      std::optional<fidl::ClientEnd<fuchsia_power_broker::Topology>> topology_client,
      std::string_view name, fuchsia_power_broker::DependencyToken element_token,
      std::vector<fuchsia_power_broker::DependencyToken> deps,
      fidl::ServerEnd<fuchsia_power_broker::ElementControl> control,
      fidl::ClientEnd<fuchsia_power_broker::ElementRunner> runner,
      fidl::ServerEnd<fuchsia_power_broker::Lessor> lessor,
      driver_manager::Collection for_collection,
      std::optional<fuchsia_power_broker::DependencyToken> cpu_token_override,
      std::optional<zx::eventpair> initial_lease_token, bool is_hermetic_power_test,
      fit::callback<void(zx::result<bool>)> cb) override {
    create_power_element_calls_++;
    last_topology_client_ = std::move(topology_client);
    last_element_token_ = std::move(element_token);
    last_deps_ = std::move(deps);
    last_cpu_token_override_ = std::move(cpu_token_override);
    element_controls_.AddBinding(dispatcher_, std::move(control), &fake_element_control_,
                                 fidl::kIgnoreBindingClosure);
    lessors_.push_back(std::move(lessor));
    runners_.push_back(std::move(runner));
    if (defer_power_element_creation_) {
      power_element_callbacks_.push_back(std::move(cb));
      return;
    }
    cb(zx::ok(true));
  }

  int create_power_element_calls() const { return create_power_element_calls_; }

  std::optional<fidl::ClientEnd<fuchsia_power_broker::Topology>>& last_topology_client() {
    return last_topology_client_;
  }

  const std::optional<fuchsia_power_broker::DependencyToken>& last_element_token() const {
    return last_element_token_;
  }

  const std::vector<fuchsia_power_broker::DependencyToken>& last_deps() const { return last_deps_; }

  const std::optional<fuchsia_power_broker::DependencyToken>& last_cpu_token_override() const {
    return last_cpu_token_override_;
  }

  void CloseDriverForNode(std::string node_name, zx_status_t status = ZX_OK) {
    driver_host_.CloseDriver(node_name, status);
  }

  FakeDriverHost& driver_host() { return driver_host_; }

  void AddClient(const std::string& node_name,
                 fidl::ClientEnd<fuchsia_component_runner::ComponentController> client) {
    stop_listeners_.emplace_back(dispatcher_, std::move(client));
  }

  void StoreComponentHandle(const std::string& node_name,
                            fidl::ServerEnd<fuchsia_component::Controller> controller) {
    controllers_.emplace(
        std::piecewise_construct, std::forward_as_tuple(node_name),
        std::forward_as_tuple(this, node_name, std::move(controller), dispatcher_));
  }

  void RemoveController(const std::string& name) { controllers_.erase(name); }

  driver_manager::DictionaryUtil& dictionary_util() override { return dictionary_util_; }

  int create_driver_host_calls() const { return create_driver_host_calls_; }
  void set_defer_power_element_creation(bool defer) { defer_power_element_creation_ = defer; }

  bool SuspendEnabled() override { return suspend_enabled_; }
  void set_suspend_enabled(bool enabled) { suspend_enabled_ = enabled; }

  void RunPendingPowerElementCallbacks() {
    for (auto& cb : power_element_callbacks_) {
      cb(zx::ok(true));
    }
    power_element_callbacks_.clear();
  }

 private:
  async_dispatcher_t* dispatcher_;
  fidl::WireClient<fuchsia_component::Realm> realm_;
  std::list<StopListener> stop_listeners_;
  std::unordered_map<std::string, TestController> controllers_;
  FakeDriverHost driver_host_;
  FakeDictionaryUtil dictionary_util_;

  int create_driver_host_calls_ = 0;
  int create_power_element_calls_ = 0;
  FakeElementControl fake_element_control_;
  fidl::ServerBindingGroup<fuchsia_power_broker::ElementControl> element_controls_;
  std::vector<fidl::ServerEnd<fuchsia_power_broker::Lessor>> lessors_;
  std::vector<fidl::ClientEnd<fuchsia_power_broker::ElementRunner>> runners_;
  std::unordered_map<std::string, driver_manager::DriverHost*> hosts_;
  bool defer_power_element_creation_ = false;
  bool suspend_enabled_ = false;
  std::vector<fit::callback<void(zx::result<bool>)>> power_element_callbacks_;
  std::optional<fidl::ClientEnd<fuchsia_power_broker::Topology>> last_topology_client_;
  std::optional<fuchsia_power_broker::DependencyToken> last_element_token_;
  std::vector<fuchsia_power_broker::DependencyToken> last_deps_;
  std::optional<fuchsia_power_broker::DependencyToken> last_cpu_token_override_;
};

void TestController::Destroy(DestroyCompleter::Sync& completer) {
  completer.Reply(fit::ok());
  // Post this as a task since the real component manager replies ok to indicate destroy has
  // started, not that it has completed. This will allow driver manager to see the ok response
  // before the controller closing.
  async::PostTask(dispatcher_, [this]() { parent_->RemoveController(name_); });
}

void TestController::Start(StartRequestView request, StartCompleter::Sync& completer) {
  completer.ReplySuccess();
}

class FakeDriver : public fidl::testing::TestBase<fuchsia_driver_host::Driver> {
 public:
  using StopCallback = fit::function<void()>;
  using HealthCheckCallback =
      fit::function<fit::result<fuchsia_driver_framework::HealthCheckError,
                                fuchsia_driver_framework::HealthCheckResponse>(
          const fuchsia_driver_framework::HealthCheckRequest&)>;

  FakeDriver(
      async_dispatcher_t* dispatcher, fidl::ServerEnd<fuchsia_driver_host::Driver> server_end,
      fidl::ClientEnd<fuchsia_driver_framework::Node> node, StopCallback stop_callback = []() {})
      : binding_(async_get_default_dispatcher(), std::move(server_end), this,
                 fidl::kIgnoreBindingClosure),
        node_(std::move(node)),
        stop_callback_(std::move(stop_callback)) {}

  void SetHealthCheckCallback(HealthCheckCallback cb) { health_check_callback_ = std::move(cb); }

  void Stop(StopCompleter::Sync& completer) override {
    stop_callback_();
    if (hang_on_health_check_) {
      return;
    }
    node_.reset();
    binding_.Close(ZX_OK);
  }

  void HealthCheck(HealthCheckRequest& request, HealthCheckCompleter::Sync& completer) override {
    if (close_on_health_check_.has_value()) {
      // Simulates the driver host crashing while the check is in flight: the channel is closed
      // through the binding and the detached completer is dropped without a reply.
      [[maybe_unused]] auto async_completer = completer.ToAsync();
      binding_.Close(*close_on_health_check_);
      return;
    }
    if (hang_on_health_check_) {
      // Simulates a wedged driver host: the request is parked and never answered unless the test
      // calls ReplyToPendingHealthCheck.
      pending_health_check_.emplace(completer.ToAsync());
      return;
    }
    if (health_check_callback_) {
      completer.Reply(health_check_callback_(request.request()));
      return;
    }
    completer.Reply(fit::ok(fuchsia_driver_framework::HealthCheckResponse{}));
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: Driver::%s\n", name.c_str());
  }

  // Makes the next HealthCheck close the Driver channel with the error |epitaph| instead of
  // replying, as the driver host would on a crash.
  void SetCloseOnHealthCheck(zx_status_t epitaph) { close_on_health_check_ = epitaph; }

  // Makes HealthCheck requests hang (never reply) until ReplyToPendingHealthCheck is called, as
  // a wedged driver host would.
  void SetHangOnHealthCheck(bool hang) { hang_on_health_check_ = hang; }

  bool has_pending_health_check() const { return pending_health_check_.has_value(); }

  // Answers the parked HealthCheck with |result|.
  void ReplyToPendingHealthCheck(fit::result<fuchsia_driver_framework::HealthCheckError,
                                             fuchsia_driver_framework::HealthCheckResponse>
                                     result) {
    ASSERT_TRUE(pending_health_check_.has_value());
    pending_health_check_->Reply(std::move(result));
    pending_health_check_.reset();
  }

  zx::result<> SendHealthAlert(const fuchsia_driver_framework::HealthAlert& alert) {
    fidl::Arena arena;
    return zx::make_result(
        fidl::WireSendEvent(binding_)->OnHealthAlert(fidl::ToWire(arena, alert)).status());
  }

 private:
  fidl::ServerBinding<fuchsia_driver_host::Driver> binding_;
  fidl::ClientEnd<fuchsia_driver_framework::Node> node_;
  StopCallback stop_callback_;
  HealthCheckCallback health_check_callback_;
  std::optional<zx_status_t> close_on_health_check_;
  bool hang_on_health_check_ = false;
  std::optional<HealthCheckCompleter::Async> pending_health_check_;
};

struct StartDriverOptions {
  bool host_restart_on_crash = false;
  bool colocate = false;
  bool suspend_enabled = false;
  bool power_managed_dispatchers_enabled = false;
};

class Dfv2NodeTest : public DriverManagerTestBase {
 public:
  void SetUp() override {
    realm_ = std::make_unique<TestRealm>(dispatcher());

    auto client = realm_->Connect();
    node_manager = std::make_unique<FakeNodeManager>(
        dispatcher(), fidl::WireClient<fuchsia_component::Realm>(std::move(client), dispatcher()));
    DriverManagerTestBase::SetUp();
  }

  void StartTestDriver(std::shared_ptr<driver_manager::Node> node,
                       StartDriverOptions options = {}) {
    std::vector<fuchsia_data::DictionaryEntry> program_entries = {
        {{
            .key = "binary",
            .value = std::make_unique<fuchsia_data::DictionaryValue>(
                fuchsia_data::DictionaryValue::WithStr("driver/library.so")),
        }},
        {{
            .key = "colocate",
            .value = std::make_unique<fuchsia_data::DictionaryValue>(
                fuchsia_data::DictionaryValue::WithStr(options.colocate ? "true" : "false")),
        }},
    };

    if (options.host_restart_on_crash) {
      program_entries.emplace_back(fuchsia_data::DictionaryEntry({
          .key = "host_restart_on_crash",
          .value = std::make_unique<fuchsia_data::DictionaryValue>(
              fuchsia_data::DictionaryValue::WithStr("true")),
      }));
    }

    if (options.suspend_enabled) {
      program_entries.emplace_back(fuchsia_data::DictionaryEntry({
          .key = "suspend_enabled",
          .value = std::make_unique<fuchsia_data::DictionaryValue>(
              fuchsia_data::DictionaryValue::WithStr("true")),
      }));
    }

    if (options.power_managed_dispatchers_enabled) {
      program_entries.emplace_back(fuchsia_data::DictionaryEntry({
          .key = "power_managed_dispatchers_enabled",
          .value = std::make_unique<fuchsia_data::DictionaryValue>(
              fuchsia_data::DictionaryValue::WithStr("true")),
      }));
    }

    auto [_, server_end] = fidl::Endpoints<fuchsia_io::Directory>::Create();

    auto start_info = fuchsia_component_runner::ComponentStartInfo{{
        .resolved_url = "fuchsia-boot:///#meta/test-driver.cm",
        .program = fuchsia_data::Dictionary{{.entries = std::move(program_entries)}},
        .outgoing_dir = std::move(server_end),
    }};

    auto component_controller_endpoints =
        fidl::Endpoints<fuchsia_component_runner::ComponentController>::Create();
    node_manager->AddClient(node->name(), std::move(component_controller_endpoints.client));

    auto controller_endpoints = fidl::Endpoints<fuchsia_component::Controller>::Create();
    node_manager->StoreComponentHandle(node->name(), std::move(controller_endpoints.server));
    node->SetController(std::move(controller_endpoints.client));

    fidl::Arena arena;
    node->StartDriver(fidl::ToWire(arena, std::move(start_info)),
                      std::move(component_controller_endpoints.server),
                      [node](zx::result<> result) { node->CompleteBind(result); });
    RunLoopUntilIdle();
  }

 protected:
  fidl::WireClient<fuchsia_device::Controller> ConnectToDeviceController(
      std::shared_ptr<driver_manager::Node> node) {
    zx::result device_controller_endpoints = fidl::CreateEndpoints<fuchsia_device::Controller>();
    EXPECT_EQ(device_controller_endpoints.status_value(), ZX_OK);
    device_controller_bindings_.AddBinding(dispatcher(),
                                           std::move(device_controller_endpoints->server),
                                           node.get(), fidl::kIgnoreBindingClosure);
    return {std::move(device_controller_endpoints->client), dispatcher()};
  }

  void StartTestDriverWithStartInfo(const std::shared_ptr<driver_manager::Node>& node,
                                    fuchsia_component_runner::wire::ComponentStartInfo start_info) {
    auto component_controller_endpoints =
        fidl::Endpoints<fuchsia_component_runner::ComponentController>::Create();
    node_manager->AddClient(node->name(), std::move(component_controller_endpoints.client));

    auto controller_endpoints = fidl::Endpoints<fuchsia_component::Controller>::Create();
    node_manager->StoreComponentHandle(node->name(), std::move(controller_endpoints.server));
    node->SetController(std::move(controller_endpoints.client));

    node->StartDriver(std::move(start_info), std::move(component_controller_endpoints.server),
                      [node](zx::result<> result) { node->CompleteBind(result); });
    RunLoopUntilIdle();
  }

  driver_manager::NodeManager* GetNodeManager() override { return node_manager.get(); }

  std::unique_ptr<FakeNodeManager> node_manager;

 private:
  std::unique_ptr<TestRealm> realm_;
  fidl::ServerBindingGroup<fuchsia_device::Controller> device_controller_bindings_;
};

TEST_F(Dfv2NodeTest, StartDriverWithNamespaceTopology) {
  auto node = CreateNode("test");

  node_manager->set_suspend_enabled(true);

  driver_runner::TestDirectory svc_dir(dispatcher());
  svc_dir.SetDirents({fidl::DiscoverableProtocolName<fuchsia_power_broker::Topology>});

  auto [svc_client, svc_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  svc_dir.Bind(std::move(svc_server));

  fidl::Arena arena;
  fidl::VectorView<fuchsia_component_runner::wire::ComponentNamespaceEntry> ns(arena, 1);
  ns[0] = fuchsia_component_runner::wire::ComponentNamespaceEntry::Builder(arena)
              .path("/svc")
              .directory(std::move(svc_client))
              .Build();

  fidl::VectorView<fuchsia_data::wire::DictionaryEntry> entries(arena, 1);
  entries[0] = {
      .key = fidl::StringView(arena, "suspend_enabled"),
      .value = fuchsia_data::wire::DictionaryValue::WithStr(arena, "true"),
  };
  auto program = fuchsia_data::wire::Dictionary::Builder(arena).entries(entries).Build();
  auto start_info = fuchsia_component_runner::wire::ComponentStartInfo::Builder(arena)
                        .resolved_url("fuchsia-boot:///#meta/test-driver.cm")
                        .ns(ns)
                        .program(program)
                        .Build();

  printf("Test: ns[0].directory is_valid: %d\n", start_info.ns()[0].directory().is_valid());

  StartTestDriverWithStartInfo(node, std::move(start_info));

  RunLoopUntilIdle();
  ASSERT_TRUE(node_manager->last_topology_client().has_value());
}

TEST_F(Dfv2NodeTest, StartDriverWithPowerDependencyOverrides) {
  auto node = CreateNode("test");

  zx::event cpu_token;
  ASSERT_EQ(zx::event::create(0, &cpu_token), ZX_OK);
  zx::event cpu_token_copy;
  ASSERT_EQ(cpu_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &cpu_token_copy), ZX_OK);
  node->SetCpuTokenOverride(std::move(cpu_token_copy));

  zx::event dep_token;
  ASSERT_EQ(zx::event::create(0, &dep_token), ZX_OK);
  zx::event dep_token_copy;
  ASSERT_EQ(dep_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &dep_token_copy), ZX_OK);

  std::vector<fuchsia_power_broker::LevelDependency> overrides;
  overrides.push_back(fuchsia_power_broker::LevelDependency{{
      .dependent_level = 1,
      .requires_token = std::move(dep_token_copy),
      .requires_level_by_preference = std::vector<uint8_t>{1},
  }});
  node->SetPowerDependencyOverrides(std::move(overrides));

  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(node->IsHermeticPowerTest());
  StartTestDriver(node, {.suspend_enabled = true});
  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(node->HasActivePowerElement());

  ASSERT_EQ(1u, node_manager->last_deps().size());
  zx_info_handle_basic_t info;
  ASSERT_EQ(node_manager->last_deps()[0].get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                                  nullptr, nullptr),
            ZX_OK);
  zx_info_handle_basic_t expected_info;
  ASSERT_EQ(dep_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info), nullptr,
                               nullptr),
            ZX_OK);
  ASSERT_EQ(info.koid, expected_info.koid);

  ASSERT_TRUE(node_manager->last_cpu_token_override().has_value());
  ASSERT_EQ(node_manager->last_cpu_token_override()->get_info(ZX_INFO_HANDLE_BASIC, &info,
                                                              sizeof(info), nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(cpu_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info), nullptr,
                               nullptr),
            ZX_OK);
  ASSERT_EQ(info.koid, expected_info.koid);
}

TEST_F(Dfv2NodeTest, NodeTokenOverridesConstruction) {
  zx::event override_token;
  ASSERT_EQ(zx::event::create(0, &override_token), ZX_OK);
  zx::event override_token_copy;
  ASSERT_EQ(override_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &override_token_copy), ZX_OK);

  zx_info_handle_basic_t expected_info;
  ASSERT_EQ(override_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info),
                                    nullptr, nullptr),
            ZX_OK);

  std::map<std::string, zx::event> overrides;
  overrides.emplace("test_node", std::move(override_token_copy));

  auto node = CreateNode("test_node", std::move(overrides));
  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(node->IsHermeticPowerTest());
  StartTestDriver(node, {.suspend_enabled = true});
  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(node->HasActivePowerElement());

  ASSERT_TRUE(node_manager->last_element_token().has_value());
  zx_info_handle_basic_t node_token_info;
  ASSERT_EQ(node_manager->last_element_token()->get_info(ZX_INFO_HANDLE_BASIC, &node_token_info,
                                                         sizeof(node_token_info), nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(node_token_info.koid, expected_info.koid);
}

TEST_F(Dfv2NodeTest, StartDriverRaceCondition) {
  auto parent = CreateNode("parent");
  StartTestDriver(parent);

  fuchsia_driver_framework::NodeAddArgs args;
  args.name() = "child1";
  args.driver_host() = "shared-host";

  zx::result node_controller_endpoints =
      fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  ASSERT_EQ(node_controller_endpoints.status_value(), ZX_OK);

  zx::result node_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
  ASSERT_EQ(node_endpoints.status_value(), ZX_OK);

  std::shared_ptr<driver_manager::Node> child1;
  parent->AddChild(std::move(args), std::move(node_controller_endpoints->server),
                   std::move(node_endpoints->server),
                   [&child1](fit::result<fuchsia_driver_framework::wire::NodeError,
                                         std::shared_ptr<driver_manager::Node>>
                                 result) {
                     ASSERT_TRUE(result.is_ok());
                     child1 = std::move(result.value());
                   });

  fuchsia_driver_framework::NodeAddArgs args2;
  args2.name() = "child2";
  args2.driver_host() = "shared-host";

  zx::result node_controller_endpoints2 =
      fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  ASSERT_EQ(node_controller_endpoints2.status_value(), ZX_OK);

  zx::result node_endpoints2 = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
  ASSERT_EQ(node_endpoints2.status_value(), ZX_OK);

  std::shared_ptr<driver_manager::Node> child2;
  parent->AddChild(std::move(args2), std::move(node_controller_endpoints2->server),
                   std::move(node_endpoints2->server),
                   [&child2](fit::result<fuchsia_driver_framework::wire::NodeError,
                                         std::shared_ptr<driver_manager::Node>>
                                 result) {
                     ASSERT_TRUE(result.is_ok());
                     child2 = std::move(result.value());
                   });

  ASSERT_TRUE(child1);
  ASSERT_TRUE(child2);

  node_manager->set_suspend_enabled(true);
  node_manager->set_defer_power_element_creation(true);
  int initial_calls = node_manager->create_driver_host_calls();

  // Start both drivers.
  StartTestDriver(child1, {.suspend_enabled = true});
  StartTestDriver(child2, {.suspend_enabled = true});

  // Both should be waiting on power element creation.
  ASSERT_EQ(initial_calls, node_manager->create_driver_host_calls());

  // Run the callbacks.
  node_manager->RunPendingPowerElementCallbacks();
  RunLoopUntilIdle();

  // Only one driver host should be created.
  ASSERT_EQ(initial_calls + 1, node_manager->create_driver_host_calls());
}

TEST_F(Dfv2NodeTest, AddChildWithDuplicatePropertyKey) {
  auto node = CreateNode("test");
  StartTestDriver(node);
  ASSERT_TRUE(node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, node->GetNodeState());

  // Add child with two properties that share the same key. It should fail.
  zx::result node_controller_endpoints =
      fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  ASSERT_EQ(node_controller_endpoints.status_value(), ZX_OK);

  zx::result node_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
  ASSERT_EQ(node_endpoints.status_value(), ZX_OK);

  fuchsia_driver_framework::NodeAddArgs args{
      {.name = "child",
       .properties2 = {{fdf::MakeProperty2("key", "value"), fdf::MakeProperty2("key", "value2")}}}};
  node->AddChild(std::move(args), std::move(node_controller_endpoints->server),
                 std::move(node_endpoints->server),
                 [](fit::result<fuchsia_driver_framework::wire::NodeError,
                                std::shared_ptr<driver_manager::Node>>
                        result) {
                   ASSERT_TRUE(result.is_error());
                   ASSERT_EQ(result.error_value(),
                             fuchsia_driver_framework::wire::NodeError::kDuplicatePropertyKeys);
                 });
}

TEST_F(Dfv2NodeTest, RemoveDuringFailedBind) {
  auto node = CreateNode("test");
  StartTestDriver(node);
  ASSERT_TRUE(node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, node->GetNodeState());

  node->Remove(driver_manager::RemovalSet::kAll, nullptr);
  RunLoopUntilIdle();

  ASSERT_EQ(driver_manager::NodeState::kWaitingOnDriver, node->GetNodeState());

  node->CompleteBind(zx::error(ZX_ERR_NOT_FOUND));
  RunLoopUntilIdle();
  ASSERT_FALSE(node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kDestroyed, node->GetNodeState());
}

TEST_F(Dfv2NodeTest, TestEvaluateRematchFlags) {
  auto node = CreateNode("plain");
  ASSERT_FALSE(node->EvaluateRematchFlags(
      fuchsia_driver_development::RestartRematchFlags::kRequested, "some-url"));
  ASSERT_TRUE(
      node->EvaluateRematchFlags(fuchsia_driver_development::RestartRematchFlags::kRequested |
                                     fuchsia_driver_development::RestartRematchFlags::kNonRequested,
                                 "some-url"));

  auto parent_1 = CreateNode("p1");
  auto parent_2 = CreateNode("p2");

  auto composite = CreateCompositeNode("composite", {parent_1, parent_2}, {{}, {}},
                                       /* primary_index */ 0);

  ASSERT_FALSE(composite->EvaluateRematchFlags(
      fuchsia_driver_development::RestartRematchFlags::kRequested |
          fuchsia_driver_development::RestartRematchFlags::kNonRequested,
      "some-url"));
}

TEST_F(Dfv2NodeTest, RemoveCompositeNodeForRebind) {
  auto parent_node_1 = CreateNode("parent_1");
  StartTestDriver(parent_node_1);
  ASSERT_TRUE(parent_node_1->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node_1->GetNodeState());

  auto parent_node_2 = CreateNode("parent_2");
  StartTestDriver(parent_node_2);
  ASSERT_TRUE(parent_node_2->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node_2->GetNodeState());

  auto composite = CreateCompositeNode("composite", {parent_node_1, parent_node_2}, {{}, {}});
  StartTestDriver(composite);
  ASSERT_TRUE(composite->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, composite->GetNodeState());

  ASSERT_EQ(1u, parent_node_1->children().size());
  ASSERT_EQ(1u, parent_node_2->children().size());

  auto remove_callback_succeeded = false;
  composite->RemoveCompositeNodeForRebind([&remove_callback_succeeded](zx::result<> result) {
    if (result.is_ok()) {
      remove_callback_succeeded = true;
    }
  });
  RunLoopUntilIdle();
  ASSERT_EQ(driver_manager::NodeState::kWaitingOnDriver, composite->GetNodeState());
  ASSERT_EQ(driver_manager::ShutdownIntent::kRebindComposite, composite->shutdown_intent());

  node_manager->CloseDriverForNode("composite");
  RunLoopUntilIdle();
  ASSERT_TRUE(remove_callback_succeeded);

  ASSERT_EQ(driver_manager::NodeState::kDestroyed, composite->GetNodeState());
}

// Verify that we receives a callback for composite rebind if the node is deallocated
// before shutdown is complete.
TEST_F(Dfv2NodeTest, RemoveCompositeNodeForRebind_Dealloc) {
  auto parent_node_1 = CreateNode("parent_1");
  StartTestDriver(parent_node_1);
  ASSERT_TRUE(parent_node_1->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node_1->GetNodeState());

  auto parent_node_2 = CreateNode("parent_2");
  StartTestDriver(parent_node_2);
  ASSERT_TRUE(parent_node_2->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node_2->GetNodeState());

  auto composite = CreateCompositeNode("composite", {parent_node_1, parent_node_2}, {{}, {}});
  StartTestDriver(composite);
  ASSERT_TRUE(composite->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, composite->GetNodeState());

  ASSERT_EQ(1u, parent_node_1->children().size());
  ASSERT_EQ(1u, parent_node_2->children().size());

  bool is_cancelled = false;
  composite->RemoveCompositeNodeForRebind([&is_cancelled](zx::result<> result) {
    if (result.is_error()) {
      is_cancelled = result.error_value() == ZX_ERR_CANCELED;
    }
  });
  RunLoopUntilIdle();
  ASSERT_EQ(driver_manager::NodeState::kWaitingOnDriver, composite->GetNodeState());
  ASSERT_EQ(driver_manager::ShutdownIntent::kRebindComposite, composite->shutdown_intent());

  parent_node_1.reset();
  parent_node_2.reset();
  composite.reset();
  RunLoopUntilIdle();
  ASSERT_TRUE(is_cancelled);
}

TEST_F(Dfv2NodeTest, RestartOnCrashComposite) {
  auto parent_node_1 = CreateNode("parent_1");
  StartTestDriver(parent_node_1);
  ASSERT_TRUE(parent_node_1->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node_1->GetNodeState());

  auto parent_node_2 = CreateNode("parent_2");
  StartTestDriver(parent_node_2);
  ASSERT_TRUE(parent_node_2->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node_2->GetNodeState());

  auto composite = CreateCompositeNode("composite", {parent_node_1, parent_node_2}, {{}, {}});
  StartTestDriver(composite, {.host_restart_on_crash = true});

  ASSERT_TRUE(composite->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, composite->GetNodeState());

  ASSERT_EQ(1u, parent_node_1->children().size());
  ASSERT_EQ(1u, parent_node_2->children().size());

  // Simulate a crash by closing the driver side of channels.
  node_manager->CloseDriverForNode("composite", ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();

  // The node manager should NOT have been asked to reboot the system.
  ASSERT_FALSE(node_manager->reboot_system_called_);

  // The node should come back to running state.
  ASSERT_EQ(driver_manager::NodeState::kRunning, composite->GetNodeState());
}

TEST_F(Dfv2NodeTest, RebootOnCrash) {
  auto parent_node = CreateNode("parent");
  StartTestDriver(parent_node, {.host_restart_on_crash = false});
  ASSERT_TRUE(parent_node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node->GetNodeState());

  // Simulate a crash by closing the driver side of channels.
  node_manager->CloseDriverForNode("parent", ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();

  // The node manager should have been asked to reboot the system.
  ASSERT_TRUE(node_manager->reboot_system_called_);
}

TEST_F(Dfv2NodeTest, NoRebootOnCleanClose) {
  auto parent_node = CreateNode("parent");
  StartTestDriver(parent_node, {.host_restart_on_crash = false});
  ASSERT_TRUE(parent_node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node->GetNodeState());

  // Simulate a clean channel closure (non-crash) by closing with ZX_OK.
  node_manager->CloseDriverForNode("parent", ZX_OK);
  RunLoopUntilIdle();

  // The node manager should NOT have been asked to reboot the system.
  ASSERT_FALSE(node_manager->reboot_system_called_);
}

TEST_F(Dfv2NodeTest, NoRebootOnCanceledClose) {
  auto parent_node = CreateNode("parent");
  StartTestDriver(parent_node, {.host_restart_on_crash = false});
  ASSERT_TRUE(parent_node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node->GetNodeState());

  // Simulate a local cancellation (non-crash) by closing with ZX_ERR_CANCELED.
  node_manager->CloseDriverForNode("parent", ZX_ERR_CANCELED);
  RunLoopUntilIdle();

  // The node manager should NOT have been asked to reboot the system.
  ASSERT_FALSE(node_manager->reboot_system_called_);
}

TEST_F(Dfv2NodeTest, ColocatedChildCrashNoReboot) {
  auto parent_node = CreateNode("parent");
  StartTestDriver(parent_node, {.host_restart_on_crash = false, .colocate = false});
  ASSERT_TRUE(parent_node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, parent_node->GetNodeState());

  auto child_node = CreateNode("child", parent_node);
  StartTestDriver(child_node, {.host_restart_on_crash = false, .colocate = true});
  ASSERT_TRUE(child_node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, child_node->GetNodeState());

  // Simulate a crash of the child driver by closing the child's channel with an error.
  node_manager->CloseDriverForNode("child", ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();

  // The node manager should NOT have been asked to reboot the system.
  ASSERT_FALSE(node_manager->reboot_system_called_);
}

TEST_F(Dfv2NodeTest, TestCompositeNodeProperties) {
  const char* kParent1Name = "parent-1";
  const std::vector<fuchsia_driver_framework::NodeProperty2> kParent1NodeProperties{
      fdf::MakeProperty2("test-key-1", 2u)};
  const char* kParent2Name = "parent-2";
  const std::vector<fuchsia_driver_framework::NodeProperty2> kParent2NodeProperties{
      fdf::MakeProperty2("test-key-2", "test=value")};
  auto parent_1 = CreateNode(kParent1Name);
  parent_1->InitializeSelfResource(kParent1NodeProperties);
  auto parent_1_self = parent_1->GetSelfResource();
  ASSERT_TRUE(parent_1_self.has_value());
  EXPECT_TRUE(parent_1_self.value()->is_self_resource());

  auto parent_2 = CreateNode(kParent2Name);
  parent_2->InitializeSelfResource(kParent2NodeProperties);
  auto parent_2_self = parent_2->GetSelfResource();
  ASSERT_TRUE(parent_2_self.has_value());
  EXPECT_TRUE(parent_2_self.value()->is_self_resource());

  std::vector<fuchsia_driver_framework::NodePropertyEntry2> parent_properties;
  parent_properties.emplace_back(kParent1Name, kParent1NodeProperties);
  parent_properties.emplace_back(kParent2Name, kParent2NodeProperties);
  auto composite = CreateCompositeNode("composite", {parent_1, parent_2}, parent_properties,
                                       /* primary_index */ 0);

  // Verify primary parent properties. Primary parent should be parent 1.
  const auto& primary_parent_node_properties = composite->GetNodeProperties();
  ASSERT_TRUE(primary_parent_node_properties.has_value());
  ASSERT_EQ(1ul, primary_parent_node_properties->size());

  const auto& primary_parent_node_property_1 = primary_parent_node_properties.value()[0];
  ASSERT_EQ(kParent1NodeProperties[0].key(), primary_parent_node_property_1.key());
  ASSERT_EQ(kParent1NodeProperties[0].value().int_value().value(),
            primary_parent_node_property_1.value().int_value().value());

  // Verify parent 1 properties.
  const auto& parent_1_node_properties = composite->GetNodeProperties(kParent1Name);
  ASSERT_TRUE(parent_1_node_properties.has_value());
  ASSERT_EQ(1ul, parent_1_node_properties->size());

  const auto& parent_1_node_property_1 = parent_1_node_properties.value()[0];
  ASSERT_EQ(kParent1NodeProperties[0].key(), parent_1_node_property_1.key());
  ASSERT_TRUE(parent_1_node_property_1.value().int_value().has_value());
  ASSERT_EQ(kParent1NodeProperties[0].value().int_value().value(),
            parent_1_node_property_1.value().int_value().value());

  // Verify parent 2 properties.
  const auto& parent_2_node_properties = composite->GetNodeProperties(kParent2Name);
  ASSERT_TRUE(parent_2_node_properties.has_value());
  ASSERT_EQ(1ul, parent_2_node_properties->size());

  const auto& parent_2_node_property_1 = parent_2_node_properties.value()[0];
  ASSERT_EQ(kParent2NodeProperties[0].key(), parent_2_node_property_1.key());
  ASSERT_TRUE(parent_2_node_property_1.value().string_value().has_value());
  ASSERT_EQ(kParent2NodeProperties[0].value().string_value().value(),
            parent_2_node_property_1.value().string_value().value());
}

// Verify Node::UnbindChildren() unbinds all of the children of a node with zero child.
TEST_F(Dfv2NodeTest, UnbindChildrenZeroChildren) {
  auto parent = CreateNode("parent");
  auto device_controller = ConnectToDeviceController(parent);

  ASSERT_TRUE(parent->children().empty());
  device_controller->UnbindChildren().ThenExactlyOnce(
      [](auto& result) { ASSERT_EQ(result.status(), ZX_OK); });
  ASSERT_TRUE(RunLoopUntilIdle());
  ASSERT_TRUE(parent->children().empty());
}

// Verify Node::UnbindChildren() unbinds all of the children of a node with one child.
TEST_F(Dfv2NodeTest, UnbindChildrenOneChild) {
  auto parent = CreateNode("parent");
  auto child = CreateNode("child", parent);
  auto device_controller = ConnectToDeviceController(parent);

  ASSERT_EQ(parent->children().size(), 1u);
  device_controller->UnbindChildren().ThenExactlyOnce(
      [](auto& result) { ASSERT_EQ(result.status(), ZX_OK); });
  ASSERT_TRUE(RunLoopUntilIdle());
  ASSERT_TRUE(parent->children().empty());
}

// Verify Node::UnbindChildren() unbinds all of the children of a node with one child that has a
// driver bound to it.
TEST_F(Dfv2NodeTest, UnbindChildrenOneBoundChild) {
  const std::string kChildNodeName = "child";

  auto parent = CreateNode("parent");
  auto child = CreateNode(kChildNodeName, parent);
  StartTestDriver(child);
  ASSERT_TRUE(child->HasDriverComponent());
  auto device_controller = ConnectToDeviceController(parent);

  // Get the driver so that the test can properly close the driver's connection when the driver
  // receives a Stop fidl request.
  auto [driver_server, node_client] = node_manager->driver_host().TakeDriver(kChildNodeName);
  FakeDriver driver{dispatcher(), std::move(driver_server), std::move(node_client)};

  ASSERT_EQ(parent->children().size(), 1u);
  device_controller->UnbindChildren().ThenExactlyOnce(
      [](auto& result) { ASSERT_EQ(result.status(), ZX_OK); });
  ASSERT_TRUE(RunLoopUntilIdle());
  ASSERT_TRUE(parent->children().empty());
}

// Verify Node::UnbindChildren() unbinds all of the children of a node with four children.
TEST_F(Dfv2NodeTest, UnbindChildrenFourChildren) {
  const size_t kNumChildren = 4;

  auto parent = CreateNode("parent");
  std::vector<std::shared_ptr<driver_manager::Node>> children;
  for (size_t i = 0; i < kNumChildren; ++i) {
    children.emplace_back(CreateNode("child-" + std::to_string(i), parent));
  }
  auto device_controller = ConnectToDeviceController(parent);

  ASSERT_EQ(parent->children().size(), kNumChildren);
  device_controller->UnbindChildren().ThenExactlyOnce(
      [](auto& result) { ASSERT_EQ(result.status(), ZX_OK); });
  ASSERT_TRUE(RunLoopUntilIdle());
  ASSERT_TRUE(parent->children().empty());
}

// Verify that multiple requests to Node::UnbindChildren() will succeed. Both of these requests
// are sent before the node can complete either.
TEST_F(Dfv2NodeTest, UnbindChildrenMultipleCalls) {
  auto parent = CreateNode("parent");
  auto child = CreateNode("child", parent);
  auto device_controller = ConnectToDeviceController(parent);

  ASSERT_EQ(parent->children().size(), 1u);
  size_t unbind_children_complete_count = 0;
  device_controller->UnbindChildren().ThenExactlyOnce(
      [&unbind_children_complete_count](auto& result) {
        ASSERT_EQ(result.status(), ZX_OK);
        unbind_children_complete_count += 1;
      });
  device_controller->UnbindChildren().ThenExactlyOnce(
      [&unbind_children_complete_count](auto& result) {
        ASSERT_EQ(result.status(), ZX_OK);
        unbind_children_complete_count += 1;
      });
  ASSERT_TRUE(RunLoopUntilIdle());
  ASSERT_EQ(unbind_children_complete_count, 2u);
  ASSERT_TRUE(parent->children().empty());
}

// Verify that Node::AddChild() fails when a node is in the middle of unbinding children.
// TODO(https://fxbug.dev/333783189): Re-enable flaky test case.
TEST_F(Dfv2NodeTest, DISABLED_UnbindChildrenFailAddChild) {
  const std::string kChildNode1Name = "child-1";

  auto parent = CreateNode("parent");
  auto child_1 = CreateNode(kChildNode1Name, parent);
  StartTestDriver(child_1);
  ASSERT_TRUE(child_1->HasDriverComponent());
  auto device_controller = ConnectToDeviceController(parent);

  // Get the driver so that the test can prevent Node::UnbindChildren() from fully completing by
  // pausing TestDriver::Stop(). The driver will live on a separate thread in order to not block
  // the main thread while the driver waits to complete stopping.
  async::Loop driver_loop{&kAsyncLoopConfigNoAttachToCurrentThread};
  ASSERT_EQ(driver_loop.StartThread("driver"), ZX_OK);
  auto [driver_server, node_client] = node_manager->driver_host().TakeDriver(kChildNode1Name);
  auto complete_stop = std::make_shared<libsync::Completion>();
  async_patterns::TestDispatcherBound<FakeDriver> driver{
      driver_loop.dispatcher(),       std::in_place,
      async_patterns::PassDispatcher, std::move(driver_server),
      std::move(node_client),         [complete_stop]() {
        complete_stop->Wait(); }};

  ASSERT_EQ(parent->children().size(), 1u);
  device_controller->UnbindChildren().ThenExactlyOnce(
      [](auto& result) { ASSERT_EQ(result.status(), ZX_OK); });

  ASSERT_TRUE(RunLoopUntilIdle());
  // At this point, the node has received the UnbindChildren request and is waiting for child-1's
  // driver to fully stop.

  // Fail to add a second child.
  zx::result node_controller_endpoints =
      fidl::CreateEndpoints<fuchsia_driver_framework::NodeController>();
  ASSERT_EQ(node_controller_endpoints.status_value(), ZX_OK);
  zx::result node_endpoints = fidl::CreateEndpoints<fuchsia_driver_framework::Node>();
  ASSERT_EQ(node_endpoints.status_value(), ZX_OK);
  fuchsia_driver_framework::NodeAddArgs args{{.name = "child-2"}};
  parent->AddChild(std::move(args), std::move(node_controller_endpoints->server),
                   std::move(node_endpoints->server),
                   [](fit::result<fuchsia_driver_framework::wire::NodeError,
                                  std::shared_ptr<driver_manager::Node>>
                          result) {
                     ASSERT_TRUE(result.is_error());
                     ASSERT_EQ(
                         result.error_value(),
                         fuchsia_driver_framework::wire::NodeError::kUnbindChildrenInProgress);
                   });

  // Let the driver complete stopping.
  complete_stop->Signal();
  // Wait for the driver to complete stopping.
  driver.SyncCall([](FakeDriver* driver) {});

  // Let Node::UnbindChildren() complete.
  ASSERT_TRUE(RunLoopUntilIdle());

  ASSERT_TRUE(parent->children().empty());
}

// Verify `Node::ScheduleUnbind` will unbind a node that is bound to a driver.
TEST_F(Dfv2NodeTest, ScheduleUnbind) {
  const std::string kNodeName = "test";

  auto node = CreateNode(kNodeName);
  StartTestDriver(node);
  ASSERT_TRUE(node->HasDriverComponent());

  // Get the driver so that the test can properly close the driver's connection when the driver
  // receives a Stop fidl request.
  auto [driver_server, node_client] = node_manager->driver_host().TakeDriver(kNodeName);
  FakeDriver driver{dispatcher(), std::move(driver_server), std::move(node_client)};

  auto device_controller = ConnectToDeviceController(node);
  device_controller->ScheduleUnbind().ThenExactlyOnce(
      [](auto& result) { ASSERT_EQ(result.status(), ZX_OK); });
  RunLoopUntilIdle();
  ASSERT_FALSE(node->HasDriverComponent());
}

TEST_F(Dfv2NodeTest, PrepareDictionaryComposite) {
  auto node = CreateNode("test");
  StartTestDriver(node);
  ASSERT_TRUE(node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, node->GetNodeState());

  // Parent 1
  auto [controller1, server1] = fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node1, node_server1] = fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  fuchsia_driver_framework::NodeAddArgs args_p1;
  args_p1.name("parent_1");

  fuchsia_component_decl::OfferService service_decl_1;
  service_decl_1.source_name("service_1");
  service_decl_1.target_name("service_1");
  service_decl_1.renamed_instances(std::vector<::fuchsia_component_decl::NameMapping>{
      fuchsia_component_decl::NameMapping("default", "default")});
  service_decl_1.source_instance_filter(std::vector<std::string>{"default"});

  fuchsia_driver_framework::Offer offer_1_fidl =
      fuchsia_driver_framework::Offer::WithDictionaryOffer(
          fuchsia_component_decl::Offer::WithService(service_decl_1));

  args_p1.offers2(std::vector{std::move(offer_1_fidl)});
  args_p1.offers_dictionary(fuchsia_component_sandbox::DictionaryRef{zx::eventpair{}});

  std::shared_ptr<driver_manager::Node> parent_node_1;
  node->AddChild(
      std::move(args_p1), std::move(server1), std::move(node_server1),
      [&](fit::result<fuchsia_driver_framework::NodeError, std::shared_ptr<driver_manager::Node>>
              result) {
        if (result.is_error()) {
          fdf_log::info("result {}", result.error_value());
        }

        ASSERT_TRUE(result.is_ok());
        parent_node_1 = result.value();
      });

  RunLoopUntilIdle();

  // Parent 2
  auto [controller2, server2] = fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node2, node_server2] = fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  fuchsia_driver_framework::NodeAddArgs args_p2;
  args_p2.name("parent_2");

  fuchsia_component_decl::OfferService service_decl_2;
  service_decl_2.source_name("service_2");
  service_decl_2.target_name("service_2");
  service_decl_2.renamed_instances(std::vector<::fuchsia_component_decl::NameMapping>{
      fuchsia_component_decl::NameMapping("default", "default")});
  service_decl_2.source_instance_filter(std::vector<std::string>{"default"});

  fuchsia_driver_framework::Offer offer_2_fidl =
      fuchsia_driver_framework::Offer::WithDictionaryOffer(
          fuchsia_component_decl::Offer::WithService(service_decl_2));

  args_p2.offers2(std::vector{std::move(offer_2_fidl)});
  args_p2.offers_dictionary(fuchsia_component_sandbox::DictionaryRef{zx::eventpair{}});

  std::shared_ptr<driver_manager::Node> parent_node_2;
  node->AddChild(
      std::move(args_p2), std::move(server2), std::move(node_server2),
      [&](fit::result<fuchsia_driver_framework::NodeError, std::shared_ptr<driver_manager::Node>>
              result) {
        if (result.is_error()) {
          fdf_log::info("result {}", result.error_value());
        }
        ASSERT_TRUE(result.is_ok());
        parent_node_2 = result.value();
      });

  RunLoopUntilIdle();

  ASSERT_TRUE(parent_node_1);
  ASSERT_TRUE(parent_node_2);

  // Create Composite
  auto composite = CreateCompositeNode("composite", {parent_node_1, parent_node_2}, {{}, {}});

  // Verify composite has offers
  ASSERT_FALSE(composite->offers().empty());

  // Call PrepareDictionary
  bool callback_called = false;
  composite->PrepareDictionary([&](zx::result<> result) {
    ASSERT_TRUE(result.is_ok());
    callback_called = true;
  });

  RunLoopUntilIdle();
  ASSERT_TRUE(callback_called);

  // Verify that CreateDictionaryWith was called with correct receivers
  auto& util = static_cast<FakeNodeManager*>(GetNodeManager())->dictionary_util();
  auto& fake_util = static_cast<FakeDictionaryUtil&>(util);

  ASSERT_EQ(fake_util.receivers_.size(), 2u);
  ASSERT_TRUE(fake_util.receivers_.count("service_1"));
  ASSERT_TRUE(fake_util.receivers_.count("service_2"));
}

// Verify that if a parent node is freed before the asynchronous dictionary import
// for its child completes, it fails gracefully and does not cause a use-after-free.
TEST_F(Dfv2NodeTest, AddChildOffersDictionaryParentFreedBeforeImportReply) {
  bool destroyed = false;
  std::shared_ptr<driver_manager::Node> parent(
      new driver_manager::Node("P", root(), GetNodeManager(), dispatcher()),
      [&destroyed](driver_manager::Node* p) {
        delete p;
        destroyed = true;
      });
  parent->AddToDevfsForTesting(root_devnode());
  parent->devfs_device().publish();
  std::weak_ptr<driver_manager::Node> weak_parent = parent;

  auto& fake_util = static_cast<FakeDictionaryUtil&>(node_manager->dictionary_util());
  fake_util.defer_import_ = true;

  // NodeAddArgs with a DictionaryOffer + offers_dictionary token, and no
  // fdf::Node server end so that child creation takes the Bind path.
  fuchsia_driver_framework::NodeAddArgs args;
  args.name("C");
  fuchsia_component_decl::OfferService svc;
  svc.source_name("svc");
  svc.target_name("svc");
  svc.renamed_instances(std::vector<fuchsia_component_decl::NameMapping>{
      fuchsia_component_decl::NameMapping("default", "default")});
  svc.source_instance_filter(std::vector<std::string>{"default"});
  args.offers2(std::vector{fuchsia_driver_framework::Offer::WithDictionaryOffer(
      fuchsia_component_decl::Offer::WithService(svc))});
  args.offers_dictionary(fuchsia_component_sandbox::DictionaryRef{zx::eventpair{}});

  bool result_seen = false;
  parent->AddChild(
      std::move(args), /*controller=*/{}, /*node=*/{},
      [&](fit::result<fuchsia_driver_framework::NodeError, std::shared_ptr<driver_manager::Node>>) {
        result_seen = true;
      });
  RunLoopUntilIdle();

  // ImportDictionary was deferred, so child creation is still pending.
  ASSERT_FALSE(result_seen);
  ASSERT_TRUE(static_cast<bool>(fake_util.pending_import_));

  // Free the parent.
  parent.reset();
  RunLoopUntilIdle();
  ASSERT_TRUE(weak_parent.expired());
  ASSERT_TRUE(destroyed);

  // Invoke the deferred continuation.
  auto cb = std::move(fake_util.pending_import_);
  cb(zx::error(ZX_ERR_INTERNAL));
  RunLoopUntilIdle();
}

TEST_F(Dfv2NodeTest, ProvideAndRemoveResource) {
  auto node = CreateNode("test-node");

  auto [node_client_end, node_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::Node>::Create();
  auto binding = fidl::BindServer(dispatcher(), std::move(node_server_end), node);

  fidl::WireClient<fuchsia_driver_framework::Node> node_client(std::move(node_client_end),
                                                               dispatcher());

  auto endpoints = fidl::Endpoints<fuchsia_driver_framework::ResourceController>::Create();

  fidl::Arena arena;
  fuchsia_component_decl::OfferService service_decl;
  service_decl.source_name("service");
  service_decl.target_name("service");
  service_decl.renamed_instances(std::vector<::fuchsia_component_decl::NameMapping>{
      fuchsia_component_decl::NameMapping("default", "default")});
  service_decl.source_instance_filter(std::vector<std::string>{"default"});

  fuchsia_driver_framework::Offer natural_offer =
      fuchsia_driver_framework::Offer::WithZirconTransport(
          fuchsia_component_decl::Offer::WithService(service_decl));
  auto offers = fidl::ToWire(arena, std::vector{natural_offer});

  auto resource_args =
      fuchsia_driver_framework::wire::ResourceArgs::Builder(arena)
          .name("test-resource")
          .properties(fidl::VectorView<fuchsia_driver_framework::wire::NodeProperty2>())
          .offers(offers)
          .Build();

  bool called = false;
  node_client->ProvideResource(resource_args, std::move(endpoints.server))
      .Then([&called](auto& result) {
        ASSERT_TRUE(result.ok());
        ASSERT_FALSE(result->is_error());
        called = true;
      });
  RunLoopUntilIdle();
  ASSERT_TRUE(called);

  ASSERT_EQ(node->provided_resources().size(), 1u);
  EXPECT_EQ(node->provided_resources()[0]->name(), "test-resource");
  EXPECT_FALSE(node->provided_resources()[0]->is_self_resource());
  ASSERT_EQ(node->provided_resources()[0]->offers().size(), 1u);
  EXPECT_EQ(node->provided_resources()[0]->offers()[0].service_name, "service");
  ASSERT_EQ(node->provided_resources()[0]->properties().size(), 1u);
  EXPECT_EQ(node->provided_resources()[0]->properties()[0].key(), "fuchsia.Service");
  EXPECT_EQ(node->provided_resources()[0]->properties()[0].value().string_value().value(),
            "service");

  auto owner = node->provided_resources()[0]->owner().lock();
  ASSERT_NE(owner, nullptr);
  EXPECT_EQ(owner, node);

  endpoints.client.reset();
  RunLoopUntilIdle();

  ASSERT_EQ(node->provided_resources().size(), 0u);

  binding.Unbind();
  RunLoopUntilIdle();
}

TEST_F(Dfv2NodeTest, ProvideResourceInvalidArgs) {
  auto node = CreateNode("test-node");

  auto [node_client_end, node_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::Node>::Create();
  auto binding = fidl::BindServer(dispatcher(), std::move(node_server_end), node);

  fidl::WireClient<fuchsia_driver_framework::Node> node_client(std::move(node_client_end),
                                                               dispatcher());

  auto endpoints = fidl::Endpoints<fuchsia_driver_framework::ResourceController>::Create();

  fidl::Arena arena;
  // Missing properties and offers.
  auto resource_args =
      fuchsia_driver_framework::wire::ResourceArgs::Builder(arena).name("test-resource").Build();

  bool called = false;
  node_client->ProvideResource(resource_args, std::move(endpoints.server))
      .Then([&called](auto& result) {
        ASSERT_TRUE(result.ok());
        ASSERT_TRUE(result->is_error());
        EXPECT_EQ(result->error_value(), fuchsia_driver_framework::NodeError::kUnsupportedArgs);
        called = true;
      });
  RunLoopUntilIdle();
  ASSERT_TRUE(called);

  ASSERT_EQ(node->provided_resources().size(), 0u);

  binding.Unbind();
  RunLoopUntilIdle();
}
TEST_F(Dfv2NodeTest, ProvideResourceInvalidOffer) {
  auto node = CreateNode("test-node");

  auto [node_client_end, node_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::Node>::Create();
  auto binding = fidl::BindServer(dispatcher(), std::move(node_server_end), node);

  fidl::WireClient<fuchsia_driver_framework::Node> node_client(std::move(node_client_end),
                                                               dispatcher());

  auto endpoints = fidl::Endpoints<fuchsia_driver_framework::ResourceController>::Create();

  fidl::Arena arena;
  fuchsia_component_decl::OfferService service_decl;
  // Missing source_name will fail offer validation.
  fuchsia_driver_framework::Offer natural_offer =
      fuchsia_driver_framework::Offer::WithZirconTransport(
          fuchsia_component_decl::Offer::WithService(service_decl));
  auto offers = fidl::ToWire(arena, std::vector{natural_offer});

  auto resource_args =
      fuchsia_driver_framework::wire::ResourceArgs::Builder(arena)
          .name("test-resource")
          .properties(fidl::VectorView<fuchsia_driver_framework::wire::NodeProperty2>())
          .offers(offers)
          .Build();

  bool called = false;
  node_client->ProvideResource(resource_args, std::move(endpoints.server))
      .Then([&called](auto& result) {
        ASSERT_TRUE(result.ok());
        ASSERT_TRUE(result->is_error());
        EXPECT_EQ(result->error_value(),
                  fuchsia_driver_framework::NodeError::kOfferSourceNameMissing);
        called = true;
      });
  RunLoopUntilIdle();
  ASSERT_TRUE(called);

  ASSERT_EQ(node->provided_resources().size(), 0u);

  binding.Unbind();
  RunLoopUntilIdle();
}

TEST_F(Dfv2NodeTest, CompositeNodeResourceDependents) {
  const char* kParent1Name = "parent-1";
  const std::vector<fuchsia_driver_framework::NodeProperty2> kParent1NodeProperties{
      fdf::MakeProperty2("test-key-1", 2u)};
  const char* kParent2Name = "parent-2";
  const std::vector<fuchsia_driver_framework::NodeProperty2> kParent2NodeProperties{
      fdf::MakeProperty2("test-key-2", "test=value")};
  auto parent_1 = CreateNode(kParent1Name);
  parent_1->InitializeSelfResource(kParent1NodeProperties);
  auto parent_1_self = parent_1->GetSelfResource();
  ASSERT_TRUE(parent_1_self.has_value());

  auto parent_2 = CreateNode(kParent2Name);
  parent_2->InitializeSelfResource(kParent2NodeProperties);
  auto parent_2_self = parent_2->GetSelfResource();
  ASSERT_TRUE(parent_2_self.has_value());

  std::vector<fuchsia_driver_framework::NodePropertyEntry2> parent_properties;
  parent_properties.emplace_back(kParent1Name, kParent1NodeProperties);
  parent_properties.emplace_back(kParent2Name, kParent2NodeProperties);

  auto parent_1_resource = parent_1_self.value();
  auto parent_2_resource = parent_2_self.value();

  // Initially, before composite is created, they should have no dependents.
  EXPECT_TRUE(parent_1_resource->dependents().empty());
  EXPECT_TRUE(parent_2_resource->dependents().empty());

  // Create composite.
  auto composite = CreateCompositeNode("composite", {parent_1, parent_2}, parent_properties,
                                       /* primary_index */ 0);

  // Now, both parent resources should have the composite node as a dependent.
  ASSERT_EQ(parent_1_resource->dependents().size(), 1u);
  EXPECT_EQ(parent_1_resource->dependents()[0].lock(), composite);

  ASSERT_EQ(parent_2_resource->dependents().size(), 1u);
  EXPECT_EQ(parent_2_resource->dependents()[0].lock(), composite);

  // Reset our shared_ptr to composite.
  composite.reset();

  // Bind the parent_1_resource to a controller so we can remove it.
  auto [resource_client_end, resource_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::ResourceController>::Create();
  parent_1_resource->Bind(std::move(resource_server_end));
  fidl::WireClient<fuchsia_driver_framework::ResourceController> resource_client(
      std::move(resource_client_end), dispatcher());

  // Remove the resource.
  auto result = resource_client->Remove();
  ASSERT_TRUE(result.ok());
  RunLoopUntilIdle();

  // The composite node should now be destroyed, and it should have removed itself from the
  // resources' dependents list.
  EXPECT_TRUE(parent_1_resource->dependents().empty());
  EXPECT_TRUE(parent_2_resource->dependents().empty());
}

// Verify that if a node is freed before the asynchronous dictionary preparation completes
// (non-composite path), it fails gracefully and does not cause a use-after-free.
TEST_F(Dfv2NodeTest, PrepDictNonCompFreeBeforeExportReply) {
  bool destroyed = false;
  std::shared_ptr<driver_manager::Node> node(
      new driver_manager::Node("N", root(), GetNodeManager(), dispatcher()),
      [&destroyed](driver_manager::Node* n) {
        delete n;
        destroyed = true;
      });
  node->AddToDevfsForTesting(root_devnode());
  node->devfs_device().publish();
  std::weak_ptr<driver_manager::Node> weak_node = node;

  // Prevent the copy-export callback that references `node` from being invoked immediately.
  auto& fake_util = static_cast<FakeDictionaryUtil&>(node_manager->dictionary_util());
  fake_util.defer_copy_export_ = true;

  // Set dictionary_ref_ to trigger CopyExportDictionary which will invoke the copy-export callback
  // that references `node`.
  node->set_collection(driver_manager::Collection::kNone);
  node->SetSubtreeDictionaryRef(fuchsia_component_sandbox::CapabilityId{1234});

  // Prepare the dictionary, resulting in a copy-export callback being created.
  bool result_seen = false;
  node->PrepareDictionary([&](zx::result<> result) { result_seen = true; });
  RunLoopUntilIdle();

  // Verify that the preparation has not completed because we deferred invoking the copy-export
  // callback.
  ASSERT_FALSE(result_seen);
  ASSERT_TRUE(static_cast<bool>(fake_util.pending_copy_export_));

  // Free the node before the copy-export callback is called.
  node.reset();
  RunLoopUntilIdle();
  ASSERT_TRUE(weak_node.expired());
  ASSERT_TRUE(destroyed);

  // Call the copy-export callback. This will panic if it tries to reference the freed `node`.
  auto cb = std::move(fake_util.pending_copy_export_);
  cb(zx::error(ZX_ERR_INTERNAL));
  RunLoopUntilIdle();
}

// Verify that if a composite node is freed before the asynchronous dictionary preparation
// completes (composite path, CreateDictionaryWith deferred), it fails gracefully.
TEST_F(Dfv2NodeTest, PrepDictCompFreeBeforeCreateDictReply) {
  auto node = CreateNode("test");
  StartTestDriver(node);
  ASSERT_TRUE(node->HasDriverComponent());
  ASSERT_EQ(driver_manager::NodeState::kRunning, node->GetNodeState());

  // Add parent nodes.

  // Parent 1
  auto [controller1, server1] = fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node1, node_server1] = fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  fuchsia_driver_framework::NodeAddArgs args_p1;
  args_p1.name("parent_1");

  fuchsia_component_decl::OfferService service_decl_1;
  service_decl_1.source_name("service_1");
  service_decl_1.target_name("service_1");
  service_decl_1.renamed_instances(std::vector<::fuchsia_component_decl::NameMapping>{
      fuchsia_component_decl::NameMapping("default", "default")});
  service_decl_1.source_instance_filter(std::vector<std::string>{"default"});

  fuchsia_driver_framework::Offer offer_1_fidl =
      fuchsia_driver_framework::Offer::WithDictionaryOffer(
          fuchsia_component_decl::Offer::WithService(service_decl_1));

  args_p1.offers2(std::vector{std::move(offer_1_fidl)});
  args_p1.offers_dictionary(fuchsia_component_sandbox::DictionaryRef{zx::eventpair{}});

  std::shared_ptr<driver_manager::Node> parent_node_1;
  node->AddChild(
      std::move(args_p1), std::move(server1), std::move(node_server1),
      [&](fit::result<fuchsia_driver_framework::NodeError, std::shared_ptr<driver_manager::Node>>
              result) {
        ASSERT_TRUE(result.is_ok());
        parent_node_1 = result.value();
      });

  RunLoopUntilIdle();

  // Parent 2
  auto [controller2, server2] = fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node2, node_server2] = fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  fuchsia_driver_framework::NodeAddArgs args_p2;
  args_p2.name("parent_2");

  fuchsia_component_decl::OfferService service_decl_2;
  service_decl_2.source_name("service_2");
  service_decl_2.target_name("service_2");
  service_decl_2.renamed_instances(std::vector<::fuchsia_component_decl::NameMapping>{
      fuchsia_component_decl::NameMapping("default", "default")});
  service_decl_2.source_instance_filter(std::vector<std::string>{"default"});

  fuchsia_driver_framework::Offer offer_2_fidl =
      fuchsia_driver_framework::Offer::WithDictionaryOffer(
          fuchsia_component_decl::Offer::WithService(service_decl_2));

  args_p2.offers2(std::vector{std::move(offer_2_fidl)});
  args_p2.offers_dictionary(fuchsia_component_sandbox::DictionaryRef{zx::eventpair{}});

  std::shared_ptr<driver_manager::Node> parent_node_2;
  node->AddChild(
      std::move(args_p2), std::move(server2), std::move(node_server2),
      [&](fit::result<fuchsia_driver_framework::NodeError, std::shared_ptr<driver_manager::Node>>
              result) {
        ASSERT_TRUE(result.is_ok());
        parent_node_2 = result.value();
      });

  RunLoopUntilIdle();

  ASSERT_TRUE(parent_node_1);
  ASSERT_TRUE(parent_node_2);

  std::shared_ptr<driver_manager::Node> composite =
      CreateCompositeNode("composite", {parent_node_1, parent_node_2}, {{}, {}});
  std::weak_ptr<driver_manager::Node> weak_composite = composite;

  // Prevent the create-dictionary callback that references `composite` from being invoked
  // immediately.
  auto& fake_util = static_cast<FakeDictionaryUtil&>(node_manager->dictionary_util());
  fake_util.defer_create_dictionary_ = true;

  // Prepare the dictionary for the composite node, resulting in a create-dictionary callback being
  // created.
  bool result_seen = false;
  composite->PrepareDictionary([&](zx::result<> result) { result_seen = true; });
  RunLoopUntilIdle();

  // Verify that the preparation has not completed because we deferred invoking the
  // create-dictionary callback.
  ASSERT_FALSE(result_seen);
  ASSERT_TRUE(static_cast<bool>(fake_util.pending_create_dictionary_));

  // Free the composite.
  composite->Remove(driver_manager::RemovalSet::kAll, nullptr);
  composite.reset();
  RunLoopUntilIdle();
  ASSERT_TRUE(weak_composite.expired());

  // Invoke the create-dictionary callback. This will panic if it tries to reference the freed
  // `composite`.
  auto cb = std::move(fake_util.pending_create_dictionary_);
  cb(zx::error(ZX_ERR_INTERNAL));
  RunLoopUntilIdle();
}

TEST_F(Dfv2NodeTest, RunHealthCheckUnboundAndBound) {
  auto node = CreateNode("test-hc");

  // 1. Unbound node returns kInvalidState.
  auto unbound_req = fuchsia_driver_framework::HealthCheckRequest::kFast;
  bool unbound_called = false;
  node->RunHealthCheck(unbound_req, [&unbound_called](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kInvalidState, result.error_value());
    unbound_called = true;
  });
  EXPECT_TRUE(unbound_called);

  // 2. Start driver and bind FakeDriver to handle HealthCheck RPCs.
  StartTestDriver(node);
  auto [driver_server_end, node_client_end] = node_manager->driver_host().TakeDriver("test-hc");
  FakeDriver fake_driver(dispatcher(), std::move(driver_server_end), std::move(node_client_end));
  fake_driver.SetHealthCheckCallback(
      [](const fuchsia_driver_framework::HealthCheckRequest& req)
          -> fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse> {
        if (req == fuchsia_driver_framework::HealthCheckRequest::kDeep) {
          return fit::error(fuchsia_driver_framework::HealthCheckError::kNotSupported);
        }
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{{
            .conditions = {{{{
                .status = fuchsia_driver_framework::HealthStatus::kDegraded,
                .reason_code = "NODE_TEST_WARN",
            }}}},
        }});
      });

  bool fast_called = false;
  node->RunHealthCheck(unbound_req, [&fast_called](auto result) {
    ASSERT_TRUE(result.is_ok());
    ASSERT_EQ(1u, result.value().conditions()->size());
    EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kDegraded,
              result.value().conditions()->at(0).status());
    EXPECT_EQ("NODE_TEST_WARN", result.value().conditions()->at(0).reason_code());
    fast_called = true;
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(fast_called);

  auto deep_req = fuchsia_driver_framework::HealthCheckRequest::kDeep;
  bool deep_called = false;
  node->RunHealthCheck(deep_req, [&deep_called](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kNotSupported, result.error_value());
    deep_called = true;
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(deep_called);
}

TEST_F(Dfv2NodeTest, RunHealthCheckCachesRecordAndTimeout) {
  auto node = CreateNode("test-hc-cache");
  EXPECT_FALSE(node->last_health_check().has_value());

  StartTestDriver(node);
  auto [driver_server_end, node_client_end] =
      node_manager->driver_host().TakeDriver("test-hc-cache");
  FakeDriver fake_driver(dispatcher(), std::move(driver_server_end), std::move(node_client_end));
  fake_driver.SetHealthCheckCallback(
      [](const fuchsia_driver_framework::HealthCheckRequest& req)
          -> fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse> {
        if (req == fuchsia_driver_framework::HealthCheckRequest::kPing) {
          return fit::error(fuchsia_driver_framework::HealthCheckError::kTimeout);
        }
        if (req == fuchsia_driver_framework::HealthCheckRequest::kDeep) {
          return fit::error(fuchsia_driver_framework::HealthCheckError::kNotSupported);
        }
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{{
            .diagnostics =
                std::vector<fuchsia_driver_framework::HealthDiagnosticItem>{
                    {{.key = "str_k",
                      .value =
                          fuchsia_driver_framework::HealthDiagnosticValue::WithStringValue("val")}},
                    {{.key = "int_k",
                      .value = fuchsia_driver_framework::HealthDiagnosticValue::WithIntValue(-3)}},
                    {{.key = "uint_k",
                      .value = fuchsia_driver_framework::HealthDiagnosticValue::WithUintValue(7u)}},
                    {{.key = "bool_k",
                      .value =
                          fuchsia_driver_framework::HealthDiagnosticValue::WithBoolValue(true)}},
                    {{.key = "ts_k",
                      .value = fuchsia_driver_framework::HealthDiagnosticValue::WithTimestamp(
                          zx::time_boot(1234))}},
                },
            .conditions =
                std::vector<fuchsia_driver_framework::HealthCondition>{
                    {{
                        .status = fuchsia_driver_framework::HealthStatus::kDegraded,
                        .reason_code = "CACHE_WARN",
                        .message = "Cached warning",
                        .first_observed = zx::time_boot(10),
                        .last_observed = zx::time_boot(20),
                        .occurrence_count = 2,
                    }},
                    {{
                        .status = fuchsia_driver_framework::HealthStatus::kOk,
                        .reason_code = "CACHE_INFO",
                    }},
                },
        }});
      });

  auto fast_req = fuchsia_driver_framework::HealthCheckRequest::kFast;
  bool fast_done = false;
  node->RunHealthCheck(fast_req, [&fast_done](auto result) {
    ASSERT_TRUE(result.is_ok());
    fast_done = true;
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(fast_done);

  ASSERT_TRUE(node->last_health_check().has_value());
  const auto& rec = node->last_health_check().value();
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kDegraded, rec.status);
  EXPECT_FALSE(rec.error.has_value());
  EXPECT_EQ(Now(), rec.timestamp);
  ASSERT_EQ(5u, rec.diagnostics.size());
  EXPECT_EQ("str_k", rec.diagnostics[0].key());
  EXPECT_EQ("val", rec.diagnostics[0].value().string_value().value());
  EXPECT_EQ(-3, rec.diagnostics[1].value().int_value().value());
  EXPECT_EQ(7u, rec.diagnostics[2].value().uint_value().value());
  EXPECT_EQ(true, rec.diagnostics[3].value().bool_value().value());
  EXPECT_EQ(1234, rec.diagnostics[4].value().timestamp().value().get());
  // Both conditions are cached verbatim.
  ASSERT_EQ(2u, rec.conditions.size());
  EXPECT_EQ("CACHE_WARN", rec.conditions[0].reason_code());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kDegraded, rec.conditions[0].status());
  EXPECT_EQ("Cached warning", rec.conditions[0].message());
  EXPECT_EQ(10, rec.conditions[0].first_observed().value().get());
  EXPECT_EQ(20, rec.conditions[0].last_observed().value().get());
  EXPECT_EQ(2u, rec.conditions[0].occurrence_count());
  EXPECT_EQ("CACHE_INFO", rec.conditions[1].reason_code());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kOk, rec.conditions[1].status());

  // A timeout error is cached with a synthetic DISPATCHER_TIMEOUT condition.
  auto ping_req = fuchsia_driver_framework::HealthCheckRequest::kPing;
  bool ping_done = false;
  node->RunHealthCheck(ping_req, [&ping_done](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kTimeout, result.error_value());
    ping_done = true;
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(ping_done);

  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_FALSE(node->last_health_check()->status.has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kTimeout, node->last_health_check()->error);
  ASSERT_EQ(1u, node->last_health_check()->conditions.size());
  EXPECT_EQ("DISPATCHER_TIMEOUT", node->last_health_check()->conditions[0].reason_code());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy,
            node->last_health_check()->conditions[0].status());
  EXPECT_EQ("Driver dispatcher timed out responding to HealthCheck",
            node->last_health_check()->conditions[0].message());
  EXPECT_TRUE(node->last_health_check()->diagnostics.empty());

  // A NotSupported error (e.g. from legacy DFv1 compat drivers) carries no information about the
  // driver's health, so it does not replace the informative TIMEOUT record.
  auto deep_req = fuchsia_driver_framework::HealthCheckRequest::kDeep;
  bool deep_done = false;
  node->RunHealthCheck(deep_req, [&deep_done](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kNotSupported, result.error_value());
    deep_done = true;
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(deep_done);

  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kTimeout, node->last_health_check()->error);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kPing, node->last_health_check()->level);
  EXPECT_EQ("DISPATCHER_TIMEOUT", node->last_health_check()->conditions[0].reason_code());

  // A DEGRADED FAST result replaces the TIMEOUT, and an OK PING afterwards does not clear it: a
  // liveness ping says nothing about the hardware.
  fast_done = false;
  node->RunHealthCheck(fast_req, [&fast_done](auto result) {
    ASSERT_TRUE(result.is_ok());
    fast_done = true;
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(fast_done);
  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kDegraded, node->last_health_check()->status);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kFast, node->last_health_check()->level);

  fake_driver.SetHealthCheckCallback(
      [](const fuchsia_driver_framework::HealthCheckRequest& req)
          -> fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse> {
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{});
      });
  ping_done = false;
  node->RunHealthCheck(ping_req, [&ping_done](auto result) {
    ASSERT_TRUE(result.is_ok());
    ping_done = true;
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(ping_done);
  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kDegraded, node->last_health_check()->status);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kFast, node->last_health_check()->level);
  EXPECT_EQ("CACHE_WARN", node->last_health_check()->conditions[0].reason_code());

  // Starting a new driver component on the node resets its cached health check record.
  node_manager->RemoveController("test-hc-cache");
  RunLoopUntilIdle();
  StartTestDriver(node);
  EXPECT_FALSE(node->last_health_check().has_value());
}

TEST_F(Dfv2NodeTest, RunHealthCheckDriverHostCrashWhileInFlight) {
  auto node = CreateNode("test-hc-crash");
  StartTestDriver(node);
  auto [driver_server_end, node_client_end] =
      node_manager->driver_host().TakeDriver("test-hc-crash");
  FakeDriver fake_driver(dispatcher(), std::move(driver_server_end), std::move(node_client_end));
  // The driver host crashes (error epitaph) with the check in flight.
  fake_driver.SetCloseOnHealthCheck(ZX_ERR_INTERNAL);

  bool done = false;
  node->RunHealthCheck(fuchsia_driver_framework::HealthCheckRequest::kFast, [&done](auto result) {
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kInternalError, result.error_value());
    done = true;
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(done);

  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy, node->last_health_check()->status);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kFast, node->last_health_check()->level);
  EXPECT_EQ("DRIVER_HOST_PEER_CLOSED", node->last_health_check()->conditions[0].reason_code());
}

TEST_F(Dfv2NodeTest, HealthCheckBackstopDeadlineMirrorsDriverHost) {
  using driver_manager::Node;
  // The backstop is the driver host's per-level deadline (250ms / 2s / 2min) plus slack, so a
  // check that the host times out itself normally reports the host's TIMEOUT, not the backstop.
  EXPECT_EQ(zx::msec(250) + Node::kHealthCheckBackstopSlack,
            Node::HealthCheckBackstopDeadline(fuchsia_driver_framework::HealthCheckRequest::kPing));
  EXPECT_EQ(zx::sec(2) + Node::kHealthCheckBackstopSlack,
            Node::HealthCheckBackstopDeadline(fuchsia_driver_framework::HealthCheckRequest::kFast));
  EXPECT_EQ(zx::sec(120) + Node::kHealthCheckBackstopSlack,
            Node::HealthCheckBackstopDeadline(fuchsia_driver_framework::HealthCheckRequest::kDeep));
  // Unknown levels get the longest deadline.
  EXPECT_EQ(zx::sec(120) + Node::kHealthCheckBackstopSlack,
            Node::HealthCheckBackstopDeadline(
                static_cast<fuchsia_driver_framework::HealthCheckRequest>(0xffff)));
}

TEST_F(Dfv2NodeTest, RunHealthCheckBackstopTimesOutWedgedDriverHost) {
  using driver_manager::Node;
  auto node = CreateNode("test-hc-wedged");
  StartTestDriver(node);
  auto [driver_server_end, node_client_end] =
      node_manager->driver_host().TakeDriver("test-hc-wedged");
  FakeDriver fake_driver(dispatcher(), std::move(driver_server_end), std::move(node_client_end));
  // The driver host process is wedged: it receives the request but never answers it.
  fake_driver.SetHangOnHealthCheck(true);

  auto fast_req = fuchsia_driver_framework::HealthCheckRequest::kFast;
  int calls = 0;
  std::optional<fit::result<fuchsia_driver_framework::HealthCheckError,
                            fuchsia_driver_framework::HealthCheckResponse>>
      outcome;
  node->RunHealthCheck(fast_req, [&calls, &outcome](auto result) {
    ++calls;
    outcome = std::move(result);
  });
  RunLoopUntilIdle();
  EXPECT_TRUE(fake_driver.has_pending_health_check());
  EXPECT_EQ(0, calls);
  EXPECT_FALSE(node->last_health_check().has_value());

  // Nothing happens until the backstop deadline for the level has elapsed.
  RunLoopFor(Node::HealthCheckBackstopDeadline(fast_req) - zx::msec(1));
  EXPECT_EQ(0, calls);
  EXPECT_FALSE(node->last_health_check().has_value());

  RunLoopFor(zx::msec(1));
  ASSERT_EQ(1, calls);
  ASSERT_TRUE(outcome->is_error());
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kTimeout, outcome->error_value());

  // The timeout is cached like any other failure, with a synthetic condition naming the host.
  ASSERT_TRUE(node->last_health_check().has_value());
  const auto& rec = node->last_health_check().value();
  EXPECT_FALSE(rec.status.has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kTimeout, rec.error);
  EXPECT_EQ(fast_req, rec.level);
  EXPECT_EQ(Now(), rec.timestamp);
  ASSERT_EQ(1u, rec.conditions.size());
  EXPECT_EQ("DRIVER_HOST_TIMEOUT", rec.conditions[0].reason_code());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy, rec.conditions[0].status());

  // The host wakes up and answers the stale request. The reply is dropped: the callback is not
  // invoked a second time and the cached TIMEOUT record stands.
  fake_driver.ReplyToPendingHealthCheck(fit::ok(fuchsia_driver_framework::HealthCheckResponse{{
      .conditions = {{{{
          .status = fuchsia_driver_framework::HealthStatus::kDegraded,
          .reason_code = "LATE_REPLY",
      }}}},
  }}));
  RunLoopUntilIdle();
  EXPECT_EQ(1, calls);
  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kTimeout, node->last_health_check()->error);
  EXPECT_EQ("DRIVER_HOST_TIMEOUT", node->last_health_check()->conditions[0].reason_code());

  // A check the host answers in time completes with the host's result, and the backstop that was
  // armed for it is a no-op when it later fires.
  fake_driver.SetHangOnHealthCheck(false);
  int ping_calls = 0;
  node->RunHealthCheck(fuchsia_driver_framework::HealthCheckRequest::kPing,
                       [&ping_calls](auto result) {
                         ++ping_calls;
                         EXPECT_TRUE(result.is_ok());
                       });
  RunLoopUntilIdle();
  EXPECT_EQ(1, ping_calls);
  RunLoopFor(
      Node::HealthCheckBackstopDeadline(fuchsia_driver_framework::HealthCheckRequest::kDeep));
  EXPECT_EQ(1, ping_calls);
  EXPECT_EQ(1, calls);
}

TEST_F(Dfv2NodeTest, RunHealthCheckBackstopIsNoOpAfterDriverHostCrash) {
  using driver_manager::Node;
  auto node = CreateNode("test-hc-wedged-crash");
  StartTestDriver(node);
  auto [driver_server_end, node_client_end] =
      node_manager->driver_host().TakeDriver("test-hc-wedged-crash");
  auto fake_driver = std::make_unique<FakeDriver>(dispatcher(), std::move(driver_server_end),
                                                  std::move(node_client_end));
  fake_driver->SetHangOnHealthCheck(true);

  auto ping_req = fuchsia_driver_framework::HealthCheckRequest::kPing;
  int calls = 0;
  node->RunHealthCheck(ping_req, [&calls](auto result) {
    ++calls;
    ASSERT_TRUE(result.is_error());
    EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kInternalError, result.error_value());
  });
  RunLoopUntilIdle();
  EXPECT_EQ(0, calls);

  // The wedged host dies before the backstop fires: the channel closure completes the check, and
  // the backstop that fires afterwards must neither complete it again nor overwrite the record.
  fake_driver.reset();
  RunLoopUntilIdle();
  EXPECT_EQ(1, calls);
  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_EQ("DRIVER_HOST_PEER_CLOSED", node->last_health_check()->conditions[0].reason_code());

  RunLoopFor(Node::HealthCheckBackstopDeadline(ping_req));
  EXPECT_EQ(1, calls);
  EXPECT_EQ("DRIVER_HOST_PEER_CLOSED", node->last_health_check()->conditions[0].reason_code());
}

TEST_F(Dfv2NodeTest, RunHealthCheckBackstopWhileShuttingDownReturnsInvalidState) {
  using driver_manager::Node;
  auto node = CreateNode("test-hc-wedged-stopping");
  StartTestDriver(node);
  auto [driver_server_end, node_client_end] =
      node_manager->driver_host().TakeDriver("test-hc-wedged-stopping");
  FakeDriver fake_driver(dispatcher(), std::move(driver_server_end), std::move(node_client_end));
  fake_driver.SetHangOnHealthCheck(true);

  auto ping_req = fuchsia_driver_framework::HealthCheckRequest::kPing;
  int calls = 0;
  std::optional<fit::result<fuchsia_driver_framework::HealthCheckError,
                            fuchsia_driver_framework::HealthCheckResponse>>
      outcome;
  node->RunHealthCheck(ping_req, [&calls, &outcome](auto result) {
    ++calls;
    outcome = std::move(result);
  });
  RunLoopUntilIdle();
  EXPECT_EQ(0, calls);

  // Begin removing the node while the driver host is wedged (so Stop() does not close the channel
  // immediately). When the backstop fires on a node that is no longer running, it completes with
  // kInvalidState and does not cache a spurious DRIVER_HOST_TIMEOUT record.
  node->Remove(driver_manager::RemovalSet::kAll, nullptr);
  RunLoopUntilIdle();
  ASSERT_FALSE(node->is_running());

  RunLoopFor(Node::HealthCheckBackstopDeadline(ping_req));
  ASSERT_EQ(1, calls);
  ASSERT_TRUE(outcome->is_error());
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kInvalidState, outcome->error_value());
  EXPECT_FALSE(node->last_health_check().has_value());
}

// A clean ZX_OK close while a check is in flight is deliberately not tested: the C++ FIDL client
// cannot represent a two-way call cut short by a ZX_OK epitaph (IncomingHeaderAndMessage asserts
// on a ZX_OK failure status in debug builds), which is why driver-host answers every in-flight
// check with INVALID_STATE before it closes the channel. The branch in Node::RunHealthCheck that
// skips caching for a clean close is only a defensive fallback.

TEST_F(Dfv2NodeTest, RunHealthCheckPingFailureDoesNotLaunderHardwareFault) {
  auto node = CreateNode("test-hc-launder");
  StartTestDriver(node);
  auto [driver_server_end, node_client_end] =
      node_manager->driver_host().TakeDriver("test-hc-launder");
  FakeDriver fake_driver(dispatcher(), std::move(driver_server_end), std::move(node_client_end));

  // FAST reports a hardware fault; PING is driven by |ping_result| below.
  fit::result<fuchsia_driver_framework::HealthCheckError,
              fuchsia_driver_framework::HealthCheckResponse>
      ping_result = fit::error(fuchsia_driver_framework::HealthCheckError::kTimeout);
  fake_driver.SetHealthCheckCallback(
      [&ping_result](const fuchsia_driver_framework::HealthCheckRequest& req)
          -> fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse> {
        if (req == fuchsia_driver_framework::HealthCheckRequest::kPing) {
          return ping_result;
        }
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{{
            .conditions = {{{{
                .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
                .reason_code = "PHY_UNLOCKED",
            }}}},
        }});
      });

  auto run = [&](fuchsia_driver_framework::HealthCheckRequest level) {
    bool done = false;
    node->RunHealthCheck(level, [&done](auto) { done = true; });
    RunLoopUntilIdle();
    EXPECT_TRUE(done);
  };

  run(fuchsia_driver_framework::HealthCheckRequest::kFast);
  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy, node->last_health_check()->status);

  // A PING timeout is the more urgent news, so it is published...
  run(fuchsia_driver_framework::HealthCheckRequest::kPing);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckError::kTimeout, node->last_health_check()->error);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kPing, node->last_health_check()->level);

  // ...but once the dispatcher answers again, the hardware fault from the FAST check is published
  // again rather than being replaced by the OK liveness ping.
  ping_result = fit::ok(fuchsia_driver_framework::HealthCheckResponse{});
  run(fuchsia_driver_framework::HealthCheckRequest::kPing);
  ASSERT_TRUE(node->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy, node->last_health_check()->status);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kFast, node->last_health_check()->level);
  ASSERT_EQ(1u, node->last_health_check()->conditions.size());
  EXPECT_EQ("PHY_UNLOCKED", node->last_health_check()->conditions[0].reason_code());

  // A later OK FAST check clears it, after which an OK PING is published as-is.
  fake_driver.SetHealthCheckCallback(
      [](const fuchsia_driver_framework::HealthCheckRequest& req)
          -> fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse> {
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{});
      });
  run(fuchsia_driver_framework::HealthCheckRequest::kFast);
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kOk, node->last_health_check()->status);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kFast, node->last_health_check()->level);
  run(fuchsia_driver_framework::HealthCheckRequest::kPing);
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kOk, node->last_health_check()->status);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kPing, node->last_health_check()->level);
}

TEST_F(Dfv2NodeTest, OnHealthAlertSelfFaultRecordsDetails) {
  auto child = CreateNode("child-sf-details");
  StartTestDriver(child);
  auto [child_server, child_node_client] =
      node_manager->driver_host().TakeDriver("child-sf-details");
  FakeDriver child_driver(dispatcher(), std::move(child_server), std::move(child_node_client));

  ASSERT_EQ(ZX_OK, child_driver
                       .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithSelfFault(
                           fuchsia_driver_framework::HealthCheckResponse{{
                               .conditions = {{{{
                                   .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
                                   .reason_code = "FW_CRASH",
                                   .message = "Firmware crashed",
                               }}}},
                           }}))
                       .status_value());
  RunLoopUntilIdle();

  ASSERT_TRUE(child->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy, child->last_health_check()->status);
  ASSERT_EQ(1u, child->last_health_check()->conditions.size());
  EXPECT_EQ("FW_CRASH", child->last_health_check()->first_reason_code());
  EXPECT_EQ("Firmware crashed", child->last_health_check()->conditions[0].message());

  // A self_fault alert with no conditions derives status kOk (recovery).
  ASSERT_EQ(ZX_OK, child_driver
                       .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithSelfFault(
                           fuchsia_driver_framework::HealthCheckResponse{}))
                       .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kOk, child->last_health_check()->status);
  EXPECT_TRUE(child->last_health_check()->conditions.empty());
}

TEST_F(Dfv2NodeTest, OnHealthAlertUpstreamNeighborFaultCompositeParents) {
  auto bus_a = CreateNode("bus-a");
  auto bus_b = CreateNode("bus-b");
  auto owned_b = CreateNode("owned-b", bus_b);
  auto [owned_b_client, owned_b_server] = fidl::Endpoints<fuchsia_driver_framework::Node>::Create();
  owned_b->SetOwnedByParent(std::move(owned_b_server));
  auto frag_a = CreateNode("frag-a", bus_a);
  auto frag_b = CreateNode("frag-b", owned_b);
  frag_a->MarkAsCompositeParent();
  frag_b->MarkAsCompositeParent();

  std::vector<fuchsia_driver_framework::NodePropertyEntry2> parent_props{
      {"frag-a", {}},
      {"frag-b", {}},
  };
  auto composite = CreateCompositeNode("composite-ns", {frag_a, frag_b}, parent_props);

  StartTestDriver(bus_a);
  auto [bus_a_server, bus_a_node] = node_manager->driver_host().TakeDriver("bus-a");
  FakeDriver bus_a_driver(dispatcher(), std::move(bus_a_server), std::move(bus_a_node));

  StartTestDriver(bus_b);
  auto [bus_b_server, bus_b_node] = node_manager->driver_host().TakeDriver("bus-b");
  FakeDriver bus_b_driver(dispatcher(), std::move(bus_b_server), std::move(bus_b_node));

  StartTestDriver(composite);
  auto [comp_server, comp_node] = node_manager->driver_host().TakeDriver("composite-ns");
  FakeDriver comp_driver(dispatcher(), std::move(comp_server), std::move(comp_node));

  size_t bus_a_probes = 0;
  size_t bus_b_probes = 0;
  bus_a_driver.SetHealthCheckCallback(
      [&bus_a_probes](const fuchsia_driver_framework::HealthCheckRequest& req) {
        bus_a_probes++;
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{});
      });
  bus_b_driver.SetHealthCheckCallback(
      [&bus_b_probes](const fuchsia_driver_framework::HealthCheckRequest& req) {
        bus_b_probes++;
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{});
      });

  ASSERT_EQ(ZX_OK,
            comp_driver
                .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
                    fuchsia_driver_framework::HealthCheckResponse{}))
                .status_value());
  RunLoopUntilIdle();

  EXPECT_EQ(1u, bus_a_probes);
  EXPECT_EQ(1u, bus_b_probes);
}

TEST_F(Dfv2NodeTest, OnHealthAlertUpstreamNeighborFaultCooldownCoalescesStorm) {
  auto parent = CreateNode("parent-cd");
  auto child_1 = CreateNode("child-cd-1", parent);
  auto child_2 = CreateNode("child-cd-2", parent);

  StartTestDriver(parent);
  auto [parent_server, parent_node_client] = node_manager->driver_host().TakeDriver("parent-cd");
  FakeDriver parent_driver(dispatcher(), std::move(parent_server), std::move(parent_node_client));

  StartTestDriver(child_1);
  auto [c1_server, c1_node_client] = node_manager->driver_host().TakeDriver("child-cd-1");
  FakeDriver c1_driver(dispatcher(), std::move(c1_server), std::move(c1_node_client));

  StartTestDriver(child_2);
  auto [c2_server, c2_node_client] = node_manager->driver_host().TakeDriver("child-cd-2");
  FakeDriver c2_driver(dispatcher(), std::move(c2_server), std::move(c2_node_client));

  size_t parent_probes = 0;
  parent_driver.SetHealthCheckCallback(
      [&parent_probes](const fuchsia_driver_framework::HealthCheckRequest& req) {
        parent_probes++;
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{});
      });

  // First alert from child_1 probes parent.
  ASSERT_EQ(ZX_OK,
            c1_driver
                .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
                    fuchsia_driver_framework::HealthCheckResponse{}))
                .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ(1u, parent_probes);

  // Duplicate alert from child_1 within cooldown is coalesced.
  ASSERT_EQ(ZX_OK,
            c1_driver
                .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
                    fuchsia_driver_framework::HealthCheckResponse{}))
                .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ(1u, parent_probes);

  // Alert from sibling child_2 within cooldown also avoids re-probing the same parent.
  ASSERT_EQ(ZX_OK,
            c2_driver
                .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
                    fuchsia_driver_framework::HealthCheckResponse{}))
                .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ(1u, parent_probes);

  // Advance virtual time past kNeighborAlertCooldown; next alert probes parent again.
  RunLoopFor(driver_manager::Node::kNeighborAlertCooldown);
  ASSERT_EQ(ZX_OK,
            c1_driver
                .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
                    fuchsia_driver_framework::HealthCheckResponse{}))
                .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ(2u, parent_probes);
}

TEST_F(Dfv2NodeTest, OnHealthAlertNeighborFaultSweepsNamedSideOnly) {
  auto parent = CreateNode("parent-dir");
  auto middle = CreateNode("middle-dir", parent);
  auto child = CreateNode("child-dir", middle);

  StartTestDriver(parent);
  auto [parent_server, parent_node_client] = node_manager->driver_host().TakeDriver("parent-dir");
  FakeDriver parent_driver(dispatcher(), std::move(parent_server), std::move(parent_node_client));

  StartTestDriver(middle);
  auto [middle_server, middle_node_client] = node_manager->driver_host().TakeDriver("middle-dir");
  FakeDriver middle_driver(dispatcher(), std::move(middle_server), std::move(middle_node_client));

  StartTestDriver(child);
  auto [child_server, child_node_client] = node_manager->driver_host().TakeDriver("child-dir");
  FakeDriver child_driver(dispatcher(), std::move(child_server), std::move(child_node_client));

  size_t parent_probes = 0;
  size_t child_probes = 0;
  parent_driver.SetHealthCheckCallback(
      [&parent_probes](const fuchsia_driver_framework::HealthCheckRequest& req)
          -> fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse> {
        EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kFast, req);
        parent_probes++;
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{});
      });
  child_driver.SetHealthCheckCallback(
      [&child_probes](const fuchsia_driver_framework::HealthCheckRequest& req)
          -> fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse> {
        EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kFast, req);
        child_probes++;
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{{
            .conditions = {{{{
                .status = fuchsia_driver_framework::HealthStatus::kDegraded,
                .reason_code = "CHILD_DEGRADED",
            }}}},
        }});
      });

  // upstream_neighbor_fault probes only the parents (what the reporter depends on).
  ASSERT_EQ(ZX_OK,
            middle_driver
                .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
                    fuchsia_driver_framework::HealthCheckResponse{}))
                .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ(1u, parent_probes);
  EXPECT_EQ(0u, child_probes);
  ASSERT_TRUE(parent->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kOk, parent->last_health_check()->status);

  // downstream_neighbor_fault probes only the children (what depends on the reporter). It is sent
  // immediately after the upstream alert: alerts are not coalesced per reporter, so an alert of a
  // different type within kNeighborAlertCooldown still sweeps its own side.
  ASSERT_EQ(ZX_OK,
            middle_driver
                .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithDownstreamNeighborFault(
                    fuchsia_driver_framework::HealthCheckResponse{}))
                .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ(1u, parent_probes);
  EXPECT_EQ(1u, child_probes);
  ASSERT_TRUE(child->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kDegraded, child->last_health_check()->status);
  EXPECT_EQ("CHILD_DEGRADED", child->last_health_check()->first_reason_code());

  // Neighbor fault alerts do not overwrite the reporter's own record.
  EXPECT_FALSE(middle->last_health_check().has_value());
}

TEST_F(Dfv2NodeTest, OnHealthAlertSelfFaultRecordsAndPingsParent) {
  auto parent = CreateNode("parent-sf");
  auto child = CreateNode("child-sf", parent);

  StartTestDriver(parent);
  auto [parent_server, parent_node_client] = node_manager->driver_host().TakeDriver("parent-sf");
  FakeDriver parent_driver(dispatcher(), std::move(parent_server), std::move(parent_node_client));

  StartTestDriver(child);
  auto [child_server, child_node_client] = node_manager->driver_host().TakeDriver("child-sf");
  FakeDriver child_driver(dispatcher(), std::move(child_server), std::move(child_node_client));

  std::optional<fuchsia_driver_framework::HealthCheckRequest> parent_check_level;
  size_t parent_ping_count = 0;
  parent_driver.SetHealthCheckCallback(
      [&parent_check_level,
       &parent_ping_count](const fuchsia_driver_framework::HealthCheckRequest& req)
          -> fit::result<fuchsia_driver_framework::HealthCheckError,
                         fuchsia_driver_framework::HealthCheckResponse> {
        parent_check_level = req;
        parent_ping_count++;
        return fit::ok(fuchsia_driver_framework::HealthCheckResponse{});
      });

  ASSERT_EQ(ZX_OK, child_driver
                       .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithSelfFault(
                           fuchsia_driver_framework::HealthCheckResponse{{
                               .conditions = {{{{
                                   .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
                                   .reason_code = "FW_CRASH",
                                   .message = "Firmware crashed",
                               }}}},
                           }}))
                       .status_value());
  RunLoopUntilIdle();

  ASSERT_TRUE(child->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy, child->last_health_check()->status);
  ASSERT_EQ(1u, child->last_health_check()->conditions.size());
  EXPECT_EQ("FW_CRASH", child->last_health_check()->first_reason_code());
  EXPECT_EQ("Firmware crashed", child->last_health_check()->conditions[0].message());

  ASSERT_TRUE(parent_check_level.has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kPing, *parent_check_level);
  EXPECT_EQ(1u, parent_ping_count);
  ASSERT_TRUE(parent->last_health_check().has_value());
  EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kOk, parent->last_health_check()->status);

  // A second SELF_FAULT within kNeighborAlertCooldown always records the faulting node's latest
  // details, but coalesces the parent PING probe.
  ASSERT_EQ(ZX_OK, child_driver
                       .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithSelfFault(
                           fuchsia_driver_framework::HealthCheckResponse{{
                               .conditions = {{{{
                                   .status = fuchsia_driver_framework::HealthStatus::kUnhealthy,
                                   .reason_code = "FW_CRASH_2",
                                   .message = "Second crash",
                               }}}},
                           }}))
                       .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ("FW_CRASH_2", child->last_health_check()->first_reason_code());
  EXPECT_EQ(1u, parent_ping_count);

  // However, an UpstreamNeighborFault alert within the same cooldown window must NOT be suppressed
  // by the earlier kPing liveness check—it must still escalate and run a kFast probe on the parent.
  ASSERT_EQ(ZX_OK,
            child_driver
                .SendHealthAlert(fuchsia_driver_framework::HealthAlert::WithUpstreamNeighborFault(
                    fuchsia_driver_framework::HealthCheckResponse{}))
                .status_value());
  RunLoopUntilIdle();
  EXPECT_EQ(2u, parent_ping_count);
  EXPECT_EQ(fuchsia_driver_framework::HealthCheckRequest::kFast, *parent_check_level);
}

TEST_F(Dfv2NodeTest, StartDriverPowerElementOptIn) {
  ASSERT_FALSE(node_manager->SuspendEnabled());

  // 0. When platform suspend is disabled and IsHermeticPowerTest() is false, drivers requesting
  // suspend_enabled or power_managed_dispatchers_enabled do not create power elements.
  auto node_no_platform_suspend = CreateNode("node_no_platform_suspend");
  ASSERT_FALSE(node_no_platform_suspend->IsHermeticPowerTest());
  StartTestDriver(node_no_platform_suspend, {.suspend_enabled = true});
  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(node_no_platform_suspend->is_bound());
  EXPECT_EQ(0, node_manager->create_power_element_calls());
  EXPECT_FALSE(node_no_platform_suspend->HasActivePowerElement());

  auto node_pm_no_platform_suspend = CreateNode("node_pm_no_platform_suspend");
  ASSERT_FALSE(node_pm_no_platform_suspend->IsHermeticPowerTest());
  StartTestDriver(node_pm_no_platform_suspend, {.power_managed_dispatchers_enabled = true});
  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(node_pm_no_platform_suspend->is_bound());
  EXPECT_EQ(0, node_manager->create_power_element_calls());
  EXPECT_FALSE(node_pm_no_platform_suspend->HasActivePowerElement());

  node_manager->set_suspend_enabled(true);

  // 1. Default driver (neither suspend_enabled nor power_managed_dispatchers_enabled) -> no PE
  auto node_default = CreateNode("node_default");
  StartTestDriver(node_default);
  EXPECT_EQ(0, node_manager->create_power_element_calls());
  EXPECT_FALSE(node_default->HasActivePowerElement());

  // 2. Driver with suspend_enabled: "true" -> PE created
  auto node_suspend = CreateNode("node_suspend");
  StartTestDriver(node_suspend, {.suspend_enabled = true});
  EXPECT_EQ(1, node_manager->create_power_element_calls());
  EXPECT_TRUE(node_suspend->HasActivePowerElement());

  // 3. Driver with power_managed_dispatchers_enabled: "true" -> PE created
  auto node_pm = CreateNode("node_pm");
  StartTestDriver(node_pm, {.power_managed_dispatchers_enabled = true});
  EXPECT_EQ(2, node_manager->create_power_element_calls());
  EXPECT_TRUE(node_pm->HasActivePowerElement());
}

TEST_F(Dfv2NodeTest, AncestorPowerDependencySkipsNonPowerEnabledNodes) {
  node_manager->set_suspend_enabled(true);

  // Linear chain: grandparent (PE) -> parent (bound, no PE) -> child (PE)
  auto grandparent = CreateNode("grandparent");
  StartTestDriver(grandparent, {.suspend_enabled = true});
  ASSERT_TRUE(grandparent->HasActivePowerElement());

  zx_info_handle_basic_t gp_token_info;
  ASSERT_EQ(grandparent->DuplicatePowerToken().get_info(ZX_INFO_HANDLE_BASIC, &gp_token_info,
                                                        sizeof(gp_token_info), nullptr, nullptr),
            ZX_OK);

  auto parent = CreateNode("parent", grandparent);
  StartTestDriver(parent);  // bound, but no PE
  ASSERT_TRUE(parent->is_bound());
  ASSERT_FALSE(parent->HasActivePowerElement());

  auto child = CreateNode("child", parent);
  StartTestDriver(child, {.suspend_enabled = true});
  ASSERT_TRUE(child->HasActivePowerElement());

  // Child's power dependencies should contain grandparent's token, skipping parent.
  ASSERT_EQ(1u, node_manager->last_deps().size());
  zx_info_handle_basic_t child_dep_info;
  ASSERT_EQ(node_manager->last_deps()[0].get_info(ZX_INFO_HANDLE_BASIC, &child_dep_info,
                                                  sizeof(child_dep_info), nullptr, nullptr),
            ZX_OK);
  EXPECT_EQ(child_dep_info.koid, gp_token_info.koid);

  // Composite DAG:
  // comp_p1 (bound, no PE) with parent comp_gp1 (PE)
  // comp_p2 (PE)
  auto comp_gp1 = CreateNode("comp_gp1");
  StartTestDriver(comp_gp1, {.suspend_enabled = true});
  zx_info_handle_basic_t comp_gp1_info;
  ASSERT_EQ(comp_gp1->DuplicatePowerToken().get_info(ZX_INFO_HANDLE_BASIC, &comp_gp1_info,
                                                     sizeof(comp_gp1_info), nullptr, nullptr),
            ZX_OK);

  auto comp_p1 = CreateNode("comp_p1", comp_gp1);
  StartTestDriver(comp_p1);  // bound, no PE
  ASSERT_FALSE(comp_p1->HasActivePowerElement());

  auto comp_p2 = CreateNode("comp_p2");
  StartTestDriver(comp_p2, {.suspend_enabled = true});
  zx_info_handle_basic_t comp_p2_info;
  ASSERT_EQ(comp_p2->DuplicatePowerToken().get_info(ZX_INFO_HANDLE_BASIC, &comp_p2_info,
                                                    sizeof(comp_p2_info), nullptr, nullptr),
            ZX_OK);

  auto composite = CreateCompositeNode("composite_pe", {comp_p1, comp_p2}, {{}, {}});
  StartTestDriver(composite, {.suspend_enabled = true});
  ASSERT_TRUE(composite->HasActivePowerElement());
  ASSERT_EQ(2u, node_manager->last_deps().size());

  std::set<zx_koid_t> dep_koids;
  for (const auto& dep : node_manager->last_deps()) {
    zx_info_handle_basic_t info;
    ASSERT_EQ(dep.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr), ZX_OK);
    dep_koids.insert(info.koid);
  }
  EXPECT_TRUE(dep_koids.count(comp_gp1_info.koid));
  EXPECT_TRUE(dep_koids.count(comp_p2_info.koid));
}

TEST_F(Dfv2NodeTest, RestartFamilyRootWithoutPowerElementPropagatesOverridesToDescendants) {
  ASSERT_FALSE(node_manager->SuspendEnabled());

  auto family_root = CreateNode("family_root");

  zx::event cpu_token;
  ASSERT_EQ(zx::event::create(0, &cpu_token), ZX_OK);
  zx::event cpu_token_copy;
  ASSERT_EQ(cpu_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &cpu_token_copy), ZX_OK);
  family_root->SetCpuTokenOverride(std::move(cpu_token_copy));

  zx::event dep_token;
  ASSERT_EQ(zx::event::create(0, &dep_token), ZX_OK);
  zx::event dep_token_copy;
  ASSERT_EQ(dep_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &dep_token_copy), ZX_OK);

  std::vector<fuchsia_power_broker::LevelDependency> overrides;
  overrides.push_back(fuchsia_power_broker::LevelDependency{{
      .dependent_level = 1,
      .requires_token = std::move(dep_token_copy),
      .requires_level_by_preference = std::vector<uint8_t>{1},
  }});
  family_root->SetPowerDependencyOverrides(std::move(overrides));

  // Start family_root WITHOUT power element opt-in.
  StartTestDriver(family_root);
  ASSERT_TRUE(family_root->is_bound());
  ASSERT_FALSE(family_root->HasActivePowerElement());
  ASSERT_TRUE(family_root->IsHermeticPowerTest());

  // Add child under family_root and start it WITH power element opt-in.
  auto child = CreateNode("child", family_root);
  EXPECT_TRUE(child->IsHermeticPowerTest());
  StartTestDriver(child, {.suspend_enabled = true});
  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(child->HasActivePowerElement());

  // Child should have inherited both the power_dependency_overrides_ and cpu_token_override_
  // from family_root since family_root has no power element of its own.
  ASSERT_EQ(1u, node_manager->last_deps().size());
  zx_info_handle_basic_t info;
  zx_info_handle_basic_t expected_info;
  ASSERT_EQ(node_manager->last_deps()[0].get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info),
                                                  nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(dep_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info), nullptr,
                               nullptr),
            ZX_OK);
  EXPECT_EQ(info.koid, expected_info.koid);

  ASSERT_TRUE(node_manager->last_cpu_token_override().has_value());
  ASSERT_EQ(node_manager->last_cpu_token_override()->get_info(ZX_INFO_HANDLE_BASIC, &info,
                                                              sizeof(info), nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(cpu_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info), nullptr,
                               nullptr),
            ZX_OK);
  EXPECT_EQ(info.koid, expected_info.koid);
}

TEST_F(Dfv2NodeTest, IsHermeticPowerTestCompositeAndDiamondAncestors) {
  ASSERT_FALSE(node_manager->SuspendEnabled());

  // 1. Multi-parent composite where Parent B (primary/first parent) has no hermetic test markers
  // and a deep ancestor chain, while Parent A (second parent) has power_dependency_overrides_.
  auto b_root = CreateNode("b_root");
  StartTestDriver(b_root);
  auto b_mid = CreateNode("b_mid", b_root);
  StartTestDriver(b_mid);
  auto parent_b = CreateNode("parent_b", b_mid);
  StartTestDriver(parent_b);
  ASSERT_FALSE(parent_b->IsHermeticPowerTest());

  auto parent_a = CreateNode("parent_a");
  zx::event parent_a_dep_token;
  ASSERT_EQ(zx::event::create(0, &parent_a_dep_token), ZX_OK);
  zx::event parent_a_dep_copy;
  ASSERT_EQ(parent_a_dep_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &parent_a_dep_copy), ZX_OK);
  std::vector<fuchsia_power_broker::LevelDependency> parent_a_overrides;
  parent_a_overrides.push_back(fuchsia_power_broker::LevelDependency{{
      .dependent_level = 1,
      .requires_token = std::move(parent_a_dep_copy),
      .requires_level_by_preference = std::vector<uint8_t>{1},
  }});
  parent_a->SetPowerDependencyOverrides(std::move(parent_a_overrides));
  StartTestDriver(parent_a);
  ASSERT_FALSE(parent_a->HasActivePowerElement());
  ASSERT_TRUE(parent_a->IsHermeticPowerTest());

  auto composite = CreateCompositeNode("composite_hermetic", {parent_b, parent_a}, {{}, {}});
  EXPECT_TRUE(composite->IsHermeticPowerTest());

  StartTestDriver(composite, {.suspend_enabled = true});
  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(composite->HasActivePowerElement());
  ASSERT_EQ(1u, node_manager->last_deps().size());

  zx_info_handle_basic_t dep_info;
  zx_info_handle_basic_t expected_info;
  ASSERT_EQ(node_manager->last_deps()[0].get_info(ZX_INFO_HANDLE_BASIC, &dep_info, sizeof(dep_info),
                                                  nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(parent_a_dep_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info),
                                        nullptr, nullptr),
            ZX_OK);
  EXPECT_EQ(dep_info.koid, expected_info.koid);

  // 2. Diamond ancestor topology: diamond_root -> {diamond_left, diamond_right} ->
  // diamond_composite.
  auto diamond_root = CreateNode("diamond_root");
  StartTestDriver(diamond_root);
  auto diamond_left = CreateNode("diamond_left", diamond_root);
  StartTestDriver(diamond_left);
  auto diamond_right = CreateNode("diamond_right", diamond_root);
  StartTestDriver(diamond_right);
  auto diamond_composite =
      CreateCompositeNode("diamond_composite", {diamond_left, diamond_right}, {{}, {}});

  // Without markers on diamond_root, IsHermeticPowerTest() traverses the diamond and returns false.
  EXPECT_FALSE(diamond_composite->IsHermeticPowerTest());

  // Setting power_dependency_overrides_ on diamond_root makes diamond_composite detect hermetic
  // test mode across 2 levels and deduplicate the shared ancestor's dependency token.
  zx::event diamond_dep_token;
  ASSERT_EQ(zx::event::create(0, &diamond_dep_token), ZX_OK);
  zx::event diamond_dep_copy;
  ASSERT_EQ(diamond_dep_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &diamond_dep_copy), ZX_OK);
  std::vector<fuchsia_power_broker::LevelDependency> diamond_overrides;
  diamond_overrides.push_back(fuchsia_power_broker::LevelDependency{{
      .dependent_level = 1,
      .requires_token = std::move(diamond_dep_copy),
      .requires_level_by_preference = std::vector<uint8_t>{1},
  }});
  diamond_root->SetPowerDependencyOverrides(std::move(diamond_overrides));

  EXPECT_TRUE(diamond_composite->IsHermeticPowerTest());

  StartTestDriver(diamond_composite, {.suspend_enabled = true});
  ASSERT_FALSE(node_manager->SuspendEnabled());
  ASSERT_TRUE(diamond_composite->HasActivePowerElement());
  ASSERT_EQ(1u, node_manager->last_deps().size());
  ASSERT_EQ(node_manager->last_deps()[0].get_info(ZX_INFO_HANDLE_BASIC, &dep_info, sizeof(dep_info),
                                                  nullptr, nullptr),
            ZX_OK);
  ASSERT_EQ(diamond_dep_token.get_info(ZX_INFO_HANDLE_BASIC, &expected_info, sizeof(expected_info),
                                       nullptr, nullptr),
            ZX_OK);
  EXPECT_EQ(dep_info.koid, expected_info.koid);
}
