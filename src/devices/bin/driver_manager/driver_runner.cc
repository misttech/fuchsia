// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/driver_runner.h"

#include <fidl/fuchsia.component.sandbox/cpp/common_types_format.h>
#include <fidl/fuchsia.driver.development/cpp/wire.h>
#include <fidl/fuchsia.driver.host/cpp/wire.h>
#include <fidl/fuchsia.driver.index/cpp/wire.h>
#include <fidl/fuchsia.driver.token/cpp/wire.h>
#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <fidl/fuchsia.process/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fdio/directory.h>
#include <lib/fit/defer.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/status.h>

#include <forward_list>
#include <memory>
#include <optional>
#include <queue>
#include <random>
#include <stack>
#include <utility>

#include "fidl/fuchsia.power.broker/cpp/natural_types.h"
#include "src/devices/bin/driver_manager/async_sharder.h"
#include "src/devices/bin/driver_manager/composite/composite_node_spec.h"
#include "src/devices/bin/driver_manager/node_property_conversion.h"
#include "src/devices/lib/log/log.h"
#include "src/lib/fxl/strings/join_strings.h"

namespace fdf {

using namespace fuchsia_driver_framework;
}
namespace fdh = fuchsia_driver_host;
namespace fdd = fuchsia_driver_development;
namespace fdi = fuchsia_driver_index;
namespace fio = fuchsia_io;
namespace frunner = fuchsia_component_runner;
namespace fcomponent = fuchsia_component;
namespace fdecl = fuchsia_component_decl;

using InspectStack = std::stack<std::pair<inspect::Node*, const driver_manager::Node*>>;

namespace driver_manager {

namespace {

constexpr auto kBootScheme = "fuchsia-boot://";
constexpr std::string_view kRootDeviceName = "dev";

void InspectNode(inspect::Inspector& inspector, InspectStack& stack) {
  std::forward_list<inspect::Node> roots;
  std::unordered_set<const Node*> unique_nodes;
  while (!stack.empty()) {
    // Pop the current root and node to operate on.
    auto [root, node] = stack.top();
    stack.pop();

    auto [_, inserted] = unique_nodes.insert(node);
    if (!inserted) {
      // Only insert unique nodes from the DAG.
      continue;
    }

    // Populate root with data from node.
    if (const auto& offers = node->offers(); !offers.empty()) {
      auto array = root->CreateStringArray("offers", offers.size());
      for (size_t i = 0; i < offers.size(); i++) {
        array.Set(i, offers[i].service_name);
      }
      root->Record(std::move(array));
    }
    if (auto symbols = node->symbols(); !symbols.empty()) {
      auto array = root->CreateStringArray("symbols", symbols.size());
      for (size_t i = 0; i < symbols.size(); i++) {
        array.Set(i, symbols[i].name().value());
      }
      root->Record(std::move(array));
    }
    if (auto properties = node->GetNodeProperties(); properties && !properties->empty()) {
      root->RecordChild("properties", [&](inspect::Node& properties_array) {
        for (uint32_t i = 0; i < properties->size(); ++i) {
          properties_array.RecordChild(std::to_string(i), [&](inspect::Node& inspect_property) {
            auto& property = properties.value()[i];
            inspect_property.RecordString("key", property.key());

            if (const auto& str_prop = property.value().string_value(); str_prop.has_value()) {
              inspect_property.RecordString("value", str_prop.value());

            } else if (const auto& int_prop = property.value().int_value(); int_prop.has_value()) {
              inspect_property.RecordUint("value", int_prop.value());

            } else if (const auto& enum_prop = property.value().enum_value();
                       enum_prop.has_value()) {
              inspect_property.RecordString("value", enum_prop.value());

            } else if (const auto& bool_prop = property.value().bool_value();
                       bool_prop.has_value()) {
              inspect_property.RecordBool("value", bool_prop.value());

            } else {
              inspect_property.RecordString("value", "UNKNOWN VALUE TYPE");
            }
          });
        }
      });
    }

    root->RecordString("type", node->IsComposite() ? "Composite Device" : "Device");
    root->RecordString("topological_path", node->MakeTopologicalPath());

    root->RecordString("driver", node->driver_url());

    // Push children of this node onto the stack. We do this in reverse order to
    // ensure the children are handled in order, from first to last.
    auto& children = node->children();
    for (auto child = children.rbegin(), end = children.rend(); child != end; ++child) {
      auto& name = (*child)->name();
      auto& root_for_child = roots.emplace_front(root->CreateChild(name));
      stack.emplace(&root_for_child, child->get());
    }
  }

  // Store all of the roots in the inspector.
  for (auto& root : roots) {
    inspector.GetRoot().Record(std::move(root));
  }
}

fidl::StringView CollectionName(Collection collection) {
  switch (collection) {
    case Collection::kNone:
      return {};
    case Collection::kBoot:
      return "boot-drivers";
    case Collection::kPackage:
      return "base-drivers";
    case Collection::kFullPackage:
      return "full-drivers";
  }
}

Collection ToCollection(fdf::DriverPackageType package) {
  switch (package) {
    case fdf::DriverPackageType::kBoot:
      return Collection::kBoot;
    case fdf::DriverPackageType::kBase:
      return Collection::kPackage;
    case fdf::DriverPackageType::kCached:
    case fdf::DriverPackageType::kUniverse:
      return Collection::kFullPackage;
    default:
      return Collection::kNone;
  }
}

// Choose the highest ranked collection between `collection` and `node`'s
// parents. If one of `node`'s parent's collection is none then check the
// parent's parents and so on.
Collection GetHighestRankingCollection(const Node& node, Collection collection) {
  std::stack<std::weak_ptr<Node>> ancestors;
  for (const auto& parent : node.parents()) {
    ancestors.emplace(parent);
  }

  // Find the highest ranked collection out of `node`'s parent nodes. If a
  // node's collection is none then check that node's parents and so on.
  while (!ancestors.empty()) {
    auto ancestor = ancestors.top();
    ancestors.pop();
    auto ancestor_ptr = ancestor.lock();
    if (!ancestor_ptr) {
      fdf_log::warn("Ancestor node released");
      continue;
    }

    auto ancestor_collection = ancestor_ptr->collection();
    if (ancestor_collection == Collection::kNone) {
      // Check ancestor's parents to see what the collection of the ancestor
      // should be.
      for (const auto& parent : ancestor_ptr->parents()) {
        ancestors.emplace(parent);
      }
    } else if (ancestor_collection > collection) {
      collection = ancestor_collection;
    }
  }

  return collection;
}

// Perform a Breadth-First-Search (BFS) over the node topology, applying the visitor function on
// the node being visited.
// The return value of the visitor function is a boolean for whether the children of the node
// should be visited. If it returns false, the children will be skipped.
void PerformBFS(const std::shared_ptr<Node>& starting_node,
                fit::function<bool(const std::shared_ptr<driver_manager::Node>&)> visitor) {
  std::unordered_set<std::shared_ptr<const Node>> visited;
  std::queue<std::shared_ptr<Node>> node_queue;
  visited.insert(starting_node);
  node_queue.push(starting_node);

  while (!node_queue.empty()) {
    auto current = node_queue.front();
    node_queue.pop();

    bool visit_children = visitor(current);
    if (!visit_children) {
      continue;
    }

    for (const auto& child : current->children()) {
      if (child->GetPrimaryParent() != current.get()) {
        continue;
      }

      if (auto [_, inserted] = visited.insert(child); inserted) {
        node_queue.push(child);
      }
    }
  }
}

void CallStartDriverOnRunner(Runner& runner, Node& node, const std::string& moniker,
                             std::string_view url,
                             const std::shared_ptr<BootupTracker>& bootup_tracker) {
  if (!node.HasDriverComponentController()) {
    auto [controller_client, controller_request] =
        fidl::Endpoints<fcomponent::Controller>::Create();
    node.SetController(std::move(controller_client));
    runner.CreateDriverComponent(node.shared_from_this(), std::move(controller_request), moniker,
                                 url, CollectionName(node.collection()).get(), node.offers());
  } else {
    runner.StartDriverComponent(moniker);
  }
}

// Exists in fsl, but perhaps a bit of duplication is better than a bit of dependency.
zx_koid_t GetKoid(const zx::event& handle) {
  zx_info_handle_basic_t info;
  zx_status_t status =
      zx_object_get_info(handle.get(), ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  return status == ZX_OK ? info.koid : ZX_KOID_INVALID;
}

fuchsia_power_broker::ElementSchema CreateElementSchema(
    std::string_view name, fuchsia_power_broker::PowerLevel initial_level,
    std::vector<fuchsia_power_broker::PowerLevel> valid_levels,
    fidl::ServerEnd<fuchsia_power_broker::Lessor> lessor_channel,
    fidl::ServerEnd<fuchsia_power_broker::ElementControl> element_control,
    fidl::ClientEnd<fuchsia_power_broker::ElementRunner> element_runner,
    std::vector<fuchsia_power_broker::LevelDependency> dependencies = {},
    std::optional<zx::eventpair> initial_lease_token = std::nullopt) {
  return fuchsia_power_broker::ElementSchema{{
      .element_name = std::string(name),
      .initial_current_level = initial_level,
      .valid_levels = std::move(valid_levels),
      .dependencies = std::move(dependencies),
      .lessor_channel = std::move(lessor_channel),
      .element_control = std::move(element_control),
      .element_runner = std::move(element_runner),
      .initial_lease_token = std::move(initial_lease_token),
  }};
}

fuchsia_power_broker::LevelDependency CreateLevelDependency(
    fuchsia_power_broker::PowerLevel dependent_level,
    fuchsia_power_broker::DependencyToken requires_token,
    std::vector<fuchsia_power_broker::PowerLevel> requires_level_by_preference) {
  return fuchsia_power_broker::LevelDependency{{
      .dependent_level = dependent_level,
      .requires_token = std::move(requires_token),
      .requires_level_by_preference = std::move(requires_level_by_preference),
  }};
}

}  // namespace

Collection ToCollection(const Node& node, fdf::DriverPackageType package_type) {
  Collection collection = ToCollection(package_type);
  return GetHighestRankingCollection(node, collection);
}

DriverRunner::DriverRunner(
    fidl::ClientEnd<fcomponent::Realm> realm,
    fidl::ClientEnd<fcomponent::Introspector> introspector,
    fidl::ClientEnd<fuchsia_component_sandbox::CapabilityStore> capability_store,
    fidl::ClientEnd<fdi::DriverIndex> driver_index, inspect::ComponentInspector& inspect,
    LoaderServiceFactory loader_service_factory, async_dispatcher_t* dispatcher,
    bool enable_test_shutdown_delays, OfferInjector offer_injector,
    fidl::ClientEnd<fuchsia_power_broker::Topology> topology_client,
    std::optional<DynamicLinkerArgs> dynamic_linker_args,
    std::optional<fidl::ClientEnd<fuchsia_power_system::CpuElementManager>> cpu_element_mgr,
    bool wait_for_storage_token_from_driver_)
    : driver_index_(std::move(driver_index), dispatcher),
      loader_service_factory_(std::move(loader_service_factory)),
      dictionary_util_(std::move(capability_store), dispatcher),
      dispatcher_(dispatcher),
      root_node_(std::make_shared<Node>(kRootDeviceName, std::weak_ptr<Node>{}, this, dispatcher)),
      composite_node_spec_manager_(this),
      bind_manager_(this, this, dispatcher),
      runner_(dispatcher, fidl::WireClient(std::move(realm), dispatcher),
              fidl::WireClient(std::move(introspector), dispatcher), offer_injector),
      removal_tracker_(dispatcher),
      enable_test_shutdown_delays_(enable_test_shutdown_delays),
      dynamic_linker_args_(std::move(dynamic_linker_args)),
      memory_attributor_(dispatcher_),
      cpu_element_client_(
          cpu_element_mgr.has_value()
              ? std::make_optional(fidl::Client<fuchsia_power_system::CpuElementManager>(
                    std::move(cpu_element_mgr.value()), dispatcher))
              : std::nullopt),
      wait_for_storage_token_from_driver_(wait_for_storage_token_from_driver_) {
  if (enable_test_shutdown_delays_) {
    // TODO(https://fxbug.dev/42084497): Allow the seed to be set from the configuration.
    auto seed = std::chrono::system_clock::now().time_since_epoch().count();
    fdf_log::info("Shutdown test delays enabled. Using seed {}", seed);
    shutdown_test_delay_rng_ = std::make_shared<std::mt19937>(static_cast<uint32_t>(seed));
  }

  inspect.root().RecordLazyNode("driver_runner", [this] { return Inspect(); });

  // Pick a non-zero starting id so that folks cannot rely on the driver host process names being
  // stable.
  std::random_device rd;
  std::mt19937 gen(rd());
  std::uniform_int_distribution<> distrib(0, 1000);
  next_driver_host_id_ = distrib(gen);

  bootup_tracker_ = std::make_shared<BootupTracker>(&bind_manager_, dispatcher);
  runner_.SetBootupTracker(bootup_tracker_);

  // Setup the driver notifier.
  auto [notifier_client, notifier_server] =
      fidl::Endpoints<fuchsia_driver_index::DriverNotifier>::Create();
  driver_notifier_bindings_.AddBinding(dispatcher_, std::move(notifier_server), this,
                                       fidl::kIgnoreBindingClosure);
  fidl::OneWayStatus status = driver_index_->SetNotifier(std::move(notifier_client));
  if (!status.ok()) {
    fdf_log::warn("Failed to set the driver notifier: {}", status.status_string());
  }

  if (topology_client.is_valid()) {
    power_topology_ =
        fidl::Client<fuchsia_power_broker::Topology>(std::move(topology_client), dispatcher);
  }
}

void DriverRunner::CreateStoragePowerElement(fuchsia_power_broker::DependencyToken driver_token,
                                             fuchsia_power_broker::PowerLevel power_level,
                                             fit::callback<void()> post_creation) {
  // We omit the check for `topology_` since this should only be called if topology is valid

  std::get<CallbackSet>(storage_callbacks_or_token_).push_back(std::move(post_creation));

  // Make a storage token.
  zx::event storage_token;
  ZX_ASSERT_MSG(zx::event::create(0, &storage_token) == ZX_OK, "Failure creating storage token");

  // Create a duplicate of the token that we can send to register with power broker.
  zx::event token_copy;
  ZX_ASSERT_MSG(storage_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &token_copy) == ZX_OK,
                "Duplication of storage token failed.");

