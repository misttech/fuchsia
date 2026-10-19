// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(clippy::let_unit_value)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::from_over_into)]
#![allow(clippy::too_many_arguments)]

use crate::frozen_index::{BaseIndex, CacheIndex};
use crate::index::PackageIndex;
use anyhow::{Context as _, Error, anyhow, format_err};
use cobalt_sw_delivery_registry as metrics;
use fidl::endpoints::{DiscoverableProtocolMarker as _, ServerEnd};
use fidl_contrib::ProtocolConnector;
use fidl_contrib::protocol_connector::ConnectedProtocol;
use fidl_fuchsia_component_resolution as fcomponent_resolution;
use fidl_fuchsia_fxfs as ffxfs;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_metrics::{
    MetricEvent, MetricEventLoggerFactoryMarker, MetricEventLoggerProxy, ProjectSpec,
};
use fidl_fuchsia_pkg as fpkg;
use fidl_fuchsia_pkg_http as fpkg_http;
use fidl_fuchsia_pkg_internal as fpkg_internal;
use fidl_fuchsia_pkg_resolution as fpkg_resolution;
use fidl_fuchsia_storage_block as fstorage_block;
use fidl_fuchsia_storage_mapping as fstorage_mapping;
use fidl_fuchsia_update::CommitStatusProviderMarker;
use fuchsia_async as fasync;
use fuchsia_async::Task;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_inspect as finspect;
use fuchsia_url::fuchsia_pkg::AbsolutePackageUrl;
use futures::join;
use futures::prelude::*;
use log::{error, info};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use vfs::directory::helper::DirectlyMutable as _;
use vfs::remote::remote_dir;

mod base_package_resolver;
mod blob_fetcher;
mod cache_service;
mod compat;
mod component_resolver;
mod executability;
mod frozen_index;
mod full_package_resolver;
mod gc_service;
mod index;
mod ota_downloader;
mod ota_resolver;
mod package_fetcher;
mod required_blobs;
mod retained_packages_service;
mod root_dir;
mod upgradable_packages;

use root_dir::{CachedRootDir, RootDir, RootDirCache, RootDirFactory};

#[cfg(test)]
mod test_utils;

const COBALT_CONNECTOR_BUFFER_SIZE: usize = 1000;
const MAX_CONCURRENT_PACKAGE_FETCHES: usize = 5;

struct CobaltConnectedService;
impl ConnectedProtocol for CobaltConnectedService {
    type Protocol = MetricEventLoggerProxy;
    type ConnectError = Error;
    type Message = MetricEvent;
    type SendError = Error;

    fn get_protocol(&mut self) -> future::BoxFuture<'_, Result<MetricEventLoggerProxy, Error>> {
        async {
            let (logger_proxy, server_end) = fidl::endpoints::create_proxy();
            let metric_event_logger_factory =
                connect_to_protocol::<MetricEventLoggerFactoryMarker>()
                    .context("Failed to connect to fuchsia::metrics::MetricEventLoggerFactory")?;

            metric_event_logger_factory
                .create_metric_event_logger(
                    &ProjectSpec { project_id: Some(metrics::PROJECT_ID), ..Default::default() },
                    server_end,
                )
                .await?
                .map_err(|e| format_err!("Connection to MetricEventLogger refused {e:?}"))?;
            Ok(logger_proxy)
        }
        .boxed()
    }

    fn send_message<'a>(
        &'a mut self,
        protocol: &'a MetricEventLoggerProxy,
        msg: MetricEvent,
    ) -> future::BoxFuture<'a, Result<(), Error>> {
        async move {
            let fut = protocol.log_metric_events(&[msg]);
            fut.await?.map_err(|e| format_err!("Failed to log metric {e:?}"))?;
            Ok(())
        }
        .boxed()
    }
}

