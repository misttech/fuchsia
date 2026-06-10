// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A tool to update the target device.

use async_trait::async_trait;
use fdomain_client::AsHandleRef;
use fdomain_client::fidl::{DiscoverableProtocolMarker, Proxy as _};
use fdomain_fuchsia_update::{
    CheckOptions, CommitStatusProviderProxy, Initiator, ManagerMarker, ManagerProxy, MonitorMarker,
    MonitorRequest, MonitorRequestStream,
};
use fdomain_fuchsia_update_channel as fupdate_channel;
use fdomain_fuchsia_update_channelcontrol::ChannelControlProxy;
use fdomain_fuchsia_update_installer::{self as finstaller, InstallerProxy};
use ffx_config::EnvironmentContext;
use ffx_update_args as args;
use ffx_update_args::ForceInstall;
use ffx_writer::SimpleWriter;
use fho::{Deferred, FfxContext, FfxMain, FfxTool, Result, bug, deferred, return_user_error};
use fidl::Signals;
use fidl_fuchsia_update_ext::State;
use fidl_fuchsia_update_installer_ext as installer;
use fuchsia_async::Timer;
use fuchsia_repo::repository::RepoProvider as _;
use futures::future::{FusedFuture as _, FutureExt as _};
use futures::{StreamExt as _, TryStreamExt as _, pin_mut, select};
use pkg::PkgServerInstanceInfo as _;
use std::path::PathBuf;
use std::time::Duration;
use target_connector::Connector;
use target_holders::fdomain::{RemoteControlProxyHolder, moniker};
use target_holders::{HostAddrHolder, TargetInfoQueryHolder};

mod server;

const WARNING_DURATION: Duration = Duration::from_secs(30);

#[derive(FfxTool)]
pub struct UpdateTool {
    #[command]
    cmd: args::Update,
    context: EnvironmentContext,
    #[with(moniker("/core/system-update"))]
    update_manager_proxy: ManagerProxy,
    #[with(moniker("/core/system-update"))]
    channel_provider_proxy: fupdate_channel::ProviderProxy,
    #[with(moniker("/core/system-update"))]
    channel_control_proxy: ChannelControlProxy,
    #[with(deferred(moniker("/core/system-update/system-updater")))]
    installer_proxy: Deferred<InstallerProxy>,
    #[with(moniker("/core/system-update"))]
    commit_status_provider_proxy: CommitStatusProviderProxy,
    target_spec: Deferred<TargetInfoQueryHolder>,
    rcs_proxy_connector: Connector<RemoteControlProxyHolder>,
    host_address: Deferred<HostAddrHolder>,
}

fho::embedded_plugin!(UpdateTool);

#[async_trait(?Send)]
impl FfxMain for UpdateTool {
    type Writer = SimpleWriter;

    type Error = ::fho::Error;

    /// Main entry point for the `update` subcommand.
    async fn main(self, mut writer: SimpleWriter) -> Result<()> {
        let update = self.cmd.clone();

        match update.cmd {
            args::Command::Channel(args::Channel { ref cmd }) => {
                handle_channel_control_cmd(
                    &cmd,
                    self.channel_provider_proxy,
                    self.channel_control_proxy,
                    &mut writer,
                )
                .await?;
            }
            args::Command::CheckNow(ref check_now) => {
                Box::pin(self.handle_check_now_cmd(check_now, &mut writer)).await?;
            }
            args::Command::ForceInstall(ref args) => {
                Box::pin(self.handle_force_install_cmd(args, &mut writer)).await?;
            }
            args::Command::WaitForCommit(_args) => {
                handle_wait_for_commit(
                    &self.commit_status_provider_proxy,
                    &mut Printer { writer, warning_duration: WARNING_DURATION },
                    WARNING_DURATION,
                )
                .await?;
            }
        }
        Ok(())
    }
}

impl UpdateTool {
    /// If there's a new version available, update to it, printing progress to the
    /// console during the process.
    async fn handle_check_now_cmd<W: std::io::Write>(
        self,
        cmd: &args::CheckNow,
        writer: &mut W,
    ) -> Result<()> {
        let package_server_task = if cmd.product_bundle {
            let product_path =
                Self::get_product_bundle_path(&cmd.product_bundle_path, &self.context)?;
            let repo_port: u16 = cmd.product_bundle_port(&self.context)?.try_into().unwrap();
            Some(
                Box::pin(server::package_server_task(
                    self.target_spec,
                    self.rcs_proxy_connector.clone(),
                    self.host_address,
                    self.context.clone(),
                    product_path,
                    repo_port,
                ))
                .await?,
            )
        } else if cmd.product_bundle_path.is_some() {
            return_user_error!(
                "Cannot specify a product bundle without the the `--product-bundle option."
            );
        } else {
            None
        };

        if let Some(server_task) = package_server_task {
            // Use select! to run the package server at the same time as the others. This is preferable
            // to using detach(), since we can get error result from the package server.

            // wait for the server to be registered before running the check.
            let check = async {
                Box::pin(server::wait_for_device_task(
                    server_task.repo_name.clone(),
                    self.rcs_proxy_connector.clone(),
                ))
                .await?;
                Self::check_for_update(self.update_manager_proxy.clone(), &cmd, writer).await
            };

            let fused_server_task = server_task.task.fuse();
            let check_task = check.fuse();

            pin_mut!(fused_server_task, check_task);

            let repo_name = server_task.repo_name.clone();

            select!(
                server_task_result = fused_server_task =>  {
                    // The server should start and run indefiniitely, if we get here there is a problem.
                    match server_task_result {
                        Ok(_) => return_user_error!("Package server exited successfully, but prematurely"),
                        Err(e) => return_user_error!("Package server failed to run: {e}")
                    }
                }
                update_task_result =  check_task => {
                   Box::pin(server::unregister_pb_repo_server(&repo_name, self.rcs_proxy_connector.clone())).await?;
                   return update_task_result}
            );
        } else {
            Self::check_for_update(self.update_manager_proxy.clone(), &cmd, writer).await?;
        }

        Ok(())
    }

