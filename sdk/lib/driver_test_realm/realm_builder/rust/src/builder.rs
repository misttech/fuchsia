// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Result};
use fidl::HandleBased;

use cm_rust::FidlIntoNative;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::{
    Capability, ChildOptions, ChildRef, CollectionRef, LocalComponentHandles, RealmBuilder,
    RealmInstance, Ref, Route,
};
use futures::{StreamExt, TryStreamExt};
use std::sync::Arc;
use {
    fidl_fuchsia_component_decl as fdecl, fidl_fuchsia_driver_development as fdd,
    fidl_fuchsia_driver_test as fdt, fidl_fuchsia_io as fio, fuchsia_async as fasync,
};

fn clone(
    dir: &fio::DirectoryProxy,
) -> Result<fidl::endpoints::ClientEnd<fio::DirectoryMarker>, fidl::Error> {
    let (client_end, server_end) = fidl::endpoints::create_endpoints::<fio::DirectoryMarker>();
    dir.clone(fidl::endpoints::ServerEnd::new(server_end.into_channel()))?;
    Ok(client_end)
}

async fn internal_serve(
    stream: fidl_fuchsia_driver_test::InternalRequestStream,
    test_pkg_dir: Arc<fio::DirectoryProxy>,
    test_resolution_context: Arc<Option<fidl_fuchsia_component_resolution::Context>>,
    boot_dir: Arc<Option<fio::DirectoryProxy>>,
    boot_driver_components: Arc<Option<Vec<String>>>,
) {
    stream
        .try_for_each_concurrent(None, |request| {
            let test_pkg_dir = test_pkg_dir.clone();
            let test_resolution_context = test_resolution_context.clone();
            let boot_dir = boot_dir.clone();
            let boot_driver_components = boot_driver_components.clone();
            async move {
                match request {
                    fidl_fuchsia_driver_test::InternalRequest::GetTestPackage { responder } => {
                        let cloned = clone(test_pkg_dir.as_ref())?;
                        responder.send(Ok(Some(cloned)))?;
                    }
                    fidl_fuchsia_driver_test::InternalRequest::GetTestResolutionContext {
                        responder,
                    } => responder.send(Ok(test_resolution_context.as_ref().as_ref()))?,
                    fidl_fuchsia_driver_test::InternalRequest::GetBootDirectory { responder } => {
                        match boot_dir.as_ref() {
                            Some(boot_dir) => {
                                let cloned = clone(boot_dir)?;
                                responder.send(Ok(Some(cloned)))?;
                            }
                            None => responder.send(Ok(None))?,
                        }
                    }
                    fidl_fuchsia_driver_test::InternalRequest::GetBootDriverOverrides {
                        responder,
                    } => responder.send(Ok(boot_driver_components
                        .as_ref()
                        .clone()
                        .unwrap_or(vec![])
                        .as_slice()))?,
                };
                Ok(())
            }
        })
        .await
        .expect("fuchsia.driver.test.Internal failed.");
}

async fn resource_provider_serve(
    stream: fidl_fuchsia_driver_test::ResourceProviderRequestStream,
    devicetree: Arc<Option<zx::Vmo>>,
) {
    stream
        .try_for_each_concurrent(None, |request| {
            let devicetree = devicetree.clone();
            async move {
                match request {
                    fidl_fuchsia_driver_test::ResourceProviderRequest::GetDeviceTree {
                        responder,
                    } => {
                        if let Some(vmo) = devicetree.as_ref().as_ref().map(|d| {
                            d.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate")
                        }) {
                            responder.send(Ok(vmo))?;
                        } else {
                            responder.send(Err(zx::Status::NOT_FOUND.into_raw()))?;
                        }
                    }
                };
                Ok(())
            }
        })
        .await
        .expect("fuchsia.driver.test.Internal failed.");
}