#[fuchsia::main(logging_tags = ["pkg-cache"])]
pub fn main() -> Result<(), Error> {
    fuchsia_trace_provider::trace_provider_create_with_fdio();

    let mut executor = fasync::LocalExecutorBuilder::new().build();
    executor.run_singlethreaded(async move {
        match main_inner().await {
            Err(err) => {
                let err = anyhow!(err);
                error!("error running pkg-cache: {err:#}");
                Err(err)
            }
            ok => ok,
        }
    })
}

struct DriverBlobReader(Arc<blob_pager_and_verifier::BlobPagerAndVerifier>);

impl std::fmt::Debug for DriverBlobReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverBlobReader").finish_non_exhaustive()
    }
}

impl blobfs::BlobReader for DriverBlobReader {
    fn get_vmo<'a>(
        &'a self,
        blob: &'a fuchsia_hash::Hash,
    ) -> futures::future::BoxFuture<'a, Result<zx::Vmo, blobfs::GetBlobVmoError>> {
        async move {
            self.0.create_vmo(blob).await.map_err(|e| {
                error!(blob:%, error:? = e; "DriverBlobReader failed to get VMO");
                if let Some(&status) = e.root_cause().downcast_ref::<zx::Status>() {
                    blobfs::GetBlobVmoError::GetVmo(status)
                } else {
                    blobfs::GetBlobVmoError::GetVmo(zx::Status::INTERNAL)
                }
            })
        }
        .boxed()
    }
}

impl blobfs::IntoBlobReader for DriverBlobReader {
    fn into_blob_reader(self) -> Arc<dyn blobfs::BlobReader> {
        Arc::new(self)
    }
}