  // Create the element schema. In a future change the schema will have a dependency on a power
  // element supplied to us by the storage driver.
  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();
  auto [element_control_client, element_control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [element_runner_client, element_runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();

  fuchsia_power_broker::ElementSchema schema = CreateElementSchema(
      "DF-Storage", /* initial_current_level */ 1, {0, 1}, std::move(lessor_server),
      std::move(element_control_server), std::move(element_runner_client));

  fidl::Client<fuchsia_power_broker::ElementControl> storage_control =
      fidl::Client<fuchsia_power_broker::ElementControl>(std::move(element_control_client),
                                                         dispatcher_);

  // We create the request even before we pass the server end of the channel to power broker.
  storage_control
      ->RegisterDependencyToken(
          {{.token = fuchsia_power_broker::DependencyToken{std::move(storage_token)}}})
      .Then([this, token = std::move(token_copy)](
                fidl::Result<fuchsia_power_broker::ElementControl::RegisterDependencyToken>
                    result) mutable {
        if (result.is_error() && result.error_value().is_framework_error()) {
          fdf_log::error(" Could not register dependency token, FIDL error: {} ",
                         result.error_value().FormatDescription());
        } else if (result.is_error()) {
          fdf_log::error("Could not register dependency token, protocol error: {}",
                         static_cast<uint32_t>(result.error_value().domain_error()));
        }

        ZX_ASSERT(result.is_ok());

        // Now that we have the storage token, run any driver creation callbacks which we deferred.
        auto after_storage_callbacks =
            std::move(std::get<CallbackSet>(storage_callbacks_or_token_));
        storage_callbacks_or_token_ = std::move(token);
        for (auto& cb : after_storage_callbacks) {
          cb();
        }
      });

  if (wait_for_storage_token_from_driver_) {
    ZX_ASSERT_MSG(driver_token.is_valid(), "Storage token required, but is invalid");
    std::vector<fuchsia_power_broker::LevelDependency> dep_on_storage_driver;
    dep_on_storage_driver.push_back(CreateLevelDependency(1, std::move(driver_token), {1}));
    schema.dependencies() = std::move(dep_on_storage_driver);
  }

  power_topology_->AddElement(std::move(schema))
      .Then([this, element_control_client = std::move(storage_control),
             runner_server = std::move(element_runner_server),
             lessor_client = std::move(lessor_client)](
                fidl::Result<fuchsia_power_broker::Topology::AddElement> add_result) mutable {
        if (add_result.is_error() && add_result.error_value().is_framework_error()) {
          fdf_log::error("Could not create storage power element, FIDL error: {}",
                         add_result.error_value().FormatDescription());
        } else if (add_result.is_error()) {
          fdf_log::error("Could not create storage power element, protocol error: ",
                         static_cast<uint32_t>(add_result.error_value().domain_error()));
        } else {
          storage_control_ = std::move(element_control_client);
          storage_element_runner_.AddBinding(this->dispatcher_, std::move(runner_server), this,
                                             fidl::kIgnoreBindingClosure);
          storage_lessor_ = std::move(lessor_client);
        }

        // If we're a power-enabled platform, it is an error for creation of the storage element to
        // fail.
        ZX_ASSERT(add_result.is_ok());
      });
}

// fidl::WireServer<fuchsia_driver_token::Debug>
void DriverRunner::LogStackTrace(LogStackTraceRequestView request,
                                 LogStackTraceCompleter::Sync& completer) {
  const zx_koid_t node_token_koid = GetKoid(request->node_token);
  if (node_token_koid == ZX_KOID_INVALID) {
    fdf_log::error("provided node token is not valid");
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
  }

  std::shared_ptr<const Node> node = nullptr;
  PerformBFS(
      root_node_,
      [&node, node_token_koid](const std::shared_ptr<driver_manager::Node>& current) -> bool {
        if (node != nullptr) {
          // Already found it.
          return false;
        }
        std::optional current_koid = current->token_koid();
        if (current_koid && current_koid.value() == node_token_koid) {
          node = current;
          return false;
        }
        return true;
      });
  if (node == nullptr) {
    completer.ReplyError(ZX_ERR_NOT_FOUND);
    fdf_log::warn("no such node: node_token_koid={}", node_token_koid);
    return;
  }
  const DriverHost* host = node->driver_host();
  if (host == nullptr) {
    completer.ReplyError(ZX_ERR_NOT_FOUND);
    fdf_log::warn("node has no host: node_token_koid={}", node_token_koid);
    return;
  }
  fdf_log::info("stack trace requested for host: node_token_koid={}", node_token_koid);
  host->TriggerStackTrace();
  completer.ReplySuccess();
}

void DriverRunner::GetHostKoid(GetHostKoidRequestView request,
                               GetHostKoidCompleter::Sync& completer) {
  const zx_koid_t node_token_koid = GetKoid(request->node_token);
  if (node_token_koid == ZX_KOID_INVALID) {
    fdf_log::error("provided node token is not valid");
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }

  std::shared_ptr<const Node> node = nullptr;
  PerformBFS(
      root_node_,
      [&node, node_token_koid](const std::shared_ptr<driver_manager::Node>& current) -> bool {
        if (node != nullptr) {
          // Already found it.
          return false;
        }
        std::optional current_koid = current->token_koid();
        if (current_koid && current_koid.value() == node_token_koid) {
          node = current;
          return false;
        }
        return true;
      });
  if (node == nullptr) {
    completer.ReplyError(ZX_ERR_NOT_FOUND);
    fdf_log::warn("no such node: node_token_koid={}", node_token_koid);
    return;
  }
  const DriverHost* host = node->driver_host();
  if (host == nullptr) {
    completer.ReplyError(ZX_ERR_NOT_FOUND);
    fdf_log::warn("node has no host: node_token_koid={}", node_token_koid);
    return;
  }

  host->GetProcessKoidAsync([completer = completer.ToAsync(),
                             node_token_koid](zx::result<uint64_t> host_koid_res) mutable {
    if (host_koid_res.is_error()) {
      completer.ReplyError(host_koid_res.status_value());
      fdf_log::warn("node host has no koid: node_token_koid={}, status={}", node_token_koid,
                    host_koid_res.status_string());
      return;
    }

    completer.ReplySuccess(host_koid_res.value());
  });
}

void DriverRunner::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_token::Debug> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  fdf_log::warn("Unknown Debug request: {}", metadata.method_ordinal);
}

void DriverRunner::CreateAllDriversPowerElement() {
  ZX_ASSERT_MSG(SuspendEnabled(), "Suspend must be enabled to create AllDrivers power element");
  ZX_ASSERT_MSG(!all_drivers_, "AllDrivers power element already created");
  all_drivers_ = std::make_shared<AllDriversElement>(this, root_node_);

  zx::event all_drivers_token;
  if (zx::event::create(0, &all_drivers_token) != ZX_OK) {
    fdf_log::error("Failed to create all driver token");
    all_drivers_.reset();
    return;
  }

  auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();
  auto [element_control_client, element_control_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
  auto [element_runner_client, element_runner_server] =
      fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();

  // Add storage token.
  std::vector<fuchsia_power_broker::LevelDependency> level_deps;
  if (std::holds_alternative<PowerDependencyToken>(storage_callbacks_or_token_)) {
    if (auto storage_token = StorageElementToken(); storage_token) {
      zx::event storage_token_copy;
      ZX_ASSERT(storage_token->duplicate(ZX_RIGHT_SAME_RIGHTS, &storage_token_copy) == ZX_OK);
      level_deps.emplace_back(CreateLevelDependency(1, std::move(storage_token_copy), {1}));
    }
  }

  // Add CPU token.
  {
    zx::event cpu_token_clone;
    ZX_ASSERT(std::get<PowerDependencyToken>(cpu_callbacks_or_token_)
                  .duplicate(ZX_RIGHT_SAME_RIGHTS, &cpu_token_clone) == ZX_OK);
    level_deps.emplace_back(CreateLevelDependency(1, std::move(cpu_token_clone), {1}));
  }

  fuchsia_power_broker::ElementSchema schema = CreateElementSchema(
      "AllDrivers", /* initial_current_level */ 0, {0, 1}, std::move(lessor_server),
      std::move(element_control_server), std::move(element_runner_client), std::move(level_deps));

  fidl::Client<fuchsia_power_broker::ElementControl> element_control =
      fidl::Client<fuchsia_power_broker::ElementControl>(std::move(element_control_client),
                                                         dispatcher_);

  zx::event all_drivers_token_copy;
  if (all_drivers_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &all_drivers_token_copy) != ZX_OK) {
    fdf_log::error("Failed to duplicate driver token");
    all_drivers_.reset();
    return;
  }

  // Since the server-side of this channel hasn't been given to power broker yet, this request is
  // effectively queued and will get processed after we create the element.
  element_control->RegisterDependencyToken({std::move(all_drivers_token_copy)})
      .Then([this, all_drivers_token = std::move(all_drivers_token)](
                fidl::Result<fuchsia_power_broker::ElementControl::RegisterDependencyToken>
                    result) mutable {
        if (result.is_error()) {
          fdf_log::error("Failed to register dependency token for AllDrivers: {}",
                         result.error_value());
          all_drivers_.reset();
          return;
        }

        zx::event all_drivers_driver_runner_copy;
        zx_status_t dupe_result =
            all_drivers_token.duplicate(ZX_RIGHT_SAME_RIGHTS, &all_drivers_driver_runner_copy);
        if (dupe_result != ZX_OK) {
          fdf_log::error("Failed to duplicate all drivers token: {}", dupe_result);
          all_drivers_.reset();
          return;
        }
        all_drivers_token_ = std::move(all_drivers_driver_runner_copy);

        // Hand the AllDrivers token to SAG.
        cpu_element_client_.value()
            ->AddExecutionStateDependency({{
                .dependency_token = std::move(all_drivers_token),
                .power_level = 1,
            }})
            .Then(
                [](fidl::Result<
                    fuchsia_power_system::CpuElementManager::AddExecutionStateDependency>& result) {
                  if (result.is_error()) {
                    fdf_log::error("Failure to register execution state dependency. {}",
                                   result.error_value());
                  }
                });
      });

  power_topology_->AddElement(std::move(schema))
      .Then([this, element_control = std::move(element_control),
             runner_server = std::move(element_runner_server),
             lessor_client = std::move(lessor_client)](
                fidl::Result<fuchsia_power_broker::Topology::AddElement> add_result) mutable {
        if (add_result.is_error() && add_result.error_value().is_framework_error()) {
          fdf_log::error("Could not create AllDrivers power element, FIDL error: {}",
                         add_result.error_value().FormatDescription());
          all_drivers_.reset();
        } else if (add_result.is_error()) {
          fdf_log::error("Could not create AllDrivers power element, protocol error: {}",
                         static_cast<uint32_t>(add_result.error_value().domain_error()));
          all_drivers_.reset();
        } else {
          PowerElementHandles pe_handles{
              .element_control = std::move(element_control),
              .element_runner = std::move(runner_server),
              .lessor =
                  fidl::Client<fuchsia_power_broker::Lessor>(std::move(lessor_client), dispatcher_),
          };
          // Give the power element's ownership to the |all_drivers_| server.
          all_drivers_->AttachElement(dispatcher_, std::move(pe_handles));
        }
      });
}

void DriverRunner::SetLevel(SetLevelRequestView request, SetLevelCompleter::Sync& completer) {
  // This is the SetLevel call for the storage element. The storage element exists only so in
  // driver manager we can make drivers loaded out of storage depend on the storage element they
  // otherwise would not. Therefore, there is not work to do when level change requests arrive.
  completer.Reply();
}

void DriverRunner::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  std::string method_type;
  switch (metadata.unknown_method_type) {
    case fidl::UnknownMethodType::kOneWay:
      method_type = "one-way";
      break;
    case fidl::UnknownMethodType::kTwoWay:
      method_type = "two-way";
      break;
  };