async fn run_internal_server(
    handles: LocalComponentHandles,
    test_pkg_dir: Arc<fio::DirectoryProxy>,
    test_resolution_context: Arc<Option<fidl_fuchsia_component_resolution::Context>>,
    boot_dir: Arc<Option<fio::DirectoryProxy>>,
    boot_driver_components: Arc<Option<Vec<String>>>,
    devicetree: Arc<Option<zx::Vmo>>,
) -> Result<()> {
    let mut fs = ServiceFs::new();

    fs.dir("svc").add_fidl_service(
        move |stream: fidl_fuchsia_driver_test::InternalRequestStream| {
            fasync::Task::spawn(internal_serve(
                stream,
                test_pkg_dir.clone(),
                test_resolution_context.clone(),
                boot_dir.clone(),
                boot_driver_components.clone(),
            ))
            .detach();
        },
    );
    fs.dir("svc").add_fidl_service(
        move |stream: fidl_fuchsia_driver_test::ResourceProviderRequestStream| {
            fasync::Task::spawn(resource_provider_serve(stream, devicetree.clone())).detach();
        },
    );
    fs.serve_connection(handles.outgoing_dir)?;
    fs.collect::<()>().await;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    dtr_offers_provider: Option<Ref>,
    boot_items_to_tunnel: Option<Ref>,
    trace_provider: Option<Ref>,
}

impl Options {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_offers_provider(mut self, dtr_offers_provider: Ref) -> Self {
        self.dtr_offers_provider = Some(dtr_offers_provider);
        self
    }

    pub fn with_boot_items_to_tunnel(mut self, boot_items_to_tunnel: Ref) -> Self {
        self.boot_items_to_tunnel = Some(boot_items_to_tunnel);
        self
    }

    pub fn with_trace_provider(mut self, trace_provider: Ref) -> Self {
        self.trace_provider = Some(trace_provider);
        self
    }
}

#[async_trait::async_trait]
pub trait DriverTestRealmBuilder {
    /// Set up the driver test realm.
    /// This version is a collapsed version compared to the previous version which used a two layer
    /// realm builder setup.
    async fn driver_test_realm_setup(
        &self,
        args: fdt::RealmArgs,
        options: Options,
    ) -> Result<&Self>;
}

#[async_trait::async_trait]
impl DriverTestRealmBuilder for RealmBuilder {
    async fn driver_test_realm_setup(
        &self,
        args: fdt::RealmArgs,
        options: Options,
    ) -> Result<&Self> {
        let manifest = std::fs::read("pkg/meta/driver_test_realm_base.cm")?;
        let component = fidl::unpersist::<fdecl::Component>(manifest.as_slice())
            .context("unpersisting the manifest vector")?;

        // Keep the rust and c++ realm_builder setups in sync.
        // LINT.IfChange
        let realm = self
            .add_child_realm_from_decl(
                "driver_test_realm",
                component.fidl_into_native(),
                ChildOptions::new(),
            )
            .await?;

        // From the test root into the dtr.
        self.add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.diagnostics.ArchiveAccessor"))
                .from(Ref::parent())
                .to(&realm),
        )
        .await?;

        // Setup the trace provider.
        let trace_provider = match options.trace_provider {
            Some(provider) => provider,
            None => Ref::void(),
        };

        self.add_route(
            Route::new()
                .capability(
                    Capability::protocol_by_name("fuchsia.tracing.provider.Registry").optional(),
                )
                .from(trace_provider)
                .to(&realm),
        )
        .await?;

        let dtr_support: ChildRef = "dtr_support".into();
        let fake_resolver: ChildRef = "fake_resolver".into();
        let driver_manager: ChildRef = "driver_manager".into();
        let driver_index: ChildRef = "driver_index".into();

        let boot_drivers: CollectionRef = "boot-drivers".into();
        let base_drivers: CollectionRef = "base-drivers".into();
        let full_drivers: CollectionRef = "full-drivers".into();

        let devicetree = Arc::new(args.devicetree);