    async fn check_for_update<W: std::io::Write>(
        update_manager_proxy: ManagerProxy,
        cmd: &args::CheckNow,
        writer: &mut W,
    ) -> Result<()> {
        let do_monitor = cmd.monitor || cmd.product_bundle;
        let options = CheckOptions {
            initiator: Some(if cmd.service_initiated {
                Initiator::Service
            } else {
                Initiator::User
            }),
            allow_attaching_to_existing_update_check: Some(true),
            ..Default::default()
        };

        // Create the monitor client if requested, or if using a product bundle as the source.
        // This is needed for product bundles to make sure the package server continues to run
        // until the update is completed.
        let (monitor_client, monitor_server) = if do_monitor {
            let client = update_manager_proxy.domain();
            let (client_end, request_stream) = client.create_request_stream::<MonitorMarker>();
            (Some(client_end), Some(request_stream))
        } else {
            (None, None)
        };

        match update_manager_proxy.check_now(&options, monitor_client).await {
            Ok(ok_result) => {
                if let Err(e) = ok_result {
                    return_user_error!("Not started error on check-now: {e:?}")
                }
            }
            Err(e) => return_user_error!("Error on check-now: {e:?}"),
        };
        writeln!(writer, "Checking for an update.").bug()?;
        if let Some(monitor_server) = monitor_server {
            monitor_state(monitor_server, writer).await?;
        }
        Ok(())
    }

    /// Change to a specific version, regardless of whether it's newer or older than
    /// the current system software.
    async fn handle_force_install_cmd<W: std::io::Write>(
        self,
        cmd: &ForceInstall,
        writer: &mut W,
    ) -> Result<()> {
        let (mut package_server_task, host_address) =
            if cmd.product_bundle || cmd.product_bundle_path.is_some() {
                let product_path =
                    Self::get_product_bundle_path(&cmd.product_bundle_path, &self.context)?;

                let repo_port: u16 = cmd.product_bundle_port(&self.context)?.try_into().unwrap();
                (
                    Some(
                        Box::pin(server::package_server_task(
                            self.target_spec,
                            self.rcs_proxy_connector.clone(),
                            self.host_address,
                            self.context.clone(),
                            product_path,
                            repo_port,
                        ))
                        .await?,
                    ),
                    None,
                )
            } else {
                (None, Some(self.host_address))
            };

        let installer_proxy = self.installer_proxy.await?;

        let update_url = if let Some(url) = &cmd.update_url {
            url.parse().bug_context("parsing update url")?
        } else if cmd.packageless {
            if let Some(server_task) = &mut package_server_task {
                // Need to get the repo host address from the package server task, because it might
                // be a tunneled connection.
                let mut repo_host =
                    timeout::timeout(Duration::from_secs(30), server_task.repo_host_rx.next())
                        .await
                        .bug_context("Timeout waiting for the repo host address")?
                        .bug_context("Failed to get the repo host address")?;
                // If package_server_task enters connection loop, we want to get the latest address.
                while let Ok(Some(host)) = server_task.repo_host_rx.try_next() {
                    repo_host = host;
                }
                let first_alias = (|| -> Result<String, anyhow::Error> {
                    let product_path =
                        Self::get_product_bundle_path(&cmd.product_bundle_path, &self.context)?;
                    let repos = product_bundle::get_repositories(product_path.try_into()?)?;
                    let repo = repos.first().ok_or_else(|| anyhow::anyhow!("No repositories found"))?;
                    let alias = repo.aliases().first().ok_or_else(|| anyhow::anyhow!("No aliases found"))?;
                    Ok(alias.to_owned())
                })()
                .unwrap_or_else(|e| {
                    log::warn!("Could not determine the first alias for the product bundle: {e}, defaulting to 'fuchsia.com'");
                    "fuchsia.com".to_string()
                });
                let url = format!(
                    "http://{}/{}.{}/ota_manifest",
                    repo_host, server_task.repo_name, first_alias
                );
                url.parse().with_bug_context(|| format!("parsing update url: {url}"))?
            } else {
                let instance_root = self.context.get("repository.process_dir").bug()?;
                let mgr = pkg::PkgServerInstances::new(instance_root);
                let mut instances = mgr.list_instances().bug()?;
                instances.retain(|i| i.is_running());
                match instances.as_slice() {
                    [] => return_user_error!(
                        "No package servers are running, could not determine the packageless update url"
                    ),
                    [instance] => {
                        let host_address: Option<ffx_ssh::parse::HostAddr> =
                            host_address.unwrap().await?.into();
                        match pkg::repo::create_repo_host(
                            instance.address,
                            host_address.map(|t| t.0),
                        )
                        .bug()?
                        {
                            pkg::repo::RepoHostAddr::Direct(repo_host) => {
                                let url =
                                    format!("http://{}/{}/ota_manifest", repo_host, instance.name);
                                url.parse()
                                    .with_bug_context(|| format!("parsing update url: {url}"))?
                            }
                            pkg::repo::RepoHostAddr::Tunnel => {
                                return_user_error!(
                                    "Tunnel required, cannot use existing package server, try --product-bundle"
                                );
                            }
                        }
                    }
                    _ => return_user_error!(
                        "Multiple package servers are running, could not determine the packageless update url, please specify\n{instances:#?}"
                    ),
                }
            }
        } else {
            http::Uri::from_static("fuchsia-pkg://fuchsia.com/update")
        };
        writeln!(writer, "Using update url: {update_url}").bug()?;

        if let Some(server_task) = package_server_task {
            // Use select! to run the package server at the same time as the others. This is preferable
            // to using detach(), since we can get error result from the package server.

            // wait for the server to be registered before running the check.
            let install = async {
                // Packageless update does not need the server to be registered on the target.
                if !cmd.packageless {
                    Box::pin(server::wait_for_device_task(
                        server_task.repo_name.clone(),
                        self.rcs_proxy_connector.clone(),
                    ))
                    .await?;
                }
                Self::force_install(update_url, cmd.reboot, installer_proxy, writer).await
            };

            let fused_server_task = server_task.task.fuse();
            let install_task = install.fuse();

            pin_mut!(fused_server_task, install_task);

            let repo_name = server_task.repo_name.clone();

            select!(
                server_task_result = fused_server_task =>  {
                    // The server should start and run indefiniitely, if we get here there is a problem.
                    match server_task_result {
                        Ok(_) => return_user_error!("Package server exited successfully, but prematurely"),
                        Err(e) => return_user_error!("Package server failed to run: {e}")
                    }
                }
                update_task_result =  install_task => {
                    if cmd.reboot {
                        Timer::new(Duration::from_secs(15)).await;
                    }
                    Box::pin(server::unregister_pb_repo_server(&repo_name, self.rcs_proxy_connector.clone())).await?;
                    return update_task_result}
            );
        } else {
            Self::force_install(update_url, cmd.reboot, installer_proxy, writer).await
        }
    }