  fdf_log::warn("ElementRunner received unknown {} method. Ordinal: {}", method_type,
                metadata.method_ordinal);
}

void DriverRunner::BindNodesForCompositeNodeSpec() { TryBindAllAvailable(); }

void DriverRunner::AddSpec(AddSpecRequestView request, AddSpecCompleter::Sync& completer) {
  if (!request->has_name() || (!request->has_parents() && !request->has_parents2())) {
    completer.Reply(fit::error(fdf::CompositeNodeSpecError::kMissingArgs));
    return;
  }

  if (!request->has_parents() && !request->has_parents2()) {
    completer.Reply(fit::error(fdf::CompositeNodeSpecError::kDuplicateParents));
    return;
  }

  std::vector<fuchsia_driver_framework::ParentSpec2> parents;
  if (request->has_parents()) {
    if (request->parents().empty()) {
      completer.Reply(fit::error(fdf::CompositeNodeSpecError::kEmptyNodes));
      return;
    }
    auto to_parent_spec2 = [](const auto& parent) {
      auto parent_spec = fidl::ToNatural(parent);
      std::vector<fuchsia_driver_framework::BindRule2> bind_rules;
      std::transform(parent_spec.bind_rules().begin(), parent_spec.bind_rules().end(),
                     std::back_inserter(bind_rules), ToBindRule2);

      std::vector<fuchsia_driver_framework::NodeProperty2> properties;
      std::transform(parent_spec.properties().begin(), parent_spec.properties().end(),
                     std::back_inserter(properties),
                     [](const auto& prop) { return ToProperty2(prop); });
      return fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = std::move(bind_rules),
          .properties = std::move(properties),
      }};
    };

