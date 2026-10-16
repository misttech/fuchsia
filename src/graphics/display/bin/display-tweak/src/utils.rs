// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, anyhow};
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_io as fio;
use fuchsia_component::client::Service;

/// Directories searched, in order, for the services used by this tool.
///
/// `/svc` is the component's incoming service directory. When the tool runs
/// inside `ffx component explore` with the default layout, `/svc` holds the
/// explore shell's own capabilities, and the explored component's namespace,
/// including its `/svc`, is mounted under `/ns`.
pub const SERVICE_DIRS: [&str; 2] = ["/svc", "/ns/svc"];

/// Returns the path of `service_name` in the first of `base_dirs` that
/// contains it.
///
/// Opening a service directory that does not exist only fails once the
/// service is used, so this checks for the directory up front. Only a missing
/// directory moves the search on to the next base directory; any other error
/// ends the search.
pub fn find_service_path(base_dirs: &[&str], service_name: &str) -> Result<String, Error> {
    find_service_path_with(base_dirs, service_name, |path| std::fs::metadata(path).map(|_| ()))
}

/// Implements [`find_service_path`], using `probe` to check whether a path
/// exists.
fn find_service_path_with(
    base_dirs: &[&str],
    service_name: &str,
    probe: impl Fn(&str) -> std::io::Result<()>,
) -> Result<String, Error> {
    let mut failures = Vec::new();
    for base_dir in base_dirs {
        let path = format!("{}/{}", base_dir, service_name);
        match probe(&path) {
            Ok(()) => {
                log::debug!("Using {}", path);
                return Ok(path);
            }
            Err(error) => {
                log::debug!("{} is not available: {}", path, error);
                let is_not_found = error.kind() == std::io::ErrorKind::NotFound;
                failures.push(format!("{}: {}", path, error));
                if !is_not_found {
                    break;
                }
            }
        }
    }
    Err(anyhow!("{} not available ({})", service_name, failures.join("; ")))
}

/// Opens the service described by `marker` from the first of `base_dirs` that
/// contains it.
pub fn open_service_from_dirs<S: ServiceMarker>(
    base_dirs: &[&str],
    marker: S,
) -> Result<Service<S>, Error> {
    let path = find_service_path(base_dirs, S::SERVICE_NAME)?;
    let dir = fuchsia_fs::directory::open_in_namespace(&path, fio::PERM_READABLE)
        .with_context(|| format!("Failed to open {}", path))?;
    Ok(Service::from_service_dir_proxy(dir, marker))
}

/// Opens the service described by `marker` from the first of [`SERVICE_DIRS`]
/// that contains it.
pub fn open_service<S: ServiceMarker>(marker: S) -> Result<Service<S>, Error> {
    open_service_from_dirs(&SERVICE_DIRS, marker)
}

/// Unwraps the result of a FIDL call that errors out with a zx::result into a
/// Result<T, E>.
pub fn flatten_zx_error<T>(
    fidl_result: Result<Result<T, zx::sys::zx_status_t>, fidl::Error>,
) -> Result<T, Error> {
    fidl_result?
        .map_err(|zx_status| anyhow!("Server response: {}", zx::Status::err_from_raw(zx_status)))
}