    async fn force_install<W: std::io::Write>(
        update_url: http::Uri,
        reboot: bool,
        installer_proxy: InstallerProxy,
        writer: &mut W,
    ) -> Result<()> {
        let options = installer::Options {
            initiator: installer::Initiator::User,
            should_write_recovery: true,
            allow_attach_to_existing_attempt: true,
            manifest_range: None,
        };

        let client = installer_proxy.domain();
        let (reboot_controller, reboot_controller_server_end) =
            client.create_proxy::<finstaller::RebootControllerMarker>();

        let mut update_attempt: installer::UpdateAttemptFDomain = installer::start_update_fdomain(
            &update_url,
            options,
            &installer_proxy,
            Some(reboot_controller_server_end),
        )
        .await
        .bug_context("starting update")?;

        writeln!(writer, "Installing an update.").bug()?;
        if update_url.scheme_str() == Some("fuchsia-pkg") {
            writeln!(
                writer,
                "Progress reporting is based on the fraction of packages resolved, so if one package is much
larger than the others, then the reported progress could appear to stall near the end.
Until the update process is improved to have more granular reporting, try using
    ffx inspect show 'core/pkg-resolver'
for more detail on the progress of update-related downloads.\n"
            )
            .bug()?;
        }
        if !reboot {
            reboot_controller.detach().bug_context("notify installer do not reboot")?;
        }
        write_progress("\nStarting install", writer)?;
        while let Some(state) = update_attempt.try_next().await.bug_context("getting next state")? {
            match state {
                fidl_fuchsia_update_installer_ext::State::WaitToReboot(info) => {
                    // if waiting for reboot, wait for a while to get a head start, hopefully returning after
                    // the shutdown.
                    write_progress(
                        &format!(
                            "{:.1} {}/{} Waiting to Reboot",
                            info.progress().fraction_completed() * 100.0,
                            info.progress().bytes_downloaded(),
                            info.info().download_size()
                        ),
                        writer,
                    )?;
                    write!(writer, "\n").bug()?;
                    if reboot {
                        return Ok(());
                    }
                }
                fidl_fuchsia_update_installer_ext::State::Reboot(info)
                | fidl_fuchsia_update_installer_ext::State::DeferReboot(info)
                | fidl_fuchsia_update_installer_ext::State::Complete(info) => {
                    write_progress(
                        &format!(
                            "{:.1} {}/{} Complete",
                            info.progress().fraction_completed() * 100.0,
                            info.progress().bytes_downloaded(),
                            info.info().download_size()
                        ),
                        writer,
                    )?;
                    return Ok(());
                }

                fidl_fuchsia_update_installer_ext::State::FailPrepare(reason) => {
                    return_user_error!("Install failed: {reason:?}")
                }
                fidl_fuchsia_update_installer_ext::State::FailStage(data) => {
                    return_user_error!("Install failed: {:?}", data.reason())
                }
                fidl_fuchsia_update_installer_ext::State::FailFetch(data) => {
                    return_user_error!("Install failed: {:?}", data.reason())
                }
                fidl_fuchsia_update_installer_ext::State::Canceled => {
                    return_user_error!("Install failed: canceled")
                }

                fidl_fuchsia_update_installer_ext::State::Prepare => {
                    write_progress(&format!("{:.1} {}/{} Preparing", 0.0, 0, "?"), writer)?
                }
                fidl_fuchsia_update_installer_ext::State::Stage(info) => write_progress(
                    &format!(
                        "{:.1} {}/{} Staging",
                        info.progress().fraction_completed() * 100.0,
                        info.progress().bytes_downloaded(),
                        info.info().download_size()
                    ),
                    writer,
                )?,
                fidl_fuchsia_update_installer_ext::State::Fetch(info) => write_progress(
                    &format!(
                        "{:.1} {}/{} Fetching",
                        info.progress().fraction_completed() * 100.0,
                        info.progress().bytes_downloaded(),
                        info.info().download_size()
                    ),
                    writer,
                )?,
                fidl_fuchsia_update_installer_ext::State::Commit(info) => write_progress(
                    &format!(
                        "{:.1} {}/{} Commit",
                        info.progress().fraction_completed() * 100.0,
                        info.progress().bytes_downloaded(),
                        info.info().download_size()
                    ),
                    writer,
                )?,
                fidl_fuchsia_update_installer_ext::State::FailCommit(info) => write_progress(
                    &format!(
                        "{:.1} {}/{} Failed commit",
                        info.progress().fraction_completed() * 100.0,
                        info.progress().bytes_downloaded(),
                        info.info().download_size()
                    ),
                    writer,
                )?,
            }
        }

        Ok(())
    }

