// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/hlcpp_conversion.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.driver.development/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/driver_test_realm/realm_builder/cpp/builder.h>
#include <lib/driver_test_realm/src/internal_server.h>
#include <lib/fdio/directory.h>

#include <fstream>

namespace driver_test_realm {

OptionsBuilder& OptionsBuilder::set_dtr_offers_provider(component_testing::Ref provider) {
  options_.dtr_offers_provider = provider;
  return *this;
}

OptionsBuilder& OptionsBuilder::set_boot_items_to_tunnel(component_testing::Ref items) {
  options_.boot_items_to_tunnel = items;
  return *this;
}

OptionsBuilder& OptionsBuilder::set_trace_provider(component_testing::Ref provider) {
  options_.trace_provider = provider;
  return *this;
}

using component_testing::Capability;
using component_testing::ChildRef;
using component_testing::CollectionRef;
using component_testing::Config;
using component_testing::ConfigCapability;
using component_testing::ConfigValue;
using component_testing::Dictionary;
using component_testing::Directory;
using component_testing::LocalComponentImpl;
using component_testing::ParentRef;
using component_testing::Protocol;
using component_testing::RealmBuilder;
using component_testing::RealmRoot;
using component_testing::Ref;
using component_testing::Resolver;
using component_testing::Route;
using component_testing::Runner;
using component_testing::SelfRef;
using component_testing::Service;
using component_testing::Storage;
using component_testing::VoidRef;

namespace {

Capability ConvertCapability(fuchsia_component_test::Capability capability) {
  std::optional<Capability> converted;
  switch (capability.Which()) {
    case fuchsia_component_test::Capability::Tag::kProtocol: {
      std::optional<fuchsia::component::decl::DependencyType> type;
      if (capability.protocol()->type().has_value()) {
        type = static_cast<fuchsia::component::decl::DependencyType>(
            capability.protocol()->type().value());
      }
      std::optional<fuchsia::component::decl::Availability> availability;
      if (capability.protocol()->availability().has_value()) {
        availability = static_cast<fuchsia::component::decl::Availability>(
            capability.protocol()->availability().value());
      }
      converted = Protocol{
          .name = capability.protocol()->name().value(),
          .as = capability.protocol()->as(),
          .type = type,
          .path = capability.protocol()->path(),
          .availability = availability,
      };
      break;
    }
    case fuchsia_component_test::Capability::Tag::kDirectory: {
      std::optional<fuchsia::component::decl::DependencyType> type;
      if (capability.directory()->type().has_value()) {
        type = static_cast<fuchsia::component::decl::DependencyType>(
            capability.directory()->type().value());
      }
      std::optional<fuchsia::io::Operations> rights;
      if (capability.directory()->rights().has_value()) {
        rights = static_cast<fuchsia::io::Operations>(
            static_cast<uint64_t>(capability.directory()->rights().value()));
      }
      std::optional<fuchsia::component::decl::Availability> availability;
      if (capability.directory()->availability().has_value()) {
        availability = static_cast<fuchsia::component::decl::Availability>(
            capability.directory()->availability().value());
      }

      converted = Directory{
          .name = capability.directory()->name().value(),
          .as = capability.directory()->as(),
          .type = type,
          .subdir = capability.directory()->subdir(),
          .rights = rights,
          .path = capability.directory()->path(),
          .availability = availability,
      };
      break;
    }
    case fuchsia_component_test::Capability::Tag::kStorage: {
      std::optional<fuchsia::component::decl::Availability> availability;
      if (capability.storage()->availability().has_value()) {
        availability = static_cast<fuchsia::component::decl::Availability>(
            capability.storage()->availability().value());
      }
      converted = Storage{
          .name = capability.storage()->name().value(),
          .as = capability.storage()->as(),
          .path = capability.storage()->path(),
          .availability = availability,
      };
      break;
    }
    case fuchsia_component_test::Capability::Tag::kService: {
      std::optional<fuchsia::component::decl::Availability> availability;
      if (capability.service()->availability().has_value()) {
        availability = static_cast<fuchsia::component::decl::Availability>(
            capability.service()->availability().value());
      }
      converted = Service{
          .name = capability.service()->name().value(),
          .as = capability.service()->as(),
          .path = capability.service()->path(),
          .availability = availability,
      };
      break;
    }
    case fuchsia_component_test::Capability::Tag::kEventStream: {
      ZX_ASSERT_MSG(false, "EventStream capability not supported here.");
      break;
    }
    case fuchsia_component_test::Capability::Tag::kConfig: {
      std::optional<fuchsia::component::decl::Availability> availability;
      if (capability.config()->availability().has_value()) {
        availability = static_cast<fuchsia::component::decl::Availability>(
            capability.config()->availability().value());
      }
      converted = Config{
          .name = capability.config()->name().value(),
          .as = capability.config()->as(),
          .availability = availability,
      };
      break;
    }
    case fuchsia_component_test::Capability::Tag::kDictionary: {
      std::optional<fuchsia::component::decl::Availability> availability;
      if (capability.dictionary()->availability().has_value()) {
        availability = static_cast<fuchsia::component::decl::Availability>(
            capability.dictionary()->availability().value());
      }
      converted = Dictionary{
          .name = capability.dictionary()->name().value(),
          .as = capability.dictionary()->as(),
          .availability = availability,
      };
      break;
    }
    case fuchsia_component_test::Capability::Tag::kResolver: {
      converted = Resolver{
          .name = capability.resolver()->name().value(),
          .as = capability.resolver()->as(),
          .path = capability.resolver()->path(),
      };
      break;
    }
    case fuchsia_component_test::Capability::Tag::kRunner: {
      converted = Runner{
          .name = capability.runner()->name().value(),
          .as = capability.runner()->as(),
          .path = capability.runner()->path(),
      };
      break;
    }
    default:
      break;
  }

  return converted.value();
}

class InternalServerComponent final
    : public LocalComponentImpl,
      public fidl::WireServer<fuchsia_driver_test::ResourceProvider> {
 public:
  InternalServerComponent(async_dispatcher_t* dispatcher,
                          std::shared_ptr<driver_test_realm::InternalServer> server,
                          std::shared_ptr<std::optional<zx::vmo>> devicetree)
      : dispatcher_(dispatcher), server_(std::move(server)), devicetree_(std::move(devicetree)) {}
  void OnStart() override {
    outgoing()->AddProtocol<fuchsia_driver_test::Internal>(
        [this](fidl::ServerEnd<fuchsia_driver_test::Internal> server_end) {
          server_->Serve(dispatcher_, std::move(server_end));
        });

    outgoing()->AddProtocol<fuchsia_driver_test::ResourceProvider>(
        resource_provider_bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure));
  }