async fn main_inner() -> Result<(), Error> {
    info!("starting package cache service");
    let inspector = finspect::Inspector::default();
    let config = pkg_cache_config::Config::take_from_startup_handle();
    inspector
        .root()
        .record_child("structured_config", |config_node| config.record_inspect(config_node));
    // TODO(https://fxbug.dev/331302451) Use the all_packages_executable config value instead of the
    // presence of file data/pkgfs_disable_executability_restrictions in the system_image package to
    // determine whether executability should be enforced.
    let pkg_cache_config::Config {
        all_packages_executable: _,
        require_system_image,
        enable_upgradable_packages,
        system_image_hash,
        blob_fetch_concurrency_limit,
        blob_network_header_timeout_seconds,
        blob_network_body_timeout_seconds,
        blob_download_resumption_attempts_limit,
        use_driver_blob_paging,
    } = config;
    let mut blobfs_builder = blobfs::Client::builder().readable().writable().executable();
    let mut blob_verifier = None;
    if use_driver_blob_paging {
        let verifier_result: Result<blob_pager_and_verifier::BlobPagerAndVerifier, Error> = async {
            let mapping_provider = connect_to_protocol::<fstorage_mapping::MappingProviderMarker>()
                .context("error connecting to fuchsia.storage.mapping.MappingProvider")?;
            let mapper = connect_to_protocol::<fstorage_block::MapperMarker>()
                .context("error connecting to fuchsia.storage.block.Mapper")?;
            blob_pager_and_verifier::BlobPagerAndVerifier::new(&mapping_provider, &mapper)
                .await
                .context("error initializing BlobPagerAndVerifier")
        }
        .await;

        match verifier_result {
            Ok(verifier) => {
                info!("Using driver blob paging");
                inspector.root().record_string("driver_blob_paging_status", "active");
                // Matches the `".boot"` profile name in Fxfs and `startup_profiling_seconds` in
                // fshost.
                // LINT.IfChange(startup_profiling)
                verifier
                    .record_and_replay_profile(".boot", zx::MonotonicDuration::from_seconds(60))
                    .await;
                // LINT.ThenChange(/src/storage/fshost/src/environment.rs:startup_profiling)
                let verifier = Arc::new(verifier);
                blobfs_builder = blobfs_builder.reader(DriverBlobReader(verifier.clone()));
                blob_verifier = Some(verifier);
            }
            Err(error) => {
                error!(
                    error:?;
                    "Failed to initialize driver blob paging, falling back to Fxfs BlobReader"
                );
                inspector.root().record_string("driver_blob_paging_status", "fallback");
            }
        }
    } else {
        inspector.root().record_string("driver_blob_paging_status", "disabled");
    }
    let driver_blob_paging_active = blob_verifier.is_some();
    let blobfs = blobfs_builder.build().await.context("error opening blobfs")?;

    let authenticator = context_authenticator::ContextAuthenticator::new();

    let system_image_result =
        system_image::SystemImage::new(blobfs.clone(), &system_image_hash).await;
    let (executability_restrictions, base_index, cache_index) = match system_image_result {
        Ok(system_image) => {
            info!("system_image package: {}", system_image.hash());
            inspector.root().record_string("system_image", system_image.hash().to_string());

            let (base_index_res, cache_index_res) =
                join!(BaseIndex::new(&blobfs, &system_image), async {
                    let cache_index =
                        system_image.cache_packages().await.context("reading cache_packages")?;
                    CacheIndex::new(&blobfs, &cache_index).await.context("creating CacheIndex")
                });
            let base_index = match base_index_res {
                Ok(base_index) => base_index,
                Err(e) if require_system_image => {
                    return Err(e).context("loading base packages");
                }
                Err(e) => {
                    error!("Failed to load base packages, using empty: {e:#}");
                    BaseIndex::empty()
                }
            };
            let cache_index = cache_index_res.unwrap_or_else(|e: anyhow::Error| {
                error!("Failed to load cache packages, using empty: {e:#}");
                CacheIndex::empty()
            });
            let executability_restrictions = system_image.load_executability_restrictions();

            (executability_restrictions, base_index, cache_index)
        }
        Err(e) if require_system_image => {
            return Err(e).context("Accessing contents of system_image package");
        }
        Err(e) => {
            error!("Failed to load system_image, using empty: {e:#}");
            inspector.root().record_string("system_image", "failed_to_load");
            (
                system_image::ExecutabilityRestrictions::Enforce,
                BaseIndex::empty(),
                CacheIndex::empty(),
            )
        }
    };

    inspector
        .root()
        .record_string("executability-restrictions", format!("{executability_restrictions:?}"));
    let base_index = Arc::new(base_index);
    let cache_index = Arc::new(cache_index);
    inspector.root().record_lazy_child("base-packages", base_index.record_lazy_inspect());
    inspector.root().record_lazy_child("cache-packages", cache_index.record_lazy_inspect());
    let package_index = Arc::new(async_lock::RwLock::new(PackageIndex::new()));
    inspector.root().record_lazy_child("index", PackageIndex::record_lazy_inspect(&package_index));
    let scope = vfs::execution_scope::ExecutionScope::new();
    let (cobalt_sender, cobalt_fut) = ProtocolConnector::new_with_buffer_size(
        CobaltConnectedService,
        COBALT_CONNECTOR_BUFFER_SIZE,
    )
    .serve_and_log_errors();
    let cobalt_fut = Task::spawn(cobalt_fut);

    let (root_dir_factory, open_packages) = root_dir::new(
        fuchsia_fs::directory::open_in_namespace(
            "/bootfs-blobs",
            fio::PERM_READABLE | fio::PERM_EXECUTABLE,
        )
        .context("open bootfs blobs dir")?,
        blobfs.clone(),
    )
    .await
    .context("creating root dir helpers")?;
    inspector.root().record_lazy_child("open-packages", open_packages.record_lazy_inspect());

    let upgradable_packages = enable_upgradable_packages
        .then(|| Arc::new(upgradable_packages::UpgradablePackages::new(Arc::clone(&cache_index))));

    // Use VFS to serve the out dir because ServiceFs does not support PERM_EXECUTABLE and
    // pkgfs/{packages|system} require it.
    let svc_dir = vfs::pseudo_directory! {};
    let cache_inspect_node = inspector.root().create_child("fuchsia.pkg.PackageCache");
    {
        let package_index = Arc::clone(&package_index);
        let blobfs = blobfs.clone();
        let root_dir_factory = root_dir_factory.clone();
        let open_packages = open_packages.clone();
        let base_index = Arc::clone(&base_index);
        let cache_index = Arc::clone(&cache_index);
        let upgradable_packages = upgradable_packages.clone();
        let scope = scope.clone();
        let cobalt_sender = cobalt_sender.clone();
        let cache_inspect_id = Arc::new(AtomicU32::new(0));
        let cache_get_node = Arc::new(cache_inspect_node.create_child("get"));

        let () = svc_dir
            .add_entry(
                fpkg::PackageCacheMarker::PROTOCOL_NAME,
                vfs::service::host(move |stream: fpkg::PackageCacheRequestStream| {
                    cache_service::serve(
                        Arc::clone(&package_index),
                        blobfs.clone(),
                        root_dir_factory.clone(),
                        Arc::clone(&base_index),
                        Arc::clone(&cache_index),
                        upgradable_packages.clone(),
                        executability_restrictions,
                        scope.clone(),
                        open_packages.clone(),
                        stream,
                        cobalt_sender.clone(),
                        Arc::clone(&cache_inspect_id),
                        Arc::clone(&cache_get_node),
                    )
                    .unwrap_or_else(|e: anyhow::Error| {
                        error!("serving fuchsia.pkg/PackageCache: {e:#}")
                    })
                }),
            )
            .context("adding fuchsia.pkg/PackageCache to /svc")?;
    }
    let () = svc_dir
        .add_entry(
            ffxfs::BlobReaderMarker::PROTOCOL_NAME,
            get_blob_reader_handler(driver_blob_paging_active, &blobfs),
        )
        .context("adding fuchsia.fxfs/BlobReader to /svc")?;
    if let Some(blob_verifier) = blob_verifier {
        let () = svc_dir
            .add_entry(
                fstorage_mapping::FileMapperMarker::PROTOCOL_NAME,
                blob_verifier.get_file_mapper_handler(scope.clone()),
            )
            .context("adding fuchsia.storage.mapping/FileMapper to /svc")?;
    }
    {
        let package_index = Arc::clone(&package_index);
        let blobfs = blobfs.clone();

        let () = svc_dir
            .add_entry(
                fpkg::RetainedPackagesMarker::PROTOCOL_NAME,
                vfs::service::host(move |stream: fpkg::RetainedPackagesRequestStream| {
                    retained_packages_service::serve(
                        Arc::clone(&package_index),
                        blobfs.clone(),
                        stream,
                    )
                    .unwrap_or_else(|e: anyhow::Error| {
                        error!("serving fuchsia.pkg/RetainedPackages: {e:#}")
                    })
                }),
            )
            .context("adding fuchsia.pkg/RetainedPackages to /svc")?;
    }
    {
        let package_index = Arc::clone(&package_index);

        let () = svc_dir
            .add_entry(
                fpkg::RetainedBlobsMarker::PROTOCOL_NAME,
                vfs::service::host(move |stream| {
                    retained_packages_service::serve_retained_blobs(
                        Arc::clone(&package_index),
                        stream,
                    )
                    .unwrap_or_else(|e: anyhow::Error| {
                        error!("serving fuchsia.pkg/RetainedBlobs: {e:#}")
                    })
                }),
            )
            .context("adding fuchsia.pkg/RetainedBlobs to /svc")?;
    }
    {
        let blobfs = blobfs.clone();
        let base_index = Arc::clone(&base_index);
        let cache_index = Arc::clone(&cache_index);
        let upgradable_packages = upgradable_packages.clone();
        let package_index = Arc::clone(&package_index);
        let open_packages = open_packages.clone();
        let commit_status_provider =
            fuchsia_component::client::connect_to_protocol::<CommitStatusProviderMarker>()
                .context("while connecting to commit status provider")?;

        let () = svc_dir
            .add_entry(
                fidl_fuchsia_pkg_garbagecollector::ManagerMarker::PROTOCOL_NAME,
                vfs::service::host(
                    move |stream: fidl_fuchsia_pkg_garbagecollector::ManagerRequestStream| {
                        gc_service::serve(
                            blobfs.clone(),
                            Arc::clone(&base_index),
                            Arc::clone(&cache_index),
                            upgradable_packages.clone(),
                            Arc::clone(&package_index),
                            open_packages.clone(),
                            commit_status_provider.clone(),
                            stream,
                        )
                        .unwrap_or_else(|e: anyhow::Error| {
                            error!("serving fuchsia.pkg.garbagecollector/Manager: {e:#}")
                        })
                    },
                ),
            )
            .context("adding fuchsia.pkg.garbagecollector/Manager to /svc")?;
    }
    let base_package_resolver = base_package_resolver::Resolver::new(
        Arc::clone(&base_index),
        authenticator.clone(),
        open_packages.clone(),
        scope.clone(),
    );
    {
        let base_package_resolver = Arc::clone(&base_package_resolver);
        let () = svc_dir
            .add_entry(
                format!("{}-base", fpkg::PackageResolverMarker::PROTOCOL_NAME),
                vfs::service::host(move |stream: fpkg::PackageResolverRequestStream| {
                    Arc::clone(&base_package_resolver).serve_request_stream(stream).unwrap_or_else(
                        |e: anyhow::Error| {
                            error!("serving fuchsia.pkg/PackageResolver-base: {e:#}")
                        },
                    )
                }),
            )
            .context("adding fuchsia.pkg/PackageResolver-base to /svc")?;
    }
    {
        let base_package_resolver = Arc::clone(&base_package_resolver);
        let () = svc_dir
            .add_entry(
                format!("{}-base", fcomponent_resolution::ResolverMarker::PROTOCOL_NAME),
                vfs::service::host(move |stream: fcomponent_resolution::ResolverRequestStream| {
                    component_resolver::serve_request_stream(
                        stream,
                        Arc::clone(&base_package_resolver),
                        "base component resolver",
                    )
                    .unwrap_or_else(|e: anyhow::Error| {
                        error!("serving fuchsia.component.resolution/Resolver-base: {e:#}")
                    })
                }),
            )
            .context("adding fuchsia.component.resolution/Resolver-base to /svc")?;
    }
    let (blob_fetcher_fut, blob_fetcher) = blob_fetcher::BlobFetcher::new(
        blob_fetch_concurrency_limit.into(),
        blob_fetcher::Params::builder()
            .header_network_timeout(zx::BootDuration::from_seconds(
                blob_network_header_timeout_seconds.into(),
            ))
            .body_network_timeout(zx::BootDuration::from_seconds(
                blob_network_body_timeout_seconds.into(),
            ))
            .download_resumption_attempts_limit(blob_download_resumption_attempts_limit)
            .build(),
        blobfs.clone(),
        fidl_connector::ServiceReconnector::<fpkg_http::ClientMarker>::new(),
        inspector.root().create_child("blob_fetcher"),
    );
    let blob_fetcher_fut = Task::spawn(blob_fetcher_fut);
    {
        let blob_fetcher = blob_fetcher.clone();
        let () = svc_dir
            .add_entry(
                fpkg_internal::OtaDownloaderMarker::PROTOCOL_NAME,
                vfs::service::host(move |stream: fpkg_internal::OtaDownloaderRequestStream| {
                    ota_downloader::serve_request_stream(stream, blob_fetcher.clone())
                        .unwrap_or_else(|e: anyhow::Error| {
                            error!("serving fuchsia.pkg.internal/OtaDownloader: {e:#}")
                        })
                }),
            )
            .context("adding fuchsia.pkg.internal/OtaDownloader to /svc")?;
    }
    let (package_fetcher_fut, package_fetcher) = package_fetcher::PackageFetcher::new(
        MAX_CONCURRENT_PACKAGE_FETCHES,
        package_index.clone(),
        blobfs.clone(),
        blob_fetcher,
        root_dir_factory.clone(),
        open_packages.clone(),
        inspector.root().create_child("package_fetcher"),
    );
    let package_fetcher_fut = Task::spawn(package_fetcher_fut);
    let tuf_authority = fidl_connector::ServiceReconnector::<fpkg::AuthorityMarker>::new();
    {
        let resolver = ota_resolver::Resolver::new(
            tuf_authority.clone(),
            package_fetcher.clone(),
            authenticator.clone(),
            root_dir_factory.clone(),
            scope.clone(),
            inspector.root().create_child("fuchsia.pkg.PackageResolver-ota"),
        );
        let () = svc_dir
            .add_entry(
                format!("{}-ota", fpkg::PackageResolverMarker::PROTOCOL_NAME),
                vfs::service::host(move |stream: fpkg::PackageResolverRequestStream| {
                    Arc::clone(&resolver).serve_request_stream(stream).unwrap_or_else(
                        |e: anyhow::Error| error!("serving fuchsia.pkg/PackageResolver-ota: {e:#}"),
                    )
                }),
            )
            .context("adding fuchsia.pkg/PackageResolver-ota to /svc")?;
    }
    let full_package_resolver = full_package_resolver::Resolver::new(
        Arc::clone(&base_package_resolver),
        upgradable_packages,
        tuf_authority,
        cache_index,
        package_fetcher,
        authenticator,
        open_packages,
        executability::Decider::new(executability_restrictions, Arc::clone(&base_index)),
        scope.clone(),
        inspector.root().create_child("fuchsia.pkg.PackageResolver-full"),
    );
    {
        let full_package_resolver = Arc::clone(&full_package_resolver);
        let () = svc_dir
            .add_entry(
                format!("{}-full", fpkg::PackageResolverMarker::PROTOCOL_NAME),
                vfs::service::host(move |stream: fpkg::PackageResolverRequestStream| {
                    Arc::clone(&full_package_resolver).serve_request_stream(stream).unwrap_or_else(
                        |e: anyhow::Error| {
                            error!("serving fuchsia.pkg/PackageResolver-full: {e:#}")
                        },
                    )
                }),
            )
            .context("adding fuchsia.pkg/PackageResolver-full to /svc")?;
    }
    {
        let full_package_resolver = Arc::clone(&full_package_resolver);
        let () = svc_dir
            .add_entry(
                format!("{}-full", fcomponent_resolution::ResolverMarker::PROTOCOL_NAME),
                vfs::service::host(move |stream: fcomponent_resolution::ResolverRequestStream| {
                    component_resolver::serve_request_stream(
                        stream,
                        Arc::clone(&full_package_resolver),
                        "full component resolver",
                    )
                    .unwrap_or_else(|e: anyhow::Error| {
                        error!("serving fuchsia.component.resolution/Resolver-full: {e:#}")
                    })
                }),
            )
            .context("adding fuchsia.component.resolution/Resolver-full to /svc")?;
    }
    {
        let () = svc_dir
            .add_entry(
                fpkg_resolution::PackageResolverMarker::PROTOCOL_NAME,
                vfs::service::host(move |stream: fpkg_resolution::PackageResolverRequestStream| {
                    Arc::clone(&full_package_resolver)
                        .serve_toolbox_resolver_request_stream(stream)
                        .unwrap_or_else(|e: anyhow::Error| {
                            error!("serving fuchsia.pkg.resolution/PackageResolver: {e:#}")
                        })
                }),
            )
            .context("adding fuchsia.pkg.resolution/PackageResolver to /svc")?;
    }

    let base_package_entry = |name: &'static str| {
        serve_base_package_if_present(
            AbsolutePackageUrl::new(
                "fuchsia-pkg://fuchsia.com".parse().expect("valid repo url"),
                name.parse().expect("valid package name"),
                None,
                None,
            ),
            base_package_resolver.as_ref(),
        )
        .map(move |result| result.map(remote_dir).with_context(|| format!("getting {name} dir")))
    };

    let out_dir = vfs::pseudo_directory! {
        "svc" => svc_dir,
        "pkgfs" =>
            crate::compat::pkgfs::make_dir(
                Arc::clone(&base_index),
                blobfs.clone(),
            ),
        "specific-base-packages" => vfs::pseudo_directory! {
            "build-info" => base_package_entry("build-info").await?,
            "config-data" => base_package_entry("config-data").await?,
            "google_root_ssl_certificates" => base_package_entry("google_root_ssl_certificates").await?,
            "root_ssl_certificates" => base_package_entry("root_ssl_certificates").await?,
            "system_image" => base_package_entry("system_image").await?,
        }
    };

    let _inspect_server_task =
        inspect_runtime::publish(&inspector, inspect_runtime::PublishOptions::default());
    let handle =
        fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::DirectoryRequest.into())
            .context("taking startup handle")?;
    vfs::directory::serve_on(
        out_dir,
        fio::PERM_READABLE | fio::PERM_WRITABLE | fio::PERM_EXECUTABLE,
        scope.clone(),
        ServerEnd::new(handle.into()),
    );
    let () = scope.wait().await;
    let () = blob_fetcher_fut.await;
    let () = package_fetcher_fut.await;
    let () = cobalt_fut.await;

    Ok(())
}