        // Get the test component information.
        let test_component = if let Some(test_component) = args.test_component {
            test_component
        } else {
            let realm = fuchsia_component::client::connect_to_protocol::<
                fidl_fuchsia_component::RealmMarker,
            >()?;

            realm.get_resolved_info().await.unwrap().unwrap()
        };

        let test_resolution_context = Arc::new(test_component.resolution_context);
        let test_pkg_dir = Arc::new(
            test_component.package.expect("a pkg").directory.expect("a directory").into_proxy(),
        );

        let boot_dir = Arc::new(match args.boot {
            Some(boot_dir) => {
                if !boot_dir.is_invalid_handle() {
                    Some(boot_dir.into_proxy())
                } else {
                    None
                }
            }
            None => None,
        });
        let boot_driver_components = Arc::new(args.boot_driver_components);

        // Route the resolvers from the test root.
        self.add_route(
            Route::new()
                .capability(
                    Capability::protocol_by_name("fuchsia.component.resolution.Resolver-hermetic")
                        .optional(),
                )
                .capability(
                    Capability::protocol_by_name("fuchsia.pkg.PackageResolver-hermetic").optional(),
                )
                .from(Ref::parent())
                .to(&realm),
        )
        .await?;

        // Setup the local Internal protocol server child.
        let driver_test_internal = realm
            .add_local_child(
                "driver_test_internal",
                move |handles| {
                    let test_pkg_dir = test_pkg_dir.clone();
                    let test_resolution_context = test_resolution_context.clone();
                    let boot_dir = boot_dir.clone();
                    let boot_driver_components = boot_driver_components.clone();
                    let devicetree = devicetree.clone();

                    Box::pin(run_internal_server(
                        handles,
                        test_pkg_dir,
                        test_resolution_context,
                        boot_dir,
                        boot_driver_components,
                        devicetree,
                    ))
                },
                ChildOptions::new(),
            )
            .await?;

        // Provide offers from the dtr_offers_provider, if the test provides one, to the driver
        // collections.
        match (args.dtr_offers, options.dtr_offers_provider) {
            (Some(offers), Some(provider)) => {
                for offer in offers {
                    self.add_route(
                        Route::new().capability(offer.clone()).from(provider.clone()).to(&realm),
                    )
                    .await?;
                    realm
                        .add_route(
                            Route::new()
                                .capability(offer)
                                .from(Ref::parent())
                                .to(&boot_drivers)
                                .to(&base_drivers)
                                .to(&full_drivers),
                        )
                        .await?;
                }
            }
            (None, None) => {}
            _ => {
                return Err(anyhow::anyhow!(
                    "Must provide |args.dtr_offers| and |dtr_offers_provider| together."
                ));
            }
        }

        // Provide exposes from the driver collections to the test.
        if let Some(exposes) = args.dtr_exposes {
            for expose in exposes {
                realm
                    .add_route(
                        Route::new()
                            .capability(expose.clone())
                            .from(&boot_drivers)
                            .to(Ref::parent()),
                    )
                    .await?;
                realm
                    .add_route(
                        Route::new()
                            .capability(expose.clone())
                            .from(&base_drivers)
                            .to(Ref::parent()),
                    )
                    .await?;
                realm
                    .add_route(
                        Route::new()
                            .capability(expose.clone())
                            .from(&full_drivers)
                            .to(Ref::parent()),
                    )
                    .await?;

                self.add_route(Route::new().capability(expose).from(&realm).to(Ref::parent()))
                    .await?;
            }
        }