    std::transform(request->parents().cbegin(), request->parents().cend(),
                   std::back_inserter(parents), to_parent_spec2);
  }

  if (request->has_parents2()) {
    if (request->parents2().empty()) {
      completer.Reply(fit::error(fdf::CompositeNodeSpecError::kEmptyNodes));
      return;
    }
    parents = fidl::ToNatural(request->parents2()).value();
  }

  auto spec = std::make_unique<CompositeNodeSpec>(
      CompositeNodeSpecCreateInfo{
          .name = std::string(request->name().get()),
          .parents = std::move(parents),
          .driver_host_name_for_colocation = request->has_driver_host()
                                                 ? std::string(request->driver_host().get())
                                                 : std::string(),
      },
      dispatcher_, this);
  composite_node_spec_manager_.AddSpec(
      *request, std::move(spec),
      [completer = completer.ToAsync()](
          fit::result<fuchsia_driver_framework::CompositeNodeSpecError> result) mutable {
        completer.Reply(result);
      });
}

void DriverRunner::FindDriverCrash(FindDriverCrashRequestView request,
                                   FindDriverCrashCompleter::Sync& completer) {
  for (const DriverHostComponent& host : driver_hosts_) {
    zx::result process_koid = host.GetProcessKoid();
    if (process_koid.is_ok() && process_koid.value() == request->process_koid) {
      host.GetCrashInfo(
          request->thread_koid,
          [this, async_completer = completer.ToAsync()](
              zx::result<fuchsia_driver_host::DriverCrashInfo> info_result) mutable {
            if (info_result.is_error()) {
              async_completer.ReplyError(info_result.error_value());
              return;
            }
            fuchsia_driver_host::DriverCrashInfo& found = info_result.value();
            zx_info_handle_basic_t info;
            zx_status_t status = found.node_token()->get_info(ZX_INFO_HANDLE_BASIC, &info,
                                                              sizeof(info), nullptr, nullptr);
            if (status != ZX_OK) {
              async_completer.ReplyError(ZX_ERR_INTERNAL);
              return;
            }

            const Node* node = nullptr;
            PerformBFS(root_node_, [&node, token_koid = info.koid](
                                       const std::shared_ptr<driver_manager::Node>& current) {
              if (node != nullptr) {
                // Already found it.
                return false;
              }
              std::optional current_koid = current->token_koid();
              if (current_koid && current_koid.value() == token_koid) {
                node = current.get();
                return false;
              }
              return true;
            });
            if (node == nullptr) {
              async_completer.ReplyError(ZX_ERR_NOT_FOUND);
              return;
            }

            fidl::Arena arena;
            async_completer.ReplySuccess(fuchsia_driver_crash::wire::DriverCrashInfo::Builder(arena)
                                             .node_moniker(arena, node->MakeComponentMoniker())
                                             .url(arena, found.url().value())
                                             .Build());
          });
      return;
    }
  }
  completer.ReplyError(ZX_ERR_NOT_FOUND);
}

void DriverRunner::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_framework::CompositeNodeManager> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  std::string method_type;
  switch (metadata.unknown_method_type) {
    case fidl::UnknownMethodType::kOneWay:
      method_type = "one-way";
      break;
    case fidl::UnknownMethodType::kTwoWay:
      method_type = "two-way";
      break;
  };

  fdf_log::warn("CompositeNodeManager received unknown {} method. Ordinal: {}", method_type,
                metadata.method_ordinal);
}

void DriverRunner::Get(GetRequest& request,
                       fidl::Completer<fidl::internal::NaturalCompleterBase<
                           fuchsia_driver_token::NodeBusTopology::Get>>::Sync& completer) {
  zx_info_handle_basic_t info;
  zx_status_t status =
      request.token().get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    completer.Reply(zx::error(status));
    return;
  }
  const Node* node = nullptr;
  PerformBFS(root_node_,
             [&node, token_koid = info.koid](const std::shared_ptr<driver_manager::Node>& current) {
               if (node != nullptr) {
                 // Already found it.
                 return false;
               }
               std::optional current_koid = current->token_koid();
               if (current_koid && current_koid.value() == token_koid) {
                 node = current.get();
                 return false;
               }
               return true;
             });
  if (node == nullptr) {
    completer.Reply(zx::error(ZX_ERR_NOT_FOUND));
    return;
  }

  completer.Reply(zx::ok(node->GetBusTopology()));
}

void DriverRunner::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_token::NodeBusTopology> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  std::string method_type;
  switch (metadata.unknown_method_type) {
    case fidl::UnknownMethodType::kOneWay:
      method_type = "one-way";
      break;
    case fidl::UnknownMethodType::kTwoWay:
      method_type = "two-way";
      break;
  };

  fdf_log::warn("NodeBusTopology received unknown {} method. Ordinal: {}", method_type,
                metadata.method_ordinal);
}

void DriverRunner::AddSpecToDriverIndex(fuchsia_driver_framework::wire::CompositeNodeSpec group,
                                        AddToIndexCallback callback) {
  driver_index_->AddCompositeNodeSpec(group).Then(
      [callback = std::move(callback)](
          fidl::WireUnownedResult<fdi::DriverIndex::AddCompositeNodeSpec>& result) mutable {
        if (!result.ok()) {
          fdf_log::error("DriverIndex::AddCompositeNodeSpec failed {}", result.status());
          callback(zx::error(result.status()));
          return;
        }

        if (result->is_error()) {
          callback(result->take_error());
          return;
        }

        callback(zx::ok());
      });
}

// TODO(https://fxbug.dev/42072971): Add information for composite node specs.
fpromise::promise<inspect::Inspector> DriverRunner::Inspect() const {
  // Create our inspector.
  // The default maximum size was too small, and so this is double the default size.
  // If a device loads too much inspect data, this can be increased in the future.
  inspect::Inspector inspector(inspect::InspectSettings{.maximum_size = 2 * 256 * 1024});

  // Make the device tree inspect nodes.
  auto device_tree = inspector.GetRoot().CreateChild("node_topology");
  auto root = device_tree.CreateChild(root_node_->name());
  InspectStack stack{{std::make_pair(&root, root_node_.get())}};
  InspectNode(inspector, stack);
  device_tree.Record(std::move(root));
  inspector.GetRoot().Record(std::move(device_tree));

  bind_manager_.RecordInspect(inspector);

  return fpromise::make_ok_promise(inspector);
}

std::vector<fdd::wire::CompositeNodeInfo> DriverRunner::GetCompositeListInfo(
    fidl::AnyArena& arena) const {
  auto spec_composite_list = composite_node_spec_manager_.GetCompositeInfo(arena);
  auto list = bind_manager_.GetCompositeListInfo(arena);
  list.reserve(list.size() + spec_composite_list.size());
  list.insert(list.end(), std::make_move_iterator(spec_composite_list.begin()),
              std::make_move_iterator(spec_composite_list.end()));
  return list;
}

void DriverRunner::WaitForBootup(fit::callback<void()> callback) {
  bootup_tracker_->WaitForBootup(std::move(callback));
}

void DriverRunner::PublishCpuElementManager(component::OutgoingDirectory& outgoing) {
  zx::result result = outgoing.AddUnmanagedProtocol<fuchsia_power_system::CpuElementManager>(
      cpu_element_server_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure));
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());
}

void DriverRunner::GetCpuDependencyToken(GetCpuDependencyTokenCompleter::Sync& completer) {
  if (!std::holds_alternative<PowerDependencyToken>(cpu_callbacks_or_token_)) {
    if (!cpu_element_client_.has_value()) {
      completer.Close(ZX_ERR_BAD_STATE);
      return;
    }

    std::get<CallbackSet>(cpu_callbacks_or_token_)
        .push_back([this, completer = completer.ToAsync()]() mutable {
          zx::event cpu_copy;

          zx_status_t dupe_result = std::get<PowerDependencyToken>(cpu_callbacks_or_token_)
                                        .duplicate(ZX_RIGHT_SAME_RIGHTS, &cpu_copy);
          if (dupe_result != ZX_OK) {
            completer.Close(dupe_result);
            return;
          }

          fidl::Arena arena;
          completer.Reply(fuchsia_power_system::wire::Cpu::Builder(arena)
                              .assertive_dependency_token(std::move(cpu_copy))
                              .Build());
        });
    return;
  }

  zx::event cpu_copy;

  zx_status_t dupe_result = std::get<PowerDependencyToken>(cpu_callbacks_or_token_)
                                .duplicate(ZX_RIGHT_SAME_RIGHTS, &cpu_copy);
  if (dupe_result != ZX_OK) {
    completer.Close(dupe_result);
    return;
  }

  fidl::Arena arena;
  completer.Reply(fuchsia_power_system::wire::Cpu::Builder(arena)
                      .assertive_dependency_token(std::move(cpu_copy))
                      .Build());
}

