// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::path::PathBuf;
use std::process::Command;

#[must_use = "The guard must be kept alive to keep the package server running"]
pub struct ServerGuard {
    pub ffx_path: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        eprintln!("Stopping ephemeral package server...");
        let _ = Command::new(&self.ffx_path).arg("repository").arg("server").arg("stop").output();
    }
}

/// Trait for running package server commands.
/// Mockable for tests.
pub trait PackageServerRunner {
    fn check_for_running_server(&self) -> anyhow::Result<bool>;
    fn run_package_server(&self, build_dir: Option<&str>) -> anyhow::Result<Option<ServerGuard>>;
}

pub struct DefaultPackageServerRunner;

impl PackageServerRunner for DefaultPackageServerRunner {
    fn check_for_running_server(&self) -> anyhow::Result<bool> {
        let ffx_path = std::env::current_exe()?;
        let output = Command::new(&ffx_path).args(&["repository", "server", "list"]).output()?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(!stdout.trim().is_empty())
        } else {
            Ok(false)
        }
    }

    fn run_package_server(&self, build_dir: Option<&str>) -> anyhow::Result<Option<ServerGuard>> {
        let ffx_path = std::env::current_exe()?;
        let build_dir_owned = build_dir.map(|s| s.to_string());
        let args = get_server_start_args(build_dir_owned.as_deref());

        let output = Command::new(&ffx_path).args(&args).output()?;

        if output.status.success() {
            Ok(Some(ServerGuard { ffx_path }))
        } else {
            eprintln!(
                "Warning: Failed to start package server: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(None)
        }
    }
}

fn get_server_start_args(build_dir: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "repository".to_string(),
        "server".to_string(),
        "start".to_string(),
        "--background".to_string(),
        "--alias".to_string(),
        "fuchsia.com".to_string(),
    ];

    if let Some(bd) = build_dir {
        args.push("--auto-publish".to_string());
        args.push(format!("{}/all_package_manifests.list", bd.trim()));
    }

    args
}

/// Starts the package server if it's not already running.
/// Returns a guard that stops the server when dropped.
pub fn maybe_start_server<R: PackageServerRunner>(
    runner: &R,
    build_dir: Option<&std::path::Path>,
) -> anyhow::Result<Option<ServerGuard>> {
    if !runner.check_for_running_server()? {
        eprintln!("No package server running. Starting an ephemeral one...");

        if let Some(dir) = build_dir {
            let manifest_path = dir.join("all_package_manifests.list");
            if !manifest_path.exists() {
                anyhow::bail!("Could not find package manifest at: {}", manifest_path.display());
            }
        }

        let build_dir_str = build_dir.map(|p| p.to_string_lossy());
        runner.run_package_server(build_dir_str.as_deref())
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockPackageServerRunner {
        server_running: bool,
        start_success: bool,
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl PackageServerRunner for MockPackageServerRunner {
        fn check_for_running_server(&self) -> anyhow::Result<bool> {
            self.calls.borrow_mut().push("check".to_string());
            Ok(self.server_running)
        }

        fn run_package_server(
            &self,
            _build_dir: Option<&str>,
        ) -> anyhow::Result<Option<ServerGuard>> {
            self.calls.borrow_mut().push("run".to_string());
            if self.start_success {
                let ffx_path = std::env::current_exe()?;
                Ok(Some(ServerGuard { ffx_path }))
            } else {
                Ok(None)
            }
        }
    }

    #[test]
    fn test_get_server_start_args_no_build_dir() {
        let args = get_server_start_args(None);
        assert_eq!(
            args,
            vec!["repository", "server", "start", "--background", "--alias", "fuchsia.com"]
        );
    }

    #[test]
    fn test_get_server_start_args_with_build_dir() {
        let args = get_server_start_args(Some("out/default\n"));
        assert_eq!(
            args,
            vec![
                "repository",
                "server",
                "start",
                "--background",
                "--alias",
                "fuchsia.com",
                "--auto-publish",
                "out/default/all_package_manifests.list"
            ]
        );
    }

    #[test]
    fn test_maybe_start_server_already_running() {
        let runner = MockPackageServerRunner {
            server_running: true,
            start_success: false,
            calls: std::cell::RefCell::new(Vec::new()),
        };

        let result = maybe_start_server(&runner, None).unwrap();
        assert!(result.is_none());
        assert_eq!(runner.calls.borrow().len(), 1);
        assert_eq!(runner.calls.borrow()[0], "check");
    }

    #[test]
    fn test_maybe_start_server_not_running_success() {
        let runner = MockPackageServerRunner {
            server_running: false,
            start_success: true,
            calls: std::cell::RefCell::new(Vec::new()),
        };

        let result = maybe_start_server(&runner, None).unwrap();
        assert!(result.is_some());
        assert_eq!(runner.calls.borrow().len(), 2);
        assert_eq!(runner.calls.borrow()[0], "check");
        assert_eq!(runner.calls.borrow()[1], "run");
    }
}
