// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use async_trait::async_trait;
use errors::ffx_bail;
use ffx_config::EnvironmentContext;
use ffx_target_repository_deregister_args::DeregisterCommand;
use fho::{daemon_protocol, FfxContext, FfxMain, FfxTool, SimpleWriter};
use fidl_fuchsia_developer_ffx::RepositoryRegistryProxy;
use fidl_fuchsia_developer_ffx_ext::RepositoryError;

#[derive(FfxTool)]
pub struct DeregisterTool {
    #[command]
    cmd: DeregisterCommand,
    #[with(daemon_protocol())]
    repos: RepositoryRegistryProxy,
    context: EnvironmentContext,
}

fho::embedded_plugin!(DeregisterTool);

#[async_trait(?Send)]
impl FfxMain for DeregisterTool {
    type Writer = SimpleWriter;
    async fn main(self, _writer: Self::Writer) -> fho::Result<()> {
        deregister(
            ffx_target::get_target_specifier(&self.context)
                .await
                .user_message("Failed to get target specifier from config")?,
            self.cmd,
            self.repos,
        )
        .await?;
        Ok(())
    }
}

async fn deregister(
    target_str: Option<String>,
    cmd: DeregisterCommand,
    repos: RepositoryRegistryProxy,
) -> Result<()> {
    let repo_name = if let Some(repo_name) = cmd.repository {
        repo_name
    } else {
        if let Some(repo_name) = pkg::config::get_default_repository().await? {
            repo_name
        } else {
            ffx_bail!(
                "Either a default repository must be set, or the --repository flag must be provided.\n\
                You can set a default repository using:\n\
                $ ffx repository default set <name>"
            )
        }
    };

    match repos
        .deregister_target(&repo_name, target_str.as_deref())
        .await
        .context("communicating with daemon")?
        .map_err(RepositoryError::from)
    {
        Ok(()) => Ok(()),
        Err(err @ RepositoryError::TargetCommunicationFailure) => {
            ffx_bail!(
                "Error while deregistering repository: {}\n\
                Ensure that a target is running and connected with:\n\
                $ ffx target list",
                err,
            )
        }
        Err(RepositoryError::ServerNotRunning) => {
            ffx_bail!(
                "Failed to deregister repository: {:#}",
                pkg::config::determine_why_repository_server_is_not_running().await
            )
        }
        Err(err) => {
            ffx_bail!("Failed to deregister repository: {}", err)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl_fuchsia_developer_ffx::{RepositoryError, RepositoryRegistryRequest};
    use fuchsia_async as fasync;
    use futures::channel::oneshot::{channel, Receiver};

    const REPO_NAME: &str = "some-name";
    const TARGET_NAME: &str = "some-target";

    async fn setup_fake_server() -> (RepositoryRegistryProxy, Receiver<(String, Option<String>)>) {
        let (sender, receiver) = channel();
        let mut sender = Some(sender);
        let repos = fho::testing::fake_proxy(move |req| match req {
            RepositoryRegistryRequest::DeregisterTarget {
                repository_name,
                target_identifier,
                responder,
            } => {
                sender.take().unwrap().send((repository_name, target_identifier)).unwrap();
                responder.send(Ok(())).unwrap();
            }
            other => panic!("Unexpected request: {:?}", other),
        });
        (repos, receiver)
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_deregister() {
        let (repos, receiver) = setup_fake_server().await;

        deregister(
            Some(TARGET_NAME.to_string()),
            DeregisterCommand { repository: Some(REPO_NAME.to_string()) },
            repos,
        )
        .await
        .unwrap();
        let got = receiver.await.unwrap();
        assert_eq!(got, (REPO_NAME.to_string(), Some(TARGET_NAME.to_string()),));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_deregister_default_repository() {
        let _env = ffx_config::test_init().await.unwrap();

        let default_repo_name = "default-repo";
        pkg::config::set_default_repository(default_repo_name).await.unwrap();

        let (repos, receiver) = setup_fake_server().await;

        deregister(Some(TARGET_NAME.to_string()), DeregisterCommand { repository: None }, repos)
            .await
            .unwrap();
        let got = receiver.await.unwrap();
        assert_eq!(got, (default_repo_name.to_string(), Some(TARGET_NAME.to_string()),));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_deregister_returns_error() {
        let repos = fho::testing::fake_proxy(move |req| match req {
            RepositoryRegistryRequest::DeregisterTarget {
                repository_name: _,
                target_identifier: _,
                responder,
            } => {
                responder.send(Err(RepositoryError::TargetCommunicationFailure)).unwrap();
            }
            other => panic!("Unexpected request: {:?}", other),
        });

        assert!(deregister(
            Some(TARGET_NAME.to_string()),
            DeregisterCommand { repository: Some(REPO_NAME.to_string()) },
            repos,
        )
        .await
        .is_err());
    }
}