void DriverRunner::AddExecutionStateDependency(
    AddExecutionStateDependencyRequestView request,
    AddExecutionStateDependencyCompleter::Sync& completer) {
  if (!request->has_dependency_token() || !request->has_power_level()) {
    completer.ReplyError(
        ::fuchsia_power_system::wire::AddExecutionStateDependencyError::kInvalidArgs);
    return;
  }

  if (!std::holds_alternative<CallbackSet>(storage_callbacks_or_token_)) {
    completer.ReplyError(fuchsia_power_system::wire::AddExecutionStateDependencyError::kBadState);
    return;
  }

  CreateStoragePowerElement(
      std::move(request->dependency_token()), request->power_level(),
      [completer = completer.ToAsync()]() mutable { completer.ReplySuccess(); });
}

void DriverRunner::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_power_system::CpuElementManager> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {}

void DriverRunner::PublishComponentRunner(component::OutgoingDirectory& outgoing) {
  zx::result result = runner_.Publish(outgoing);
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());

  result = memory_attributor_.Publish(outgoing);
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());

  result = outgoing.AddUnmanagedProtocol<fdf::CompositeNodeManager>(
      manager_bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure));
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());

  result = outgoing.AddUnmanagedProtocol<fuchsia_driver_token::NodeBusTopology>(
      bus_topo_bindings_.CreateHandler(this, dispatcher_, [](fidl::UnbindInfo info) {
        if (info.is_user_initiated() || info.is_peer_closed()) {
          return;
        }
        fdf_log::warn("Unexpected closure of NodeBusTopology: {}", info.FormatDescription());
      }));
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());

  result = outgoing.AddUnmanagedProtocol<fuchsia_driver_crash::CrashIntrospect>(
      crash_introspect_bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure));
  ZX_ASSERT_MSG(result.is_ok(), "%s", result.status_string());
}

zx::result<> DriverRunner::StartRootDriver(std::string_view url) {
  fdf::DriverPackageType package = cpp20::starts_with(url, kBootScheme)
                                       ? fdf::DriverPackageType::kBoot
                                       : fdf::DriverPackageType::kBase;
  bootup_tracker_->Start();
  WaitForBootup([this]() { this->OnBootupComplete(); });
  root_node_->set_driver_host_name_for_colocation("root");
  return StartDriver(*root_node_, url, package);
}

void DriverRunner::StartDevfsDriver(std::shared_ptr<driver_manager::Devfs>& devfs) {
  auto [controller_client, controller_request] = fidl::Endpoints<fcomponent::Controller>::Create();
  devfs->SetController(std::move(controller_client));

  std::vector<NodeOffer> offers;
  runner_.CreateDriverComponent(devfs, std::move(controller_request), "devfs_driver",
                                "fuchsia-boot:///devfs-driver#meta/devfs-driver.cm",
                                CollectionName(Collection::kBoot).get(), offers);
}

void DriverRunner::NewDriverAvailable(NewDriverAvailableCompleter::Sync& completer) {
  TryBindAllAvailable();
}

void DriverRunner::TryBindAllAvailable(NodeBindingInfoResultCallback result_callback) {
  bind_manager_.TryBindAllAvailable(std::move(result_callback));
}

zx::result<> DriverRunner::StartDriver(Node& node, std::string_view url,
                                       fdf::DriverPackageType package_type) {
  // Ensure `node`'s collection is equal to or higher ranked than its ancestor
  // nodes' collections. This is to avoid node components having a dependency
  // cycle with each other. For example, node components in the boot driver
  // collection depend on the devfs component which ultimately depends on all
  // components within the package driver collection. If a package driver
  // component depended on a component in the boot driver collection (a lower
  // ranked collection than the package driver collection) then a cyclic
  // dependency would occur.
  node.set_collection(ToCollection(node, package_type));
  node.set_driver_package_type(package_type);

  std::weak_ptr node_weak = node.shared_from_this();
  std::string url_string(url.data(), url.size());
  auto moniker = node.MakeComponentMoniker();
  bootup_tracker_->NotifyNewStartRequest(moniker, url_string, node.shared_from_this());

  node.PrepareDictionary([this, node_weak, moniker, url_string](zx::result<> result) {
    if (!result.is_ok()) {
      return;
    }

    std::shared_ptr node = node_weak.lock();
    if (!node) {
      return;
    }

    CallStartDriverOnRunner(runner_, *node, moniker, url_string, bootup_tracker_);
  });

  return zx::ok();
}

void DriverRunner::Bind(Node& node, std::shared_ptr<BindResultTracker> result_tracker) {
  BindToUrl(node, {}, std::move(result_tracker));
}

void DriverRunner::BindToUrl(Node& node, std::string_view driver_url_suffix,
                             std::shared_ptr<BindResultTracker> result_tracker) {
  bind_manager_.Bind(node, driver_url_suffix, std::move(result_tracker));
}

void DriverRunner::RebindComposite(std::string spec, std::optional<std::string> driver_url,
                                   fit::callback<void(zx::result<>)> callback) {
  composite_node_spec_manager_.Rebind(spec, driver_url, std::move(callback));
}

void DriverRunner::RebindCompositesWithDriver(const std::string& url,
                                              fit::callback<void(size_t)> complete_callback) {
  std::unordered_set<std::string> names;
  PerformBFS(root_node_, [&names, url](const std::shared_ptr<driver_manager::Node>& current) {
    if (current->type() == driver_manager::NodeType::kComposite && current->driver_url() == url) {
      fdf_log::debug("RebindCompositesWithDriver rebinding composite {}",
                     current->MakeComponentMoniker());
      names.insert(current->name());
      return false;
    }

    return true;
  });

  if (names.empty()) {
    complete_callback(0);
    return;
  }

  auto complete_wrapper = [complete_callback = std::move(complete_callback), count = names.size()](
                              zx::result<>) mutable { complete_callback(count); };

  std::shared_ptr<AsyncSharder> sharder =
      std::make_shared<AsyncSharder>(names.size(), std::move(complete_wrapper));

  for (const auto& name : names) {
    RebindComposite(name, std::nullopt,
                    [sharder](zx::result<>) mutable { sharder->CompleteShard(); });
  }
}

DriverHost* DriverRunner::GetDriverHost(std::string_view driver_host_name_for_colocation) {
  if (driver_host_name_for_colocation.empty()) {
    return nullptr;
  }
  for (auto& driver_host : driver_hosts_) {
    if (driver_host.name_for_colocation() == driver_host_name_for_colocation) {
      return &driver_host;
    }
  }
  return nullptr;
}

zx::result<DriverHost*> DriverRunner::CreateDriverHost(
    bool use_next_vdso, std::string_view driver_host_name_for_colocation) {
  auto endpoints = fidl::Endpoints<fio::Directory>::Create();
  std::string name;
  if (!driver_host_name_for_colocation.empty()) {
    std::string_view suffix = driver_host_name_for_colocation;
    suffix = suffix.starts_with("#") ? suffix.substr(1) : suffix;
    name = std::format("driver-host-{}", suffix);
  } else {
    name = std::format("driver-host-{}", next_driver_host_id_++);
  }

  std::shared_ptr<bool> connected = std::make_shared<bool>(false);
  auto create =
      CreateDriverHostComponent(name, std::move(endpoints.server), connected, use_next_vdso);
  if (create.is_error()) {
    return create.take_error();
  }

  auto client_end = component::ConnectAt<fdh::DriverHost>(endpoints.client);
  if (client_end.is_error()) {
    fdf_log::error("Failed to connect to service '{}': {}",
                   fidl::DiscoverableProtocolName<fdh::DriverHost>, client_end.status_string());
    return client_end.take_error();
  }

  auto loader_service_client = loader_service_factory_();
  if (loader_service_client.is_error()) {
    fdf_log::error("Failed to connect to service fuchsia.ldsvc/Loader: {}",
                   loader_service_client.status_string());
    return loader_service_client.take_error();
  }

  auto driver_host =
      std::make_unique<DriverHostComponent>(std::move(*client_end), dispatcher_, &driver_hosts_,
                                            connected, driver_host_name_for_colocation);
  auto result = driver_host->InstallLoader(std::move(*loader_service_client));
  if (result.is_error()) {
    fdf_log::error("Failed to install loader service: {}", result);
    return result.take_error();
  }

  auto driver_host_ptr = driver_host.get();
  driver_hosts_.push_back(std::move(driver_host));

  return zx::ok(driver_host_ptr);
}