async fn serve_base_package_if_present(
    url: AbsolutePackageUrl,
    resolver: &base_package_resolver::Resolver,
) -> anyhow::Result<fio::DirectoryProxy> {
    let (proxy, server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    match resolver
        .resolve_and_serve(&url, server)
        .await
        .map(|(_, _): (fpkg::ResolutionContext, Arc<CachedRootDir>)| ())
    {
        Ok(()) => (),
        Err(base_package_resolver::Error::PackageNotInIndex) => {
            log::warn!(url:%; "package not in base, so exposed directory will close connections")
        }
        Err(e) => Err(e).context("resolving specific base package")?,
    }
    Ok(proxy)
}

fn get_blob_reader_handler(
    driver_blob_paging_active: bool,
    blobfs: &blobfs::Client,
) -> Arc<vfs::service::Service> {
    if driver_blob_paging_active {
        let blobfs = blobfs.clone();
        vfs::service::host(move |stream: ffxfs::BlobReaderRequestStream| {
            let blobfs = blobfs.clone();
            stream
                .try_for_each_concurrent(None, move |request| {
                    let blobfs = blobfs.clone();
                    async move {
                        match request {
                            ffxfs::BlobReaderRequest::GetVmo { blob_hash, responder } => {
                                let res = blobfs.get_blob_vmo(&blob_hash.into()).await.map_err(
                                    |e| match e {
                                        blobfs::GetBlobVmoError::GetVmo(s) => s.into_raw(),
                                        blobfs::GetBlobVmoError::OpenBlob(
                                            fuchsia_fs::node::OpenError::OpenError(s),
                                        ) => s.into_raw(),
                                        blobfs::GetBlobVmoError::OpenBlob(_)
                                        | blobfs::GetBlobVmoError::Fidl(_) => {
                                            zx::Status::INTERNAL.into_raw()
                                        }
                                    },
                                );
                                let _ = responder.send(res);
                            }
                        }
                        Ok(())
                    }
                })
                .unwrap_or_else(|error| error!(error:?; "serving fuchsia.fxfs/BlobReader"))
        })
    } else {
        vfs::service::endpoint(|_scope, channel| {
            if let Err(error) = fuchsia_component::client::connect_channel_to_protocol::<
                ffxfs::BlobReaderMarker,
            >(channel.into_zx_channel())
            {
                error!(error:?; "Failed to forward fuchsia.fxfs/BlobReader");
            }
        })
    }
}

// Replicates `format!("{:#}", anyhow!(err))` but without consuming `err` or converting it into an
// `anyhow::Error`, so that the original, un-type-erased error can be propagated.
// Normally errors should either be propagated XOR logged, but there are some instances in which we
// are duplicating errors into an inspect history so that they will be available when debugging an
// OTA even after they have fallen out of the syslog history.
fn stringify_error(mut err: &dyn std::error::Error) -> String {
    let mut result = err.to_string();
    while let Some(source) = err.source() {
        use std::fmt::Write as _;
        let _ = write!(result, ": {source}");
        err = source;
    }
    result
}