  void GetDeviceTree(GetDeviceTreeCompleter::Sync& completer) override {
    if ((*devicetree_).has_value()) {
      zx::vmo out;
      zx_status_t status = (*devicetree_)->duplicate(ZX_RIGHT_SAME_RIGHTS, &out);
      if (status == ZX_OK) {
        completer.ReplySuccess(std::move(out));
        return;
      }
    }
    completer.ReplyError(ZX_ERR_NOT_FOUND);
  }

 private:
  async_dispatcher_t* dispatcher_;
  std::shared_ptr<driver_test_realm::InternalServer> server_;
  std::shared_ptr<std::optional<zx::vmo>> devicetree_;
  fidl::ServerBindingGroup<fuchsia_driver_test::ResourceProvider> resource_provider_bindings_;
};

}  // namespace

void Setup(RealmBuilder& realm_builder, async_dispatcher_t* dispatcher,
           fuchsia_driver_test::RealmArgs args, Options options) {
  auto manifest_provider = component::Connect<fuchsia_driver_test::ManifestProvider>();
  ZX_ASSERT(manifest_provider.is_ok());

  auto manifest_result = fidl::Call(*manifest_provider)->GetManifest();
  ZX_ASSERT(manifest_result.is_ok());

  std::vector<uint8_t> manifest;
  uint8_t buffer[fuchsia_io::kMaxBuf];
  zx_iovec_t vec = {
      .buffer = buffer,
      .capacity = fuchsia_io::kMaxBuf,
  };
  size_t read = 1;
  while (read != 0) {
    zx_status_t read_result = manifest_result->manifest().readv(0, &vec, 1, &read);
    ZX_ASSERT_MSG(read_result == ZX_OK, "Failed to read manifest stream.");
    manifest.insert(manifest.end(), buffer, buffer + read);
  }

  auto component = fidl::Unpersist<fuchsia_component_decl::Component>(manifest);
  ZX_ASSERT(component.is_ok());
  fuchsia::component::decl::Component hlcpp_component =
      fidl::NaturalToHLCPP(std::move(component.value()));

  // Keep the rust and c++ realm_builder setups in sync.
  // LINT.IfChange
  auto realm = realm_builder.AddChildRealmFromDecl("driver_test_realm", hlcpp_component);
  Ref dtr_realm_ref = ChildRef{"driver_test_realm"};

  // From the test root into the dtr.
  realm_builder.AddRoute(Route{
      .capabilities =
          std::vector<Capability>{
              Protocol{.name = "fuchsia.diagnostics.ArchiveAccessor"},
          },
      .source = {ParentRef{}},
      .targets = {dtr_realm_ref},
  });

  // Setup the trace provider.
  realm_builder.AddRoute(Route{
      .capabilities =
          std::vector<Capability>{
              Protocol{
                  .name = "fuchsia.tracing.provider.Registry",
                  .availability = fuchsia::component::decl::Availability::OPTIONAL,
              },
          },
      .source = {options.trace_provider ? options.trace_provider.value() : VoidRef{}},
      .targets = {dtr_realm_ref},
  });

  Ref driver_manager = ChildRef{"driver_manager"};
  Ref fake_resolver = ChildRef{"fake_resolver"};
  Ref driver_index = ChildRef{"driver_index"};
  Ref dtr_support = ChildRef{"dtr_support"};

  Ref boot_drivers = CollectionRef{"boot-drivers"};
  Ref base_drivers = CollectionRef{"base-drivers"};
  Ref full_drivers = CollectionRef{"full-drivers"};

  // Get the test component information.
  fuchsia_component_resolution::Component test_component;
  if (args.test_component().has_value()) {
    test_component = std::move(args.test_component().value());
  } else {
    auto component_realm = component::Connect<fuchsia_component::Realm>();
    fidl::SyncClient<fuchsia_component::Realm> realm_client(std::move(component_realm.value()));
    auto resolved_info = realm_client->GetResolvedInfo();
    ZX_ASSERT(resolved_info.is_ok());

    test_component = std::move(resolved_info.value().resolved_info());
  }

  // Route the resolvers from the test root.
  realm_builder.AddRoute(Route{
      .capabilities =
          std::vector<Capability>{
              Protocol{
                  .name = "fuchsia.component.resolution.Resolver-hermetic",
                  .availability = fuchsia::component::decl::Availability::OPTIONAL,
              },
              Protocol{
                  .name = "fuchsia.pkg.PackageResolver-hermetic",
                  .availability = fuchsia::component::decl::Availability::OPTIONAL,
              },
          },
      .source = {ParentRef{}},
      .targets = {dtr_realm_ref},
  });

  // Setup the local Internal protocol server child.
  std::optional<std::vector<std::string>> boot_driver_components =
      std::move(args.boot_driver_components());

  std::shared_ptr<driver_test_realm::InternalServer> internal_server =
      std::make_shared<driver_test_realm::InternalServer>(
          std::move(args.boot()).value_or(fidl::ClientEnd<fuchsia_io::Directory>{}),
          fidl::ClientEnd<fuchsia_io::Directory>{},
          std::move(test_component.package()->directory()).value(),
          std::move(test_component.resolution_context()).value(),
          std::move(boot_driver_components));

  std::shared_ptr<std::optional<zx::vmo>> devicetree =
      std::make_shared<std::optional<zx::vmo>>(std::move(args.devicetree()));
  realm.AddLocalChild("driver_test_internal",
                      [dispatcher, internal_server = std::move(internal_server), devicetree]() {
                        auto impl = std::make_unique<InternalServerComponent>(
                            dispatcher, internal_server, devicetree);
                        return impl;
                      });

  Ref driver_test_internal = ChildRef{"driver_test_internal"};

  // Provide offers from the dtr_offers_provider, if the test provides one, to the driver
  // collections.
  if (args.dtr_offers().has_value() && options.dtr_offers_provider.has_value()) {
    for (const auto& offer : args.dtr_offers().value()) {
      realm_builder.AddRoute(Route{
          .capabilities = {ConvertCapability(offer)},
          .source = {options.dtr_offers_provider.value()},
          .targets = {dtr_realm_ref},
      });
      realm.AddRoute(Route{
          .capabilities = {ConvertCapability(offer)},
          .source = {ParentRef{}},
          .targets =
              {
                  boot_drivers,
                  base_drivers,
                  full_drivers,
              },
      });
    }
  } else if (args.dtr_offers().has_value() || options.dtr_offers_provider.has_value()) {
    ZX_ASSERT_MSG(false, "Must provide |args.dtr_offers| and |dtr_offers_provider| together.");
  }

  // Provide exposes from the driver collections to the test.
  if (args.dtr_exposes().has_value()) {
    for (const auto& expose : args.dtr_exposes().value()) {
      realm.AddRoute(Route{
          .capabilities = {ConvertCapability(expose)},
          .source = {boot_drivers},
          .targets = {ParentRef{}},
      });
      realm.AddRoute(Route{
          .capabilities = {ConvertCapability(expose)},
          .source = {base_drivers},
          .targets = {ParentRef{}},
      });
      realm.AddRoute(Route{
          .capabilities = {ConvertCapability(expose)},
          .source = {full_drivers},
          .targets = {ParentRef{}},
      });

      realm_builder.AddRoute(Route{
          .capabilities = {ConvertCapability(expose)},
          .source = {dtr_realm_ref},
          .targets = {ParentRef{}},
      });
    }
  }

  // Setup boot items, either tunneled from boot_items_to_tunnel, if the test provides one, or
  // tunneling is disabled, in which case the dtr_support provides a stand-in implementation.
  {
    if (options.boot_items_to_tunnel.has_value()) {
      realm_builder.AddRoute(Route{
          .capabilities = {Protocol{
              .name = "fuchsia.boot.Items",
              .availability = fuchsia::component::decl::Availability::OPTIONAL,
          }},
          .source = {options.boot_items_to_tunnel.value()},
          .targets = {dtr_realm_ref},
      });

      realm.AddRoute(Route{
          .capabilities = {Protocol{
              .name = "fuchsia.boot.Items",
              .availability = fuchsia::component::decl::Availability::OPTIONAL,
          }},
          .source = {ParentRef{}},
          .targets = {dtr_support},
      });

    } else {
      realm.AddRoute(Route{
          .capabilities = {Protocol{
              .name = "fuchsia.boot.Items",
              .availability = fuchsia::component::decl::Availability::OPTIONAL,
          }},
          .source = {VoidRef{}},
          .targets = {dtr_support},
      });
    }
  }

  // Setup the driver test resource provider.
  realm.AddRoute(Route{
      .capabilities = {Protocol{.name = "fuchsia.driver.test.ResourceProvider"}},
      .source = {driver_test_internal},
      .targets = {dtr_support},
  });

  // Setup various basic config capabilities.
  {
    std::string vid;
    if (args.platform_vid()) {
      vid = std::format("{}", args.platform_vid().value());
    }

    std::string pid;
    if (args.platform_pid()) {
      pid = std::format("{}", args.platform_pid().value());
    }

    std::vector<ConfigCapability> configs;
    configs.push_back({
        .name = "fuchsia.driver.testrealm.TunnelBootItems",
        .value = ConfigValue::Bool(options.boot_items_to_tunnel.has_value()),
    });

    configs.push_back({
        .name = "fuchsia.driver.testrealm.BoardName",
        .value = ConfigValue(args.board_name().value_or("")),
    });
    configs.push_back({
        .name = "fuchsia.driver.testrealm.PlatformVid",
        .value = ConfigValue(vid),
    });
    configs.push_back({
        .name = "fuchsia.driver.testrealm.PlatformPid",
        .value = ConfigValue(pid),
    });
    configs.push_back({
        .name = "fuchsia.driver.BindEager",
        .value = ConfigValue(args.driver_bind_eager().value_or(std::vector<std::string>{})),
    });
    configs.push_back({
        .name = "fuchsia.driver.DisabledDrivers",
        .value = ConfigValue(args.driver_disable().value_or(std::vector<std::string>{})),
    });
    configs.push_back({
        .name = "fuchsia.driver.index.StopOnIdleTimeoutMillis",
        .value = ConfigValue::Int64(args.driver_index_stop_timeout_millis().value_or(-1)),
    });
    configs.push_back({
        .name = "fuchsia.driver.manager.RootDriver",
        .value =
            ConfigValue(args.root_driver().value_or("fuchsia-boot:///dtr#meta/test-parent-sys.cm")),
    });
    realm.AddConfiguration(std::move(configs));
  }

  // Setup software device config capabilities.
  Ref software_dev_source = VoidRef{};
  {
    if (args.software_devices().has_value()) {
      std::vector<ConfigCapability> configs;
      software_dev_source = SelfRef{};
      std::vector<std::string> device_names;
      std::vector<uint32_t> device_ids;
      for (const auto& device : args.software_devices().value()) {
        device_names.push_back(device.device_name());
        device_ids.push_back(device.device_id());
      }
      configs.push_back({
          .name = "fuchsia.platform.bus.SoftwareDeviceNames",
          .value = ConfigValue(device_names),
      });
      configs.push_back({
          .name = "fuchsia.platform.bus.SoftwareDeviceIds",
          .value = ConfigValue(device_ids),
      });
      realm.AddConfiguration(std::move(configs));
    }
  }

  // Config routes.
  {
    realm.AddRoute(Route{
        .capabilities =
            {
                Config{.name = "fuchsia.driver.BindEager"},
                Config{.name = "fuchsia.driver.DisabledDrivers"},
                Config{.name = "fuchsia.driver.index.StopOnIdleTimeoutMillis"},
            },
        .source = SelfRef{},
        .targets = {driver_index},
    });

    realm.AddRoute(Route{
        .capabilities = {Config{.name = "fuchsia.driver.manager.RootDriver"}},
        .source = SelfRef{},
        .targets = {driver_manager},
    });

    realm.AddRoute(Route{
        .capabilities =
            {
                Config{.name = "fuchsia.driver.testrealm.TunnelBootItems"},
                Config{.name = "fuchsia.driver.testrealm.BoardName"},
                Config{.name = "fuchsia.driver.testrealm.PlatformVid"},
                Config{.name = "fuchsia.driver.testrealm.PlatformPid"},
            },
        .source = SelfRef{},
        .targets = {dtr_support},
    });

    realm.AddRoute(Route{
        .capabilities =
            {
                Config{
                    .name = "fuchsia.platform.bus.SoftwareDeviceNames",
                    .availability = fuchsia::component::decl::Availability::OPTIONAL,
                },
                Config{
                    .name = "fuchsia.platform.bus.SoftwareDeviceIds",
                    .availability = fuchsia::component::decl::Availability::OPTIONAL,
                },
            },
        .source = software_dev_source,
        .targets = {boot_drivers},
    });
  }

  // Dynamic routes to the driver framework children.
  {
    realm.AddRoute(Route{
        .capabilities = {Protocol{.name = "fuchsia.driver.test.Internal"}},
        .source = {driver_test_internal},
        .targets = {fake_resolver},
    });
  }

  // Routes from the driver framework children out to the test.
  {
    realm_builder.AddRoute(Route{
        .capabilities =
            {
                Directory{.name = "dev-class"},
                Directory{.name = "dev-topological"},
                Protocol{.name = "fuchsia.driver.registrar.DriverRegistrar"},
                Protocol{.name = "fuchsia.driver.development.Manager"},
                Protocol{.name = "fuchsia.driver.framework.CompositeNodeManager"},
                Protocol{.name = "fuchsia.system.state.Administrator"},
            },
        .source = {dtr_realm_ref},
        .targets = {ParentRef{}},
    });
  }
  // LINT.ThenChange(/sdk/lib/driver_test_realm/realm_builder/rust/src/builder.rs)
}

zx::result<> WaitForBootup(RealmRoot& realm_root) {
  // Connect to the driver manager and wait for boot up to complete before returning.
  auto manager = component::ConnectAt<fuchsia_driver_development::Manager>(
      fidl::UnownedClientEnd<fuchsia_io::Directory>(
          realm_root.component().exposed().unowned_channel()));

  if (manager.is_error()) {
    return manager.take_error();
  }

  fidl::SyncClient<fuchsia_driver_development::Manager> development_manager_client(
      *std::move(manager));

  fidl::Result<fuchsia_driver_development::Manager::WaitForBootup> result =
      development_manager_client->WaitForBootup();
  if (result.is_error()) {
    return zx::error(ZX_ERR_INTERNAL);
  }

  return zx::ok();
}

}  // namespace driver_test_realm