void DriverRunner::CreateDriverHostDynamicLinker(
    std::string_view driver_host_name_for_colocation,
    fit::callback<void(zx::result<DriverHost*>)> completion_cb) {
  if (!dynamic_linker_args_.has_value()) {
    fdf_log::error("Dynamic linker was not available");
    completion_cb(zx::error(ZX_ERR_NOT_SUPPORTED));
    return;
  }

  auto endpoints = fidl::Endpoints<fio::Directory>::Create();

  auto client_end = component::ConnectAt<fdh::DriverHost>(endpoints.client);
  if (client_end.is_error()) {
    fdf_log::error("Failed to connect to service '{}': {}",
                   fidl::DiscoverableProtocolName<fdh::DriverHost>, client_end.status_string());
    completion_cb(client_end.take_error());
    return;
  }

  // TODO(https://fxbug.dev/349831408): for now we use the same driver host launcher client
  // channel for each driver host.
  if (!driver_host_launcher_.has_value()) {
    auto client = dynamic_linker_args_->linker_service_factory();
    if (client.is_error()) {
      fdf_log::error("Failed to create driver host launcher client");
      completion_cb(client.take_error());
      return;
    }
    driver_host_launcher_ = fidl::WireSharedClient<fuchsia_driver_loader::DriverHostLauncher>(
        std::move(*client), dispatcher_);
  }
  std::shared_ptr<bool> connected = std::make_shared<bool>(false);
  dynamic_linker_args_->driver_host_runner->StartDriverHost(
      driver_host_launcher_->Clone(), std::move(endpoints.server), connected,
      [this, completion_cb = std::move(completion_cb), client_end = std::move(client_end),
       connected = std::move(connected), name = std::string(driver_host_name_for_colocation)](
          zx::result<fidl::ClientEnd<fuchsia_driver_loader::DriverHost>> result) mutable {
        if (result.is_error()) {
          completion_cb(result.take_error());
          return;
        }

        auto driver_host = std::make_unique<DriverHostComponent>(
            std::move(*client_end), dispatcher_, &driver_hosts_, connected, name,
            std::move(*result));

        auto driver_host_ptr = driver_host.get();
        driver_hosts_.push_back(std::move(driver_host));
        completion_cb(zx::ok(driver_host_ptr));
      });
}

bool DriverRunner::IsDriverHostValid(DriverHost* driver_host) const {
  return driver_hosts_.find_if([driver_host](const DriverHostComponent& host) {
    return &host == driver_host;
  }) != driver_hosts_.end();
}

zx::result<std::string> DriverRunner::StartDriver(
    Node& node, fuchsia_driver_framework::wire::DriverInfo driver_info) {
  if (!driver_info.has_url()) {
    fdf_log::error("Failed to start driver for node '{}', the driver URL is missing", node.name());
    return zx::error(ZX_ERR_INTERNAL);
  }

  auto pkg_type =
      driver_info.has_package_type() ? driver_info.package_type() : fdf::DriverPackageType::kBase;
  auto result = StartDriver(node, driver_info.url().get(), pkg_type);
  if (result.is_error()) {
    return result.take_error();
  }
  return zx::ok(std::string(driver_info.url().get()));
}

zx::result<BindSpecResult> DriverRunner::BindToParentSpec(fidl::AnyArena& arena,
                                                          CompositeParents composite_parents,
                                                          std::weak_ptr<Node> node,
                                                          bool enable_multibind) {
  return this->composite_node_spec_manager_.BindParentSpec(arena, composite_parents, node,
                                                           enable_multibind);
}

void DriverRunner::FetchCpuToken() {
  if (cpu_element_client_.has_value() &&
      !std::holds_alternative<PowerDependencyToken>(cpu_callbacks_or_token_)) {
    cpu_element_client_.value()->GetCpuDependencyToken().Then(
        [this](fidl::Result<fuchsia_power_system::CpuElementManager::GetCpuDependencyToken>&
                   result) mutable {
          ZX_ASSERT_MSG(result.is_ok(), "Error getting CPU token %s",
                        result.error_value().FormatDescription().c_str());

          CallbackSet callbacks = std::move(std::get<CallbackSet>(cpu_callbacks_or_token_));
          cpu_callbacks_or_token_ = std::move(result->assertive_dependency_token().value());
          for (auto& callback : callbacks) {
            callback();
          }
        });
  }
}

void DriverRunner::CreatePowerElement(
    std::optional<fidl::ClientEnd<fuchsia_power_broker::Topology>> topology_client,
    std::string_view name, fuchsia_power_broker::DependencyToken element_token,
    std::vector<fuchsia_power_broker::DependencyToken> deps,
    fidl::ServerEnd<fuchsia_power_broker::ElementControl> control,
    fidl::ClientEnd<fuchsia_power_broker::ElementRunner> runner,
    fidl::ServerEnd<fuchsia_power_broker::Lessor> lessor, Collection for_collection,
    std::optional<fuchsia_power_broker::DependencyToken> cpu_token_override,
    std::optional<zx::eventpair> initial_lease_token, fit::callback<void(zx::result<bool>)> cb) {
  if (!SuspendEnabled() && !topology_client.has_value()) {
    cb(zx::ok(false));
    return;
  }

  PowerDependencyToken* cpu_token = std::get_if<PowerDependencyToken>(&cpu_callbacks_or_token_);
  if (!cpu_token && !cpu_token_override.has_value() && SuspendEnabled()) {
    std::get<CallbackSet>(cpu_callbacks_or_token_)
        .push_back(
            [weak_self = weak_from_this(), topology_client = std::move(topology_client), name,
             element_token = std::move(element_token), deps = std::move(deps),
             control = std::move(control), runner = std::move(runner), lessor = std::move(lessor),
             for_collection, cpu_token_override = std::move(cpu_token_override),
             initial_lease_token = std::move(initial_lease_token), cb = std::move(cb)]() mutable {
              auto self = weak_self.lock();
              if (!self) {
                return;
              }

              self->CreatePowerElement(
                  std::move(topology_client), name, std::move(element_token), std::move(deps),
                  std::move(control), std::move(runner), std::move(lessor), for_collection,
                  std::move(cpu_token_override), std::move(initial_lease_token), std::move(cb));
            });
    return;
  }

  // If this is not a boot driver and we don't have the storage token yet, create a callback to
  // re-invoked this method later.
  // This might happen because creation of the storage power element has multiple async operations,
  // therefore it is possible that a driver from storage loads before the element is created.
  PowerDependencyToken* token = std::get_if<PowerDependencyToken>(&storage_callbacks_or_token_);
  if (for_collection != Collection::kBoot && !token && SuspendEnabled()) {
    std::get<CallbackSet>(storage_callbacks_or_token_)
        .push_back(
            [weak_self = weak_from_this(), topology_client = std::move(topology_client), name,
             element_token = std::move(element_token), deps = std::move(deps),
             control = std::move(control), runner = std::move(runner), lessor = std::move(lessor),
             for_collection, cpu_token_override = std::move(cpu_token_override),
             initial_lease_token = std::move(initial_lease_token), cb = std::move(cb)]() mutable {
              auto self = weak_self.lock();
              if (!self) {
                return;
              }

              self->CreatePowerElement(
                  std::move(topology_client), name, std::move(element_token), std::move(deps),
                  std::move(control), std::move(runner), std::move(lessor), for_collection,
                  std::move(cpu_token_override), std::move(initial_lease_token), std::move(cb));
            });
    return;
  }

  std::vector<fuchsia_power_broker::LevelDependency> level_deps;
  for (fuchsia_power_broker::DependencyToken& dep : deps) {
    fuchsia_power_broker::DependencyToken clone;
    zx_status_t dupe_result = dep.duplicate(ZX_RIGHT_SAME_RIGHTS, &clone);
    if (dupe_result != ZX_OK) {
      cb(zx::error(dupe_result));
      return;
    }

    level_deps.push_back(CreateLevelDependency(1, std::move(clone), {1}));
  }

  std::optional<fuchsia_power_broker::DependencyToken> final_cpu_token;
  if (cpu_token_override.has_value()) {
    fuchsia_power_broker::DependencyToken clone;
    zx_status_t dupe_result =
        cpu_token_override->duplicate(ZX_RIGHT_SAME_RIGHTS, (zx::event*)&clone);
    ZX_ASSERT(dupe_result == ZX_OK);
    final_cpu_token = std::move(clone);
  } else if (SuspendEnabled()) {
    fuchsia_power_broker::DependencyToken clone;
    ZX_ASSERT(std::get<PowerDependencyToken>(cpu_callbacks_or_token_)
                  .duplicate(ZX_RIGHT_SAME_RIGHTS, (zx::event*)&clone) == ZX_OK);
    final_cpu_token = std::move(clone);
  }

  if (final_cpu_token.has_value()) {
    level_deps.push_back(CreateLevelDependency(1, std::move(final_cpu_token.value()), {1}));
  }

  // Any drivers that aren't from bootfs have a dependency on storage.
  if (for_collection != Collection::kBoot) {
    std::optional<fuchsia_power_broker::DependencyToken> token = StorageElementToken();

    ZX_ASSERT_MSG(token.has_value(),
                  "No storage token on power-enabled platform, is there a race?");

    level_deps.push_back(CreateLevelDependency(1, std::move(*token), {1}));
  }

  fuchsia_power_broker::ElementSchema schema = CreateElementSchema(
      name, /* initial_current_level */ 1, {0, 1}, std::move(lessor), std::move(control),
      std::move(runner), std::move(level_deps), std::move(initial_lease_token));

  // Select the right fidl client to use. If we found a Topology instance in
  // the driver's namespace, we want to use that one, otherwise use the
  // instance routed to driver manager.
  //
  // If we use the driver-specific one we make a client. In both cases we pass
  // a `shared_ptr` to the `AddElement` response callback. We need this for the
  // driver-specific case to guarantee the client lives long enough. In the
  // case we use the driver manager connection, this simply points to the
  // client held in DriverRunner.
  fidl::Client<fuchsia_power_broker::Topology>* topology_to_use = &power_topology_;
  std::shared_ptr<fidl::Client<fuchsia_power_broker::Topology>> driver_specific_topology;
  if (topology_client.has_value()) {
    driver_specific_topology = std::make_shared<fidl::Client<fuchsia_power_broker::Topology>>(
        std::move(topology_client.value()), dispatcher_);
    topology_to_use = driver_specific_topology.get();
  }

  (*topology_to_use)
      ->AddElement(std::move(schema))
      // Move a pointer to the client into the callback. In the case we're
      // using a client we just made above, this should keep it alive until
      // we've completed the work.
      .Then([cb = std::move(cb), topology_client = driver_specific_topology](
                fidl::Result<fuchsia_power_broker::Topology::AddElement>& add_result) mutable {
        if (add_result.is_error() && add_result.error_value().is_framework_error()) {
          cb(zx::error(add_result.error_value().framework_error().status()));
          return;
        }
        if (add_result.is_error()) {
          // This is a protocol error.
          switch (add_result.error_value().domain_error()) {
            case fuchsia_power_broker::AddElementError::kInvalid:
              cb(zx::error(ZX_ERR_INVALID_ARGS));
              return;
            case fuchsia_power_broker::AddElementError::kNotAuthorized:
              cb(zx::error(ZX_ERR_ACCESS_DENIED));
              return;
            default:
              cb(zx::error(ZX_ERR_INTERNAL));
              return;
          }
        }
        cb(zx::ok(true));
      });
}