    fn get_product_bundle_path(
        product_bundle_path: &Option<PathBuf>,
        context: &EnvironmentContext,
    ) -> Result<PathBuf> {
        let pb_path = match product_bundle_path {
            Some(product_path) => product_path.clone(),
            None => {
                if let Some(product_path) =
                    context.get::<Option<PathBuf>, _>("product.path").bug()?
                {
                    product_path
                } else {
                    return_user_error!(
                        "No product bundle path specified nor configured. Run `ffx product-bundle get` or specify one with the appropriate flag."
                    )
                }
            }
        };
        Ok(pb_path)
    }
}

fn write_progress<W: std::io::Write>(s: &str, writer: &mut W) -> Result<()> {
    // Use escape sequences to make this line overwrite the current terminal line.
    // \r: send cursor to start of line
    // \x1b[K: clear to end of line
    if termion::is_tty(&std::io::stdout()) {
        write!(writer, "\r{s}\x1b[K").bug()?;
    } else {
        writeln!(writer, "{s}").bug()?;
    }
    writer.flush().bug()
}

/// Handle subcommands for `update channel`.
async fn handle_channel_control_cmd<W: std::io::Write>(
    cmd: &args::channel::Command,
    channel_provider: fupdate_channel::ProviderProxy,
    channel_control: fdomain_fuchsia_update_channelcontrol::ChannelControlProxy,
    writer: &mut W,
) -> Result<()> {
    match cmd {
        args::channel::Command::Get(_) => {
            let channel = channel_provider.get_current().await.map_err(|e: fidl::Error| bug!(e))?;
            writeln!(writer, "current channel: {}", channel).bug()?;
        }
        args::channel::Command::Target(_) => {
            let channel = channel_control.get_target().await.map_err(|e: fidl::Error| bug!(e))?;
            writeln!(writer, "target channel: {}", channel).bug()?;
        }
        args::channel::Command::Set(args::channel::Set { channel }) => {
            channel_control.set_target(&channel).await.map_err(|e: fidl::Error| bug!(e))?;
        }
        args::channel::Command::List(_) => {
            let channels =
                channel_control.get_target_list().await.map_err(|e: fidl::Error| bug!(e))?;
            if channels.is_empty() {
                writeln!(writer, "known channels list is empty.").bug()?;
            } else {
                writeln!(writer, "known channels:").bug()?;
                for channel in channels {
                    writeln!(writer, "{}", channel).bug()?;
                }
            }
        }
    }
    Ok(())
}