        // Setup boot items, either tunneled from boot_items_to_tunnel, if the test provides one, or
        // tunneling is disabled, in which case the dtr_support provides a stand-in implementation.
        let do_tunneling = match options.boot_items_to_tunnel {
            Some(provider) => {
                self.add_route(
                    Route::new()
                        .capability(Capability::protocol_by_name("fuchsia.boot.Items").optional())
                        .from(provider)
                        .to(&realm),
                )
                .await?;

                realm
                    .add_route(
                        Route::new()
                            .capability(
                                Capability::protocol_by_name("fuchsia.boot.Items").optional(),
                            )
                            .from(Ref::parent())
                            .to(&dtr_support),
                    )
                    .await?;

                true
            }
            None => {
                realm
                    .add_route(
                        Route::new()
                            .capability(
                                Capability::protocol_by_name("fuchsia.boot.Items").optional(),
                            )
                            .from(Ref::void())
                            .to(&dtr_support),
                    )
                    .await?;

                false
            }
        };

        // Setup the driver test resource provider.
        realm
            .add_route(
                Route::new()
                    .capability(Capability::protocol_by_name(
                        "fuchsia.driver.test.ResourceProvider",
                    ))
                    .from(&driver_test_internal)
                    .to(&dtr_support),
            )
            .await?;