void DriverRunner::OnBootupComplete() {
  // We only want to create the AllDrivers power element if suspend is enabled.
  if (!SuspendEnabled()) {
    return;
  }

  // If we need to wait and the storage element token isn't created yet, delay creating all drivers
  // until it's created.
  if (wait_for_storage_token_from_driver_ &&
      !std::holds_alternative<PowerDependencyToken>(storage_callbacks_or_token_)) {
    std::get<CallbackSet>(storage_callbacks_or_token_).push_back([weak_self = weak_from_this()]() {
      if (auto self = weak_self.lock(); self) {
        self->CreateAllDriversPowerElement();
      }
    });
    return;
  }
  CreateAllDriversPowerElement();
}

std::optional<fuchsia_power_broker::DependencyToken> DriverRunner::StorageElementToken() {
  PowerDependencyToken* token = std::get_if<PowerDependencyToken>(&storage_callbacks_or_token_);
  ZX_ASSERT_MSG(token, "Invalid state, storage token requested before being set.");

  zx::event copy;
  ZX_ASSERT(token->duplicate(ZX_RIGHT_SAME_RIGHTS, &copy) == ZX_OK);
  return fuchsia_power_broker::DependencyToken(std::move(copy));
}

void DriverRunner::RequestMatchFromDriverIndex(
    fuchsia_driver_index::wire::MatchDriverArgs args,
    fit::callback<void(fidl::WireUnownedResult<fdi::DriverIndex::MatchDriver>&)> match_callback) {
  driver_index()->MatchDriver(args).Then(std::move(match_callback));
}

void DriverRunner::RequestRebindFromDriverIndex(std::string spec,
                                                std::optional<std::string> driver_url_suffix,
                                                fit::callback<void(zx::result<>)> callback) {
  fidl::Arena allocator;
  fidl::StringView fidl_driver_url = driver_url_suffix == std::nullopt
                                         ? fidl::StringView()
                                         : fidl::StringView(allocator, driver_url_suffix.value());
  driver_index_->RebindCompositeNodeSpec(fidl::StringView(allocator, spec), fidl_driver_url)
      .Then(
          [callback = std::move(callback)](
              fidl::WireUnownedResult<fdi::DriverIndex::RebindCompositeNodeSpec>& result) mutable {
            if (!result.ok()) {
              fdf_log::error(
                  "Failed to send a composite rebind request to the Driver Index failed {}",
                  result.error().FormatDescription());
              callback(zx::error(result.status()));
              return;
            }

            if (result->is_error()) {
              callback(result->take_error());
              return;
            }
            callback(zx::ok());
          });
}

zx::result<> DriverRunner::CreateDriverHostComponent(
    std::string moniker, fidl::ServerEnd<fuchsia_io::Directory> exposed_dir,
    std::shared_ptr<bool> exposed_dir_connected, bool use_next_vdso) {
  constexpr std::string_view kUrl = "fuchsia-boot:///driver_host#meta/driver_host.cm";
  constexpr std::string_view kNextUrl = "fuchsia-boot:///driver_host#meta/driver_host_next.cm";
  fidl::Arena arena;
  auto child_decl_builder = fdecl::wire::Child::Builder(arena)
                                .name(moniker)
                                .url(use_next_vdso ? kNextUrl : kUrl)
                                .startup(fdecl::wire::StartupMode::kLazy);
  auto child_args_builder = fcomponent::wire::CreateChildArgs::Builder(arena);
  auto open_callback =
      [moniker](fidl::WireUnownedResult<fcomponent::Realm::OpenExposedDir>& result) {
        if (!result.ok()) {
          fdf_log::error("Failed to open exposed directory for driver host: '{}': {}", moniker,
                         result.FormatDescription());
          return;
        }
        if (result->is_error()) {
          fdf_log::error("Failed to open exposed directory for driver host: '{}': {}", moniker,
                         static_cast<uint32_t>(result->error_value()));
        }
      };
  auto create_callback =
      [this, moniker, exposed_dir = std::move(exposed_dir),
       exposed_dir_connected = std::move(exposed_dir_connected),
       open_callback = std::move(open_callback)](
          fidl::WireUnownedResult<fcomponent::Realm::CreateChild>& result) mutable {
        if (!result.ok()) {
          fdf_log::error("Failed to create driver host '{}': {}", moniker,
                         result.error().FormatDescription());
          return;
        }
        if (result->is_error()) {
          fdf_log::error("Failed to create driver host '{}': {}", moniker,
                         static_cast<uint32_t>(result->error_value()));
          return;
        }
        fdecl::wire::ChildRef child_ref{
            .name = fidl::StringView::FromExternal(moniker),
            .collection = "driver-hosts",
        };
        runner_.realm()
            ->OpenExposedDir(child_ref, std::move(exposed_dir))
            .ThenExactlyOnce(std::move(open_callback));
        *exposed_dir_connected = true;
      };
  runner_.realm()
      ->CreateChild(
          fdecl::wire::CollectionRef{
              .name = "driver-hosts",
          },
          child_decl_builder.Build(), child_args_builder.Build())
      .Then(std::move(create_callback));
  return zx::ok();
}