/// Wait for and print state changes. For informational / DX purposes.
async fn monitor_state<W: std::io::Write>(
    mut stream: MonitorRequestStream,
    writer: &mut W,
) -> Result<()> {
    while let Some(event) = stream.try_next().await.bug()? {
        match event {
            MonitorRequest::OnState { state, responder } => {
                responder.send().bug()?;

                let state = State::from(state);
                // If this gets set to `Some(_)` then we must exit with a user error.
                let mut critical_error: Option<String> = None;
                match state.clone() {
                    State::CheckingForUpdates => write_progress("Checking for updates", writer)?,
                    State::NoUpdateAvailable => write_progress("No update available", writer)?,
                    State::InstallationDeferredByPolicy(installation_deferred_data) => {
                        let reason =
                            if let Some(reason) = installation_deferred_data.deferral_reason {
                                format!("{reason:?}")
                            } else {
                                "".into()
                            };
                        write_progress(&format!("Update deferred by policy: {reason}"), writer)?
                    }
                    State::InstallingUpdate(installing_data) => {
                        let pct = if let Some(progress) = installing_data.installation_progress {
                            format!("{:.2}", progress.fraction_completed.unwrap_or(0.0) * 100.0)
                        } else {
                            "".into()
                        };
                        write_progress(&format!("{pct} Installing"), writer)?;
                    }
                    State::WaitingForReboot(installing_data) => {
                        let pct = if let Some(progress) = installing_data.installation_progress {
                            format!("{:.2}", progress.fraction_completed.unwrap_or(0.0) * 100.0)
                        } else {
                            "".into()
                        };
                        write_progress(&format!("{pct} Waiting for reboot"), writer)?;
                        Timer::new(Duration::from_secs(15)).await;
                    }
                    State::InstallationError(installing_data) => {
                        let pct = if let Some(progress) = installing_data.installation_progress {
                            format!("{:.2}", progress.fraction_completed.unwrap_or(0.0) * 100.0)
                        } else {
                            "".into()
                        };
                        critical_error
                            .replace(format!("Internal error encountered at {pct} percent."));
                    }
                    State::ErrorCheckingForUpdate => {
                        critical_error.replace(format!(
                            "{} encountered an error while checking for an update.",
                            ManagerMarker::PROTOCOL_NAME
                        ));
                    }
                };
                if let Some(e) = critical_error {
                    return_user_error!("Update failed: {}", e)
                }
                if state.is_terminal() {
                    writeln!(writer, "\n").bug()?;
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// The set of events associated with the `wait-for-commit` path.
#[derive(Debug, PartialEq)]
enum CommitEvent {
    Begin,
    Warning,
    End,
}

/// An observer of `update wait-for-commit`.
trait CommitObserver {
    fn on_event(&mut self, event: CommitEvent) -> std::io::Result<()>;
}

/// A `CommitObserver` that forwards the events to writer.
struct Printer<W: std::io::Write> {
    writer: W,
    warning_duration: Duration,
}

impl<W: std::io::Write> CommitObserver for Printer<W> {
    fn on_event(&mut self, event: CommitEvent) -> std::io::Result<()> {
        match event {
            CommitEvent::Begin => writeln!(&mut self.writer, "Waiting for commit."),
            CommitEvent::Warning => writeln!(
                &mut self.writer,
                "It's been {} seconds. Something may be wrong.",
                self.warning_duration.as_secs(),
            ),
            CommitEvent::End => writeln!(&mut self.writer, "Committed!"),
        }
    }
}

/// Waits for the system to commit (e.g. when the EventPair observes a signal).
async fn wait_for_commit(proxy: &CommitStatusProviderProxy) -> Result<()> {
    let p = proxy.is_current_system_committed().await.bug_context("while obtaining EventPair")?;
    fdomain_client::OnFDomainSignals::new(&p.as_handle_ref(), Signals::USER_0)
        .await
        .map_err(|e: fdomain_client::Error| bug!(e))
        .bug_context("while waiting for the commit")?;
    Ok(())
}

/// Waits for the commit and sends updates to the observer. This is abstracted from the regular
/// `handle_wait_for_commit` fn so we can test events without having to wait the `warning_duration`.
/// The [testability rubric](https://fuchsia.dev/fuchsia-src/concepts/testing/testability_rubric)
/// exempts logs from testing, but in this case we test them anyway because of the additional layer
/// of complexity that the warning timeout introduces.
async fn handle_wait_for_commit(
    proxy: &CommitStatusProviderProxy,
    observer: &mut impl CommitObserver,
    warning_duration: Duration,
) -> Result<()> {
    observer.on_event(CommitEvent::Begin).bug_context("while handling a begin event")?;

    let commit_fut = wait_for_commit(proxy).fuse();
    futures::pin_mut!(commit_fut);
    let mut timer_fut = fuchsia_async::Timer::new(warning_duration).fuse();

    // Send a warning after the WARNING_DURATION.
    let () = futures::select! {
        commit_res = commit_fut => commit_res?,
        _ = timer_fut => observer.on_event(CommitEvent::Warning).bug_context("while handling a warning event")?,
    };

    // If we timed out on WARNING_DURATION, try again.
    if !commit_fut.is_terminated() {
        let () = commit_fut.await.bug_context("while calling wait_for_commit second")?;
    }

    let () = observer.on_event(CommitEvent::End).bug_context("while handling a end event")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fdomain_client::Peered;
    use fdomain_fuchsia_update::{CommitStatusProviderRequest, ManagerRequest};
    use fdomain_fuchsia_update_channelcontrol::ChannelControlRequest;
    use ffx_update_args::Update;
    use ffx_writer::TestBuffers;
    use futures::prelude::*;
    use mock_installer_fdomain::MockUpdateInstallerService;
    use std::sync::Arc;
    use target_holders::fdomain::{fake_async_proxy, fake_proxy};

    async fn perform_channel_provider_test<V, O>(
        argument: args::channel::Command,
        verifier: V,
        output: O,
    ) where
        V: Fn(fupdate_channel::ProviderRequest),
        O: Fn(String),
    {
        let client = fdomain_local::local_client_empty();
        let (proxy, mut stream) =
            client.create_proxy_and_stream::<fupdate_channel::ProviderMarker>();
        let mut buf = Vec::new();
        let fut = async {
            assert_matches!(
                handle_channel_control_cmd(
                    &argument,
                    proxy,
                    client.create_proxy::<
                        fdomain_fuchsia_update_channelcontrol::ChannelControlMarker,
                    >()
                    .0,
                    &mut buf
                )
                .await,
                Ok(())
            );
        };
        let stream_fut = async move {
            let result = stream.next().await.unwrap();
            match result {
                Ok(cmd) => verifier(cmd),
                err => panic!("Err in request handler: {:?}", err),
            }
        };
        future::join(fut, stream_fut).await;
        let out = String::from_utf8(buf).unwrap();
        output(out);
    }

    async fn perform_channel_control_test<V, O>(
        argument: args::channel::Command,
        verifier: V,
        output: O,
    ) where
        V: Fn(ChannelControlRequest),
        O: Fn(String),
    {
        let client = fdomain_local::local_client_empty();
        let (proxy, mut stream) = client
            .create_proxy_and_stream::<fdomain_fuchsia_update_channelcontrol::ChannelControlMarker>(
            );
        let mut buf = Vec::new();
        let fut = async {
            assert_matches!(
                handle_channel_control_cmd(
                    &argument,
                    client.create_proxy::<fupdate_channel::ProviderMarker>().0,
                    proxy,
                    &mut buf
                )
                .await,
                Ok(())
            );
        };
        let stream_fut = async move {
            let result = stream.next().await.unwrap();
            match result {
                Ok(cmd) => verifier(cmd),
                err => panic!("Err in request handler: {:?}", err),
            }
        };
        future::join(fut, stream_fut).await;
        let out = String::from_utf8(buf).unwrap();
        output(out);
    }

    async fn write_product_bundle(pb_dir: &camino::Utf8Path) {
        let blobs_dir = pb_dir.join("blobs");

        let repo_name = "fuchsia.com";
        let metadata_path = pb_dir.join(repo_name);
        fuchsia_repo::test_utils::make_repo_dir(metadata_path.as_ref(), blobs_dir.as_ref(), None)
            .await;

        std::fs::write(metadata_path.join("ota_manifest"), b"mock ota manifest content").unwrap();

        let pb = product_bundle::ProductBundle::V2(product_bundle::ProductBundleV2 {
            product_name: "test".into(),
            product_version: "test-product-version".into(),
            partitions: assembly_partitions_config::PartitionsConfig::default(),
            sdk_version: "test-sdk-version".into(),
            system_a: None,
            system_b: None,
            system_r: None,
            platform_tools_a: vec![],
            platform_tools_b: vec![],
            platform_tools_r: vec![],
            repositories: vec![product_bundle::Repository {
                name: repo_name.into(),
                metadata_path: metadata_path.into(),
                blobs_path: blobs_dir.clone().into(),
                delivery_blob_type: 1,
                root_private_key_path: None,
                targets_private_key_path: None,
                snapshot_private_key_path: None,
                timestamp_private_key_path: None,
                ota_manifest_signature_path: None,
            }],
            update_package_hash: None,
            virtual_devices_path: None,
            release_info: None,
        });
        pb.write(&pb_dir).unwrap();
    }

    #[fuchsia::test]
    async fn test_channel_get() {
        perform_channel_provider_test(
            args::channel::Command::Get(args::channel::Get {}),
            |cmd| match cmd {
                fupdate_channel::ProviderRequest::GetCurrent { responder } => {
                    responder.send("channel").unwrap();
                }
            },
            |output| assert_eq!(output, "current channel: channel\n"),
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_channel_target() {
        perform_channel_control_test(
            args::channel::Command::Target(args::channel::Target {}),
            |cmd| match cmd {
                ChannelControlRequest::GetTarget { responder } => {
                    responder.send("target-channel").unwrap();
                }
                request => panic!("Unexpected request: {:?}", request),
            },
            |output| assert_eq!(output, "target channel: target-channel\n"),
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_channel_set() {
        perform_channel_control_test(
            args::channel::Command::Set(args::channel::Set { channel: "new-channel".to_string() }),
            |cmd| match cmd {
                ChannelControlRequest::SetTarget { channel, responder } => {
                    assert_eq!(channel, "new-channel");
                    responder.send().unwrap();
                }
                request => panic!("Unexpected request: {:?}", request),
            },
            |output| assert!(output.is_empty()),
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_channel_list_no_channels() {
        perform_channel_control_test(
            args::channel::Command::List(args::channel::List {}),
            |cmd| match cmd {
                ChannelControlRequest::GetTargetList { responder } => {
                    responder.send(&[]).unwrap();
                }
                request => panic!("Unexpected request: {:?}", request),
            },
            |output| assert_eq!(output, "known channels list is empty.\n"),
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_channel_list_with_channels() {
        perform_channel_control_test(
            args::channel::Command::List(args::channel::List {}),
            |cmd| match cmd {
                ChannelControlRequest::GetTargetList { responder } => {
                    responder
                        .send(&["some-channel".to_owned(), "other-channel".to_owned()])
                        .unwrap();
                }
                request => panic!("Unexpected request: {:?}", request),
            },
            |output| assert_eq!(output, "known channels:\nsome-channel\nother-channel\n"),
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_check_now() {
        let client = fdomain_local::local_client_empty();
        let test_env = ffx_config::test_init().expect("test env");

        let fake_installer_proxy =
            Deferred::from_output(Ok(fake_proxy(Arc::clone(&client), move |req| {
                panic!("Unexpected request: {:?}", req)
            })));
        let fake_channel_provider_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_channel_control_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_commit_status_provider_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_update_manager_proxy = fake_proxy(Arc::clone(&client), move |req| {
            match req {
                ManagerRequest::CheckNow { responder, .. } => {
                    responder.send(Ok(())).expect("send ok")
                }
                _ => panic!("Unexpected request: {:?}", req),
            };
        });

        let fake_env = crate::server::tests::FakeTestEnv::new(&test_env).await;

        let tool = UpdateTool {
            cmd: Update {
                cmd: args::Command::CheckNow(args::CheckNow {
                    service_initiated: false,
                    monitor: true,
                    product_bundle: false,
                    product_bundle_path: None,
                    product_bundle_port: None,
                }),
            },
            context: test_env.context.clone(),
            update_manager_proxy: fake_update_manager_proxy,
            channel_provider_proxy: fake_channel_provider_proxy,
            channel_control_proxy: fake_channel_control_proxy,
            installer_proxy: fake_installer_proxy,
            commit_status_provider_proxy: fake_commit_status_provider_proxy,
            target_spec: fake_env.target_spec,
            rcs_proxy_connector: fake_env.rcs_proxy_connector,
            host_address: fake_env.host_address,
        };
        let buffers = TestBuffers::default();
        let writer = SimpleWriter::new_test(&buffers);

        let result = tool.main(writer).await;

        let (stdout, stderr) = buffers.into_strings();

        assert!(result.is_ok(), "Expected Ok got {result:?}");
        assert_eq!(stdout, "Checking for an update.\n");
        assert_eq!(stderr, "");
    }

    #[fuchsia::test]
    async fn test_force_install() {
        let client = fdomain_local::local_client_empty();
        let test_env = ffx_config::test_init().expect("test env");
        let update_info = installer::UpdateInfo::builder().download_size(1000).build();
        let mock_installer = Arc::new(MockUpdateInstallerService::with_states(vec![
            installer::State::Prepare,
            installer::State::Fetch(
                installer::UpdateInfoAndProgress::new(update_info, installer::Progress::none())
                    .unwrap(),
            ),
            installer::State::Stage(
                installer::UpdateInfoAndProgress::new(
                    update_info,
                    installer::Progress::builder()
                        .fraction_completed(0.5)
                        .bytes_downloaded(500)
                        .build(),
                )
                .unwrap(),
            ),
            installer::State::WaitToReboot(installer::UpdateInfoAndProgress::done(update_info)),
        ]));
        let fake_installer_proxy = mock_installer.spawn_installer_service(Arc::clone(&client));

        let args = ForceInstall {
            reboot: true,
            update_url: Some("fuchsia-pkg://fuchsia.test/update".into()),
            product_bundle: false,
            product_bundle_port: None,
            product_bundle_path: None,
            packageless: false,
        };

        let fake_update_manager_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_channel_provider_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_channel_control_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_commit_status_provider_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_env = crate::server::tests::FakeTestEnv::new(&test_env).await;

        let tool = UpdateTool {
            cmd: Update { cmd: args::Command::ForceInstall(args) },
            context: test_env.context.clone(),
            update_manager_proxy: fake_update_manager_proxy,
            channel_provider_proxy: fake_channel_provider_proxy,
            channel_control_proxy: fake_channel_control_proxy,
            installer_proxy: Deferred::from_output(Ok(fake_installer_proxy)),
            commit_status_provider_proxy: fake_commit_status_provider_proxy,
            target_spec: fake_env.target_spec,
            rcs_proxy_connector: fake_env.rcs_proxy_connector,
            host_address: fake_env.host_address,
        };

        let buffers = TestBuffers::default();
        let writer = SimpleWriter::new_test(&buffers);
        tool.main(writer).await.expect("success");

        let (stdout, stderr) = buffers.into_strings();

        assert_eq!(stderr, "");
        assert_eq!(
            stdout,
            "Using update url: fuchsia-pkg://fuchsia.test/update\n\
            Installing an update.\n\
            Progress reporting is based on the fraction of packages resolved, so if one package is much\n\
            larger than the others, then the reported progress could appear to stall near the end.\n\
            Until the update process is improved to have more granular reporting, try using\
            \n    ffx inspect show 'core/pkg-resolver'\n\
            for more detail on the progress of update-related downloads.\n\n\n\
            Starting install\n\
            0.0 0/? Preparing\n\
            0.0 0/1000 Fetching\n\
            50.0 500/1000 Staging\n\
            100.0 1000/1000 Waiting to Reboot\n\n"
        );
    }

    #[fuchsia::test]
    async fn test_force_install_packageless() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path().to_path_buf();
        let test_env = ffx_config::test_env()
            .user_config("repository.process_dir", temp_path.to_str().unwrap())
            .build()
            .unwrap();
        let client = fdomain_local::local_client_empty();

        let update_info = installer::UpdateInfo::builder().download_size(1000).build();
        let mock_installer = Arc::new(MockUpdateInstallerService::with_states(vec![
            installer::State::Prepare,
            installer::State::Fetch(
                installer::UpdateInfoAndProgress::new(update_info, installer::Progress::none())
                    .unwrap(),
            ),
            installer::State::Stage(
                installer::UpdateInfoAndProgress::new(
                    update_info,
                    installer::Progress::builder()
                        .fraction_completed(0.5)
                        .bytes_downloaded(500)
                        .build(),
                )
                .unwrap(),
            ),
            installer::State::WaitToReboot(installer::UpdateInfoAndProgress::done(update_info)),
        ]));
        let fake_installer_proxy = mock_installer.spawn_installer_service(Arc::clone(&client));

        let args = ForceInstall {
            reboot: true,
            update_url: None,
            product_bundle: false,
            product_bundle_port: None,
            product_bundle_path: None,
            packageless: true,
        };

        let fake_update_manager_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_channel_provider_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_channel_control_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));
        let fake_commit_status_provider_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {:?}", req));

        let repo_url = fuchsia_url::RepositoryUrl::parse_host("fuchsia.com".into()).unwrap();
        let repo_config = fidl_fuchsia_pkg_ext::RepositoryConfigBuilder::new(repo_url).build();

        pkg::write_instance_info(
            &test_env.context,
            pkg::ServerMode::Foreground,
            "devhost.fuchsia.com",
            &"1.2.3.4:8083".parse().unwrap(),
            fuchsia_repo::repository::RepositorySpec::Pm {
                path: "/tmp".into(),
                aliases: std::collections::BTreeSet::new(),
            },
            fidl_fuchsia_pkg_ext::RepositoryStorageType::Ephemeral,
            fidl_fuchsia_pkg_ext::RepositoryRegistrationAliasConflictMode::Replace,
            repo_config,
        )
        .await
        .expect("write instance info");

        let fake_env = crate::server::tests::FakeTestEnv::new(&test_env).await;

        let tool = UpdateTool {
            cmd: Update { cmd: args::Command::ForceInstall(args) },
            context: test_env.context.clone(),
            update_manager_proxy: fake_update_manager_proxy,
            channel_provider_proxy: fake_channel_provider_proxy,
            channel_control_proxy: fake_channel_control_proxy,
            installer_proxy: Deferred::from_output(Ok(fake_installer_proxy)),
            commit_status_provider_proxy: fake_commit_status_provider_proxy,
            target_spec: fake_env.target_spec,
            rcs_proxy_connector: fake_env.rcs_proxy_connector,
            host_address: fake_env.host_address,
        };

        let buffers = TestBuffers::default();
        let writer = SimpleWriter::new_test(&buffers);
        tool.main(writer).await.expect("success");

        let (stdout, stderr) = buffers.into_strings();

        assert_eq!(stderr, "");
        assert_eq!(
            stdout,
            "Using update url: http://127.0.0.1:8083/devhost.fuchsia.com/ota_manifest\n\
            Installing an update.\n\n\
            Starting install\n\
            0.0 0/? Preparing\n\
            0.0 0/1000 Fetching\n\
            50.0 500/1000 Staging\n\
            100.0 1000/1000 Waiting to Reboot\n\n"
        );
    }

    #[fuchsia::test]
    async fn test_force_install_product_bundle_packageless() {
        let test_env = ffx_config::test_init().expect("test env");
        let client = fdomain_local::local_client_empty();
        let pb_dir_temp = tempfile::tempdir().unwrap();
        let pb_dir = camino::Utf8PathBuf::from_path_buf(pb_dir_temp.path().to_path_buf()).unwrap();
        write_product_bundle(&pb_dir).await;

        let args = ForceInstall {
            reboot: true,
            update_url: None,
            product_bundle: false,
            product_bundle_port: None,
            product_bundle_path: Some(pb_dir_temp.path().to_path_buf()),
            packageless: true,
        };

        let update_info = installer::UpdateInfo::builder().download_size(1000).build();
        let (mut states_tx, states_rx) = futures::channel::mpsc::channel(1);
        let mock_installer =
            Arc::new(MockUpdateInstallerService::builder().states_receiver(states_rx).build());
        let fake_installer_proxy =
            Arc::clone(&mock_installer).spawn_installer_service(Arc::clone(&client));

        let fake_update_manager_proxy = fake_proxy(Arc::clone(&client), move |req| match req {
            fdomain_fuchsia_update::ManagerRequest::CheckNow { responder, options: _, .. } => {
                responder.send(Ok(())).unwrap();
            }
            _ => panic!("Unexpected request: {req:?}"),
        });
        let fake_channel_provider_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {req:?}"));
        let fake_channel_control_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {req:?}"));
        let fake_commit_status_provider_proxy =
            fake_proxy(Arc::clone(&client), move |req| panic!("Unexpected request: {req:?}"));

        let fake_env = crate::server::tests::FakeTestEnv::new(&test_env).await;

        let tool = UpdateTool {
            cmd: Update { cmd: args::Command::ForceInstall(args) },
            context: test_env.context.clone(),
            update_manager_proxy: fake_update_manager_proxy,
            channel_provider_proxy: fake_channel_provider_proxy,
            channel_control_proxy: fake_channel_control_proxy,
            installer_proxy: Deferred::from_output(Ok(fake_installer_proxy)),
            commit_status_provider_proxy: fake_commit_status_provider_proxy,
            target_spec: fake_env.target_spec,
            rcs_proxy_connector: fake_env.rcs_proxy_connector,
            host_address: fake_env.host_address,
        };

        let buffers = TestBuffers::default();
        let writer = SimpleWriter::new_test(&buffers);
        let tool_task = fuchsia_async::Task::local(async move {
            tool.main(writer).await.expect("success");
        });

        let mut url_str = None;
        for _ in 0..100 {
            let args = mock_installer.captured_args().lock();
            if let Some(mock_installer_fdomain::CapturedUpdateInstallerRequest::StartUpdate {
                url,
                ..
            }) = args.get(0)
            {
                url_str = Some(url.clone());
                break;
            }
            drop(args);
            fuchsia_async::Timer::new(std::time::Duration::from_millis(100)).await;
        }
        let url = url_str.expect("StartUpdate should be called");
        let client = fuchsia_hyper::new_client();
        let res = client
            .request(hyper::Request::get(&url).body(hyper::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), hyper::StatusCode::OK);

        // Unblock the update process after the URL is verified.
        states_tx.send(installer::State::Prepare).await.unwrap();
        states_tx
            .send(installer::State::Fetch(
                installer::UpdateInfoAndProgress::new(update_info, installer::Progress::none())
                    .unwrap(),
            ))
            .await
            .unwrap();
        states_tx
            .send(installer::State::Stage(
                installer::UpdateInfoAndProgress::new(
                    update_info,
                    installer::Progress::builder()
                        .fraction_completed(0.5)
                        .bytes_downloaded(500)
                        .build(),
                )
                .unwrap(),
            ))
            .await
            .unwrap();
        states_tx
            .send(installer::State::WaitToReboot(installer::UpdateInfoAndProgress::done(
                update_info,
            )))
            .await
            .unwrap();
        drop(states_tx);

        tool_task.await;

        let (stdout, stderr) = buffers.into_strings();
        assert_eq!(stderr, "");
        assert!(
            stdout.ends_with(
                "Installing an update.\n\n\
            Starting install\n\
            0.0 0/? Preparing\n\
            0.0 0/1000 Fetching\n\
            50.0 500/1000 Staging\n\
            100.0 1000/1000 Waiting to Reboot\n\n"
            ),
            "stdout: {stdout}",
        );
    }

    struct TestCommitObserver {
        events: Vec<CommitEvent>,
    }
    impl TestCommitObserver {
        fn new() -> Self {
            Self { events: vec![] }
        }
        fn take_events(&mut self) -> Vec<CommitEvent> {
            self.events.drain(..).collect()
        }
    }
    impl CommitObserver for TestCommitObserver {
        fn on_event(&mut self, event: CommitEvent) -> std::io::Result<()> {
            self.events.push(event);
            Ok(())
        }
    }

    #[fuchsia::test]
    async fn test_wait_for_commit() {
        let client = fdomain_local::local_client_empty();
        let client_clone = Arc::clone(&client);
        let proxy = fake_async_proxy(Arc::clone(&client), async move |req| {
            let CommitStatusProviderRequest::IsCurrentSystemCommitted { responder } = req;

            let (lhs, rhs) = client_clone.create_event_pair();
            let () = responder.send(lhs).unwrap();

            fuchsia_async::Timer::new(Duration::from_millis(500)).await;

            let () = rhs.signal_peer(Signals::NONE, Signals::USER_0).await.unwrap();

            ()
        });

        let mut observer = TestCommitObserver::new();

        handle_wait_for_commit(&proxy, &mut observer, Duration::from_millis(1000)).await.unwrap();
        assert_eq!(observer.take_events(), &[CommitEvent::Begin, CommitEvent::End]);

        handle_wait_for_commit(&proxy, &mut observer, Duration::from_millis(50)).await.unwrap();
        assert_eq!(
            observer.take_events(),
            &[CommitEvent::Begin, CommitEvent::Warning, CommitEvent::End]
        );
    }
}