        // Setup various basic config capabilities.
        realm
            .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                name: "fuchsia.driver.testrealm.TunnelBootItems".parse()?,
                value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Bool(do_tunneling)),
            }))
            .await?;

        realm
            .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                name: "fuchsia.driver.testrealm.BoardName".parse()?,
                value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::String(
                    args.board_name.unwrap_or("".to_string()),
                )),
            }))
            .await?;

        realm
            .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                name: "fuchsia.driver.testrealm.PlatformVid".parse()?,
                value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::String(
                    args.platform_vid.map(|v| v.to_string()).unwrap_or("".to_string()),
                )),
            }))
            .await?;

        realm
            .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                name: "fuchsia.driver.testrealm.PlatformPid".parse()?,
                value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::String(
                    args.platform_pid.map(|v| v.to_string()).unwrap_or("".to_string()),
                )),
            }))
            .await?;

        let bind_eager = args.driver_bind_eager.unwrap_or_default();
        realm
            .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                name: "fuchsia.driver.BindEager".parse()?,
                value: cm_rust::ConfigValue::Vector(cm_rust::ConfigVectorValue::StringVector(
                    bind_eager.into(),
                )),
            }))
            .await?;

        let driver_disable = args.driver_disable.unwrap_or_default();
        realm
            .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                name: "fuchsia.driver.DisabledDrivers".parse()?,
                value: cm_rust::ConfigValue::Vector(cm_rust::ConfigVectorValue::StringVector(
                    driver_disable.into(),
                )),
            }))
            .await?;

        let driver_index_stop_timeout_millis = args.driver_index_stop_timeout_millis.unwrap_or(-1);
        realm
            .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                name: "fuchsia.driver.index.StopOnIdleTimeoutMillis".parse()?,
                value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Int64(
                    driver_index_stop_timeout_millis,
                )),
            }))
            .await?;

        let root_driver = match args.root_driver {
            Some(val) => val,
            None => "fuchsia-boot:///dtr#meta/test-parent-sys.cm".to_string(),
        };
        realm
            .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                name: "fuchsia.driver.manager.RootDriver".parse()?,
                value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::String(
                    root_driver,
                )),
            }))
            .await?;

        // Setup software device config capabilities.
        let software_devs_src = match args.software_devices {
            Some(devs) => {
                let names = devs.iter().map(|dev| dev.device_name.clone()).collect::<Vec<_>>();
                let ids = devs.iter().map(|dev| dev.device_id).collect::<Vec<_>>();
                realm
                    .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                        name: "fuchsia.platform.bus.SoftwareDeviceNames".parse()?,
                        value: cm_rust::ConfigValue::Vector(
                            cm_rust::ConfigVectorValue::StringVector(names.into()),
                        ),
                    }))
                    .await?;
                realm
                    .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                        name: "fuchsia.platform.bus.SoftwareDeviceIds".parse()?,
                        value: cm_rust::ConfigValue::Vector(
                            cm_rust::ConfigVectorValue::Uint32Vector(ids.into()),
                        ),
                    }))
                    .await?;
                Ref::self_()
            }
            None => Ref::void(),
        };

        // Config routes.
        realm
            .add_route(
                Route::new()
                    .capability(Capability::configuration("fuchsia.driver.BindEager"))
                    .capability(Capability::configuration("fuchsia.driver.DisabledDrivers"))
                    .capability(Capability::configuration(
                        "fuchsia.driver.index.StopOnIdleTimeoutMillis",
                    ))
                    .from(Ref::self_())
                    .to(&driver_index),
            )
            .await?;

        realm
            .add_route(
                Route::new()
                    .capability(Capability::configuration("fuchsia.driver.manager.RootDriver"))
                    .from(Ref::self_())
                    .to(&driver_manager),
            )
            .await?;

        realm
            .add_route(
                Route::new()
                    .capability(Capability::configuration(
                        "fuchsia.driver.testrealm.TunnelBootItems",
                    ))
                    .capability(Capability::configuration("fuchsia.driver.testrealm.BoardName"))
                    .capability(Capability::configuration("fuchsia.driver.testrealm.PlatformVid"))
                    .capability(Capability::configuration("fuchsia.driver.testrealm.PlatformPid"))
                    .from(Ref::self_())
                    .to(&dtr_support),
            )
            .await?;

        realm
            .add_route(
                Route::new()
                    .capability(
                        Capability::configuration("fuchsia.platform.bus.SoftwareDeviceNames")
                            .optional(),
                    )
                    .capability(
                        Capability::configuration("fuchsia.platform.bus.SoftwareDeviceIds")
                            .optional(),
                    )
                    .from(software_devs_src)
                    .to(&boot_drivers),
            )
            .await?;

        // Dynamic routes to the driver framework children.
        realm
            .add_route(
                Route::new()
                    .capability(Capability::protocol_by_name("fuchsia.driver.test.Internal"))
                    .from(&driver_test_internal)
                    .to(&fake_resolver),
            )
            .await?;

        // Routes from the driver framework children out to the test.
        self.add_route(
            Route::new()
                .capability(Capability::directory("dev-class"))
                .capability(Capability::directory("dev-topological"))
                .capability(Capability::protocol_by_name(
                    "fuchsia.driver.registrar.DriverRegistrar",
                ))
                .capability(Capability::protocol_by_name("fuchsia.driver.development.Manager"))
                .capability(Capability::protocol_by_name(
                    "fuchsia.driver.framework.CompositeNodeManager",
                ))
                .capability(Capability::protocol_by_name("fuchsia.system.state.Administrator"))
                .from(&realm)
                .to(Ref::parent()),
        )
        .await?;
        // LINT.ThenChange(/sdk/lib/driver_test_realm/realm_builder/cpp/builder.cc)
        Ok(&self)
    }
}

#[async_trait::async_trait]
pub trait DriverTestRealmInstance {
    /// Connect to the /dev/ directory hosted by  DriverTestRealm in this Instance.
    fn driver_test_realm_connect_to_dev(&self) -> Result<fio::DirectoryProxy>;

    /// Waits for the driver manager boot up logic to complete. This will ensure all
    /// in-progress binds complete and indicates its safe to proceed with the test
    /// or tear down the test realm with no errors.
    async fn wait_for_bootup(&self) -> Result<()>;
}

#[async_trait::async_trait]
impl DriverTestRealmInstance for RealmInstance {
    fn driver_test_realm_connect_to_dev(&self) -> Result<fio::DirectoryProxy> {
        fuchsia_fs::directory::open_directory_async(
            self.root.get_exposed_dir(),
            "dev-topological",
            fio::Flags::empty(),
        )
        .map_err(Into::into)
    }

    async fn wait_for_bootup(&self) -> Result<()> {
        let manager: fdd::ManagerProxy = self.root.connect_to_protocol_at_exposed_dir()?;
        manager.wait_for_bootup().await?;
        Ok(())
    }
}