void DriverRunner::DestroyDriverHostComponent(std::string_view driver_host_name_for_colocation,
                                              fit::callback<void(zx::result<>)> completion_cb) {
  std::string name;
  if (!driver_host_name_for_colocation.empty()) {
    std::string_view suffix = driver_host_name_for_colocation;
    suffix = suffix.starts_with("#") ? suffix.substr(1) : suffix;
    name = std::format("driver-host-{}", suffix);
  } else {
    // Cannot reliably destroy an unnamed host here by name.
    completion_cb(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  fdecl::wire::ChildRef child_ref{
      .name = fidl::StringView::FromExternal(name),
      .collection = "driver-hosts",
  };
  runner_.realm()->DestroyChild(child_ref).Then(
      [completion_cb = std::move(completion_cb), moniker = std::move(name)](
          fidl::WireUnownedResult<fcomponent::Realm::DestroyChild>& result) mutable {
        if (!result.ok()) {
          fdf_log::error("Failed to destroy driver host '{}': {}", moniker,
                         result.FormatDescription());
          completion_cb(zx::error(result.status()));
          return;
        }
        if (result->is_error()) {
          // If the component has already been cleaned up by component_manager
          // or is not found, we treat it as success.
          if (result->error_value() == fcomponent::wire::Error::kInstanceNotFound ||
              result->error_value() == fcomponent::wire::Error::kInstanceDied) {
            completion_cb(zx::ok());
          } else {
            fdf_log::error("Failed to destroy driver host '{}': {}", moniker,
                           static_cast<uint32_t>(result->error_value()));
            completion_cb(zx::error(ZX_ERR_INTERNAL));
          }
          return;
        }
        completion_cb(zx::ok());
      });
}

zx::result<uint32_t> DriverRunner::RestartNodesColocatedWithDriverUrl(
    std::string_view url, fdd::RestartRematchFlags rematch_flags) {
  auto driver_hosts = DriverHostsWithDriverUrl(url);

  // Perform a BFS over the node topology, if the current node's host is one of the driver_hosts
  // we collected, then restart that node and skip its children since they will go away
  // as part of it's restart.
  //
  // The BFS ensures that we always find the topmost node of a driver host.
  // This node will by definition have colocated set to false, so when we call StartDriver
  // on this node we will always create a new driver host. The old driver host will go away
  // on its own asynchronously since it is drained from all of its drivers.
  PerformBFS(root_node_, [this, &driver_hosts, rematch_flags,
                          url](const std::shared_ptr<driver_manager::Node>& current) {
    if (driver_hosts.find(current->driver_host()) == driver_hosts.end()) {
      // Not colocated with one of the restarting hosts. Continue to visit the children.
      return true;
    }

    if (current->EvaluateRematchFlags(rematch_flags, url)) {
      if (current->type() == driver_manager::NodeType::kComposite) {
        // Composites need to go through a different flow that will fully remove the
        // node and empty out the composite spec management layer.
        fdf_log::debug("RestartNodesColocatedWithDriverUrl rebinding composite {}",
                       current->MakeComponentMoniker());
        RebindComposite(current->name(), std::nullopt, [](zx::result<>) {});
        return false;
      }

      // Non-composite nodes use the restart with rematch flow.
      fdf_log::debug("RestartNodesColocatedWithDriverUrl restarting node with rematch {}",
                     current->MakeComponentMoniker());
      current->RestartNodeWithRematch();
      return false;
    }

    // Not rematching, plain node restart.
    fdf_log::debug("RestartNodesColocatedWithDriverUrl restarting node {}",
                   current->MakeComponentMoniker());
    current->RestartNode();
    return false;
  });

  return zx::ok(static_cast<uint32_t>(driver_hosts.size()));
}

void DriverRunner::RestartWithDictionary(fidl::StringView moniker,
                                         fuchsia_component_sandbox::wire::DictionaryRef dictionary,
                                         zx::eventpair reset_eventpair) {
  dictionary_util_.ImportDictionaryWire(std::move(dictionary), [this,
                                                                moniker =
                                                                    std::string(moniker.get()),
                                                                reset_eventpair =
                                                                    std::move(reset_eventpair)](
                                                                   zx::result<
                                                                       fuchsia_component_sandbox::
                                                                           NewCapabilityId>
                                                                       result) mutable {
    if (result.is_error()) {
      return;
    }

    std::shared_ptr<driver_manager::Node> restarted_node = nullptr;
    PerformBFS(root_node_, [&](const std::shared_ptr<driver_manager::Node>& current) {
      if (current->MakeComponentMoniker() == moniker && current->HasDriverComponent()) {
        if (current->HasSubtreeDictionaryRef()) {
          fdf_log::error(
              "RestartWithDictionary requested node id already contains a dictionary_ref from another RestartWithDictionary operation.");
          return false;
        }
        ZX_ASSERT_MSG(restarted_node == nullptr, "Multiple nodes with same moniker not possible.");
        restarted_node = current;
        current->SetSubtreeDictionaryRef(result.value());
        current->RestartNode();
        return false;
      }

      return true;
    });

    if (restarted_node != nullptr) {
      std::weak_ptr<driver_manager::Node> weak_restarted_node = restarted_node;
      std::unique_ptr<async::WaitOnce> wait = std::make_unique<async::WaitOnce>(
          reset_eventpair.release(), ZX_EVENTPAIR_PEER_CLOSED | ZX_EVENTPAIR_SIGNALED);
      async::WaitOnce* wait_ptr = wait.get();
      zx_status_t status = wait_ptr->Begin(
          dispatcher_,
          [weak_restarted_node = std::move(weak_restarted_node), moved_wait = std::move(wait)](
              async_dispatcher_t* dispatcher, async::WaitOnce* wait, zx_status_t status,
              const zx_packet_signal_t* signal) {
            fdf_log::info("RestartWithDictionary operation released.");
            auto restarted_node = weak_restarted_node.lock();
            if (!restarted_node) {
              return;
            }
            restarted_node->SetSubtreeDictionaryRef(std::nullopt);
            restarted_node->RestartNode();
          });

      if (status != ZX_OK) {
        fdf_log::error("Failed to Begin async::Wait for RestartWithDictionary.");
      }
    }
  });
}

void DriverRunner::RestartWithDictionaryAndPowerDependencies(
    std::string moniker, fuchsia_component_sandbox::DictionaryRef dictionary,
    std::vector<fuchsia_power_broker::LevelDependency> power_dependencies,
    std::optional<zx::event> cpu_token_override, zx::eventpair release_fence) {
  dictionary_util_.ImportDictionary(std::move(dictionary), [this, moniker = std::move(moniker),
                                                            power_dependencies =
                                                                std::move(power_dependencies),
                                                            cpu_token_override =
                                                                std::move(cpu_token_override),
                                                            release_fence =
                                                                std::move(release_fence)](
                                                               zx::result<
                                                                   fuchsia_component_sandbox::
                                                                       NewCapabilityId>
                                                                   result) mutable {
    if (result.is_error()) {
      fdf_log::error(
          "Failed to import dictionary for RestartWithDictionaryAndPowerDependencies: {}",
          result.status_string());
      return;
    }

    std::shared_ptr<driver_manager::Node> restarted_node = nullptr;
    PerformBFS(root_node_, [&](const std::shared_ptr<driver_manager::Node>& current) {
      if (current->MakeComponentMoniker() == moniker && current->HasDriverComponent()) {
        if (current->HasSubtreeDictionaryRef()) {
          fdf_log::error(
              "RestartWithDictionaryAndPowerDependencies requested node id already contains a dictionary_ref from another restart operation.");
          return false;
        }
        ZX_ASSERT_MSG(restarted_node == nullptr, "Multiple nodes with same moniker not possible.");
        restarted_node = current;
        current->SetSubtreeDictionaryRef(result.value());

        // Clone power dependencies for the node
        std::vector<fuchsia_power_broker::LevelDependency> deps;
        for (const auto& dep : power_dependencies) {
          if (!dep.requires_token().has_value() || !dep.dependent_level().has_value() ||
              !dep.requires_level_by_preference().has_value()) {
            fdf_log::warn("Power dependency is invalid, skipping.");
            continue;
          }
          fuchsia_power_broker::DependencyToken clone;
          zx_status_t status = dep.requires_token()->duplicate(ZX_RIGHT_SAME_RIGHTS, &clone);
          if (status != ZX_OK) {
            fdf_log::error("Failed to duplicate power token: {}", zx_status_get_string(status));
            continue;
          }
          deps.push_back(CreateLevelDependency(dep.dependent_level().value(), std::move(clone),
                                               dep.requires_level_by_preference().value()));
        }
        current->SetPowerDependencyOverrides(std::move(deps));

        if (cpu_token_override.has_value()) {
          zx::event clone;
          zx_status_t status = cpu_token_override->duplicate(ZX_RIGHT_SAME_RIGHTS, &clone);
          if (status == ZX_OK) {
            current->SetCpuTokenOverride(std::move(clone));
          } else {
            fdf_log::error("Failed to duplicate CPU token override: {}",
                           zx_status_get_string(status));
          }
        }

        current->RestartNode();
        return false;
      }

      return true;
    });

    if (restarted_node != nullptr) {
      std::weak_ptr<driver_manager::Node> weak_restarted_node = restarted_node;
      std::unique_ptr<async::WaitOnce> wait = std::make_unique<async::WaitOnce>(
          release_fence.release(), ZX_EVENTPAIR_PEER_CLOSED | ZX_EVENTPAIR_SIGNALED);
      async::WaitOnce* wait_ptr = wait.get();
      zx_status_t status = wait_ptr->Begin(
          dispatcher_,
          [weak_restarted_node = std::move(weak_restarted_node), moved_wait = std::move(wait)](
              async_dispatcher_t* dispatcher, async::WaitOnce* wait, zx_status_t status,
              const zx_packet_signal_t* signal) {
            fdf_log::info("RestartWithDictionaryAndPowerDependencies operation released.");
            auto restarted_node = weak_restarted_node.lock();
            if (!restarted_node) {
              return;
            }
            restarted_node->SetSubtreeDictionaryRef(std::nullopt);
            restarted_node->SetPowerDependencyOverrides(std::nullopt);
            restarted_node->SetCpuTokenOverride(std::nullopt);
            restarted_node->RestartNode();
          });

      if (status != ZX_OK) {
        fdf_log::error(
            "Failed to Begin async::Wait for RestartWithDictionaryAndPowerDependencies.");
      }
    }
  });
}

std::unordered_set<const DriverHost*> DriverRunner::DriverHostsWithDriverUrl(std::string_view url) {
  std::unordered_set<const DriverHost*> result_hosts;

  // Perform a BFS over the node topology, if the current node's driver url is the url we are
  // interested in, add the driver host it is in to the result set.
  PerformBFS(root_node_,
             [&result_hosts, url](const std::shared_ptr<driver_manager::Node>& current) {
               if (current->driver_url() == url) {
                 result_hosts.insert(current->driver_host());
               }
               return true;
             });

  return result_hosts;
}

}  // namespace driver_manager