/// Helper for accepting boolean values as "off" / "on" strings.
pub fn on_off_to_bool(value: &str) -> Result<bool, String> {
    match value {
        "off" => Ok(false),
        "on" => Ok(true),
        _ => Err(String::from("Unrecognized value. Possible values are \"on\" and \"off\".")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_hardware_backlight as backlight;
    use fuchsia_component::server::{ServiceFs, ServiceObj};
    use futures::StreamExt;
    use googletest::{expect_that, gtest, matchers};

    /// Brightness reported by the fake backlight under `<root>/svc`.
    const SVC_BRIGHTNESS: f64 = 0.25;

    /// Brightness reported by the fake backlight under `<root>/ns/svc`.
    const NS_SVC_BRIGHTNESS: f64 = 0.75;

    /// A fake directory tree bound into this process's namespace at `root`.
    ///
    /// It serves a backlight service instance under `<root>/svc` and/or
    /// `<root>/ns/svc`, mirroring the `ffx component explore` layout. Each
    /// instance reports a different brightness, so tests can tell which one
    /// they reached. The tree is unbound on drop.
    struct FakeNamespace {
        root: &'static str,
        shutdown_tx: Option<futures::channel::oneshot::Sender<()>>,
        server_thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeNamespace {
        fn new(root: &'static str, in_svc: bool, in_ns_svc: bool) -> Self {
            let mut fs: ServiceFs<ServiceObj<'static, (f64, backlight::ServiceRequest)>> =
                ServiceFs::new();
            if in_svc {
                fs.dir("svc")
                    .add_fidl_service_instance("default", |request| (SVC_BRIGHTNESS, request));
            }
            if in_ns_svc {
                fs.dir("ns")
                    .dir("svc")
                    .add_fidl_service_instance("default", |request| (NS_SVC_BRIGHTNESS, request));
            }
            let (client, server) = fidl::endpoints::create_endpoints::<fio::DirectoryMarker>();
            fs.serve_connection(server).expect("failed to serve the fake namespace");
            fdio::Namespace::installed()
                .expect("failed to get the installed namespace")
                .bind(root, client)
                .expect("failed to bind the fake namespace");

            let (shutdown_tx, shutdown_rx) = futures::channel::oneshot::channel();
            let server_thread = std::thread::spawn(move || {
                let mut executor = fuchsia_async::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    let server_fut = std::pin::pin!(fs.for_each_concurrent(
                        None,
                        |(brightness, request)| async move {
                            let backlight::ServiceRequest::Backlight(mut device_requests) = request;
                            while let Some(Ok(request)) = device_requests.next().await {
                                match request {
                                    backlight::DeviceRequest::GetStateNormalized { responder } => {
                                        responder
                                            .send(Ok(&backlight::State {
                                                backlight_on: true,
                                                brightness,
                                            }))
                                            .unwrap();
                                    }
                                    request => panic!("Unexpected request: {:?}", request),
                                }
                            }
                        },
                    ));
                    futures::future::select(server_fut, shutdown_rx).await;
                });
            });
            FakeNamespace {
                root,
                shutdown_tx: Some(shutdown_tx),
                server_thread: Some(server_thread),
            }
        }

        fn path(&self, relative_path: &str) -> String {
            format!("{}/{}", self.root, relative_path)
        }
    }

    impl Drop for FakeNamespace {
        fn drop(&mut self) {
            // Don't panic here: a panic while unwinding from a failed test
            // would abort and hide the original failure.
            match fdio::Namespace::installed() {
                Ok(namespace) => {
                    if let Err(status) = namespace.unbind(self.root) {
                        log::warn!("Failed to unbind {}: {}", self.root, status);
                    }
                }
                Err(status) => log::warn!("Failed to get the installed namespace: {}", status),
            }
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(());
            }
            if let Some(thread) = self.server_thread.take() {
                thread.join().unwrap();
            }
        }
    }

    /// Opens the backlight service from the first of `base_dirs` that has it,
    /// and returns the brightness reported by its first instance.
    async fn read_brightness_from_dirs(base_dirs: &[&str]) -> Result<f64, Error> {
        let device = open_service_from_dirs(base_dirs, backlight::ServiceMarker)?
            .watch_for_any()
            .await?
            .connect_to_backlight()?;
        let state = flatten_zx_error(device.get_state_normalized().await)?;
        Ok(state.brightness)
    }

    #[gtest]
    #[fuchsia::test]
    fn find_service_path_uses_first_dir() {
        // Every component's namespace has /pkg/meta.
        expect_that!(
            find_service_path(&["/pkg", "/display-tweak-missing"], "meta"),
            matchers::ok(matchers::eq("/pkg/meta"))
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn find_service_path_falls_back_to_next_dir() {
        expect_that!(
            find_service_path(&["/display-tweak-missing", "/pkg"], "meta"),
            matchers::ok(matchers::eq("/pkg/meta"))
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn find_service_path_not_found() {
        let message = find_service_path(&["/missing-a", "/missing-b"], "fake.Service")
            .expect_err("fake.Service should not be found")
            .to_string();
        // Both directories are reported, along with the (platform-specific)
        // error for each.
        expect_that!(
            message,
            matchers::starts_with("fake.Service not available (/missing-a/fake.Service: ")
        );
        expect_that!(message, matchers::contains_substring("; /missing-b/fake.Service: "));
    }

    #[gtest]
    #[fuchsia::test]
    fn find_service_path_reports_every_missing_dir() {
        let result = find_service_path_with(&["/a", "/b"], "fake.Service", |_| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"))
        });
        expect_that!(
            result,
            matchers::err(matchers::displays_as(matchers::eq(
                "fake.Service not available (/a/fake.Service: missing; /b/fake.Service: missing)"
            )))
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn find_service_path_stops_on_other_errors() {
        let result = find_service_path_with(&["/a", "/b"], "fake.Service", |path| match path {
            "/a/fake.Service" => {
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"))
            }
            _ => Ok(()),
        });
        expect_that!(
            result,
            matchers::err(matchers::displays_as(matchers::eq(
                "fake.Service not available (/a/fake.Service: denied)"
            )))
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn find_service_path_skips_missing_dirs() {
        let result =
            find_service_path_with(&["/a", "/b", "/c"], "fake.Service", |path| match path {
                "/a/fake.Service" => {
                    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"))
                }
                _ => Ok(()),
            });
        expect_that!(result, matchers::ok(matchers::eq("/b/fake.Service")));
    }

    // The fake namespace is served on a second thread, because
    // `find_service_path` blocks on the filesystem.
    #[gtest]
    #[fuchsia::test]
    async fn open_service_from_dirs_prefers_first_dir() {
        let namespace = FakeNamespace::new("/display-tweak-test-both", true, true);
        let (svc, ns_svc) = (namespace.path("svc"), namespace.path("ns/svc"));
        expect_that!(
            read_brightness_from_dirs(&[&svc, &ns_svc]).await,
            matchers::ok(matchers::eq(&SVC_BRIGHTNESS))
        );
    }

    #[gtest]
    #[fuchsia::test]
    async fn open_service_from_dirs_falls_back_to_ns_svc() {
        let namespace = FakeNamespace::new("/display-tweak-test-ns-only", false, true);
        let (svc, ns_svc) = (namespace.path("svc"), namespace.path("ns/svc"));
        expect_that!(
            read_brightness_from_dirs(&[&svc, &ns_svc]).await,
            matchers::ok(matchers::eq(&NS_SVC_BRIGHTNESS))
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn open_service_from_dirs_not_found() {
        let namespace = FakeNamespace::new("/display-tweak-test-none", false, false);
        let (svc, ns_svc) = (namespace.path("svc"), namespace.path("ns/svc"));
        let result = open_service_from_dirs(&[&svc, &ns_svc], backlight::ServiceMarker);
        let message = result.map(|_| ()).expect_err("the service should not be found").to_string();
        let name = "fuchsia.hardware.backlight.Service";
        expect_that!(
            message,
            matchers::starts_with(format!("{name} not available ({svc}/{name}: "))
        );
        expect_that!(message, matchers::contains_substring(format!("; {ns_svc}/{name}: ")));
    }

    #[gtest]
    #[fuchsia::test]
    fn flatten_zx_error_with_success() {
        let fidl_result: std::result::Result<
            std::result::Result<i32, zx::sys::zx_status_t>,
            fidl::Error,
        > = Ok(Ok(42));
        let result = flatten_zx_error(fidl_result);
        expect_that!(result, matchers::ok(matchers::eq(&42)));
    }

    #[gtest]
    #[fuchsia::test]
    fn flatten_zx_error_with_zx_error() {
        let fidl_result: std::result::Result<
            std::result::Result<i32, zx::sys::zx_status_t>,
            fidl::Error,
        > = Ok(Err(zx::sys::ZX_ERR_NOT_SUPPORTED));
        let result: std::result::Result<i32, Error> = flatten_zx_error(fidl_result);

        expect_that!(
            result,
            matchers::err(matchers::displays_as(matchers::eq("Server response: NOT_SUPPORTED")))
        );
    }

    #[gtest]
    #[fuchsia::test]
    fn flatten_zx_error_with_fidl_error() {
        let fidl_error = fidl::Error::ClientChannelClosed {
            epitaph: fidl::Epitaph::PeerClosed,
            protocol_name: "TestService",
        };
        let fidl_result: std::result::Result<
            std::result::Result<i32, zx::sys::zx_status_t>,
            fidl::Error,
        > = Err(fidl_error.clone());
        let result: std::result::Result<i32, Error> = flatten_zx_error(fidl_result);

        expect_that!(
            result,
            matchers::err(matchers::displays_as(matchers::eq(format!("{}", fidl_error))))
        );
    }
}
