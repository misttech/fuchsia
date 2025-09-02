// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::config::{ParseSshConfigError, SshConfig};
use anyhow::{Context as _, Result, anyhow};
use ffx_config::EnvironmentContext;
use netext::ScopedSocketAddr;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use thiserror::Error;

const SSH_PRIV: &str = "ssh.priv";
const SSH_CONTROLMASTER_MODE: &str = "ssh.controlmaster.mode";
const SSH_CONTROLMASTER_PATH: &str = "ssh.controlmaster.path";
const SSH_CONTROLMASTER_DIR: &str = "ssh.controlmaster.dir";
pub const KEEPALIVE_TIMEOUT_CONFIG: &str = "ssh.keepalive_timeout";

#[derive(Error, Debug, Hash, Clone, PartialEq, Eq)]
pub enum SshError {
    #[error("unknown ssh error: {0}")]
    Unknown(String),
    #[error("permission denied")]
    PermissionDenied,
    #[error("connection refused")]
    ConnectionRefused,
    #[error("unknown name or service")]
    UnknownNameOrService,
    #[error("timeout")]
    Timeout,
    #[error("key verification failure")]
    KeyVerificationFailure,
    #[error("no route to host")]
    NoRouteToHost,
    #[error("network unreachable")]
    NetworkUnreachable,
    #[error("invalid argument")]
    InvalidArgument,
    #[error("target not compatible")]
    TargetIncompatible,
    #[error("connection closed by remote host")]
    ConnectionClosedByRemoteHost,
}

impl From<String> for SshError {
    fn from(s: String) -> Self {
        if s.contains("Permission denied") {
            return Self::PermissionDenied;
        }
        if s.contains("Connection refused") {
            return Self::ConnectionRefused;
        }
        if s.contains("Name or service not known") {
            return Self::UnknownNameOrService;
        }
        if s.contains("Connection timed out") {
            return Self::Timeout;
        }
        if s.contains("Host key verification failed") {
            return Self::KeyVerificationFailure;
        }
        if s.contains("No route to host") {
            return Self::NoRouteToHost;
        }
        if s.contains("Network is unreachable") {
            return Self::NetworkUnreachable;
        }
        if s.contains("Invalid argument") {
            return Self::InvalidArgument;
        }
        if s.contains("not compatible") {
            return Self::TargetIncompatible;
        }
        if s.contains("Connection closed by remote host") {
            return Self::ConnectionClosedByRemoteHost;
        }
        return Self::Unknown(s);
    }
}

impl From<&str> for SshError {
    fn from(s: &str) -> Self {
        Self::from(s.to_owned())
    }
}

#[cfg(not(test))]
pub fn get_ssh_key_paths() -> Result<Vec<String>> {
    use anyhow::Context;
    ffx_config::query(SSH_PRIV)
        .get_file()
        .context("getting path to an ssh private key from ssh.priv")
}

pub fn get_ssh_key_paths_from_env(env: &EnvironmentContext) -> Result<Vec<String>> {
    env.query(SSH_PRIV)
        .get_file()
        .context("getting path to an ssh private key from ssh.priv from env context")
}

#[cfg(test)]
const TEST_SSH_KEY_PATH: &str = "ssh/ssh_key_in_test";
#[cfg(test)]
fn get_ssh_key_paths() -> Result<Vec<String>> {
    Ok(vec![TEST_SSH_KEY_PATH.to_string()])
}

fn apply_auth_sock(cmd: &mut Command) {
    const SSH_AUTH_SOCK: &str = "ssh.auth-sock";
    if let Ok(path) = ffx_config::get::<String, _>(SSH_AUTH_SOCK) {
        log::debug!("SSH_AUTH_SOCK retrieved via config: {}", path);
        cmd.env("SSH_AUTH_SOCK", path.as_str());
        if !std::fs::exists(path.as_str()).unwrap() {
            log::warn!("SSH_AUTH_SOCK file does not exist at: {}", path);
        }
    }
}

fn build_ssh_command_with_ssh_path(
    ssh_path: &str,
    addr: ScopedSocketAddr,
    command: Vec<&str>,
) -> Result<Command> {
    let mut config = SshConfig::new()?;
    build_ssh_command_with_ssh_config(ssh_path, addr, &mut config, command)
}

pub fn build_ssh_command_with_env(
    ssh_path: &str,
    addr: ScopedSocketAddr,
    env: &EnvironmentContext,
    command: Vec<&str>,
) -> Result<Command> {
    let mut ssh_config = SshConfig::new()?;
    build_ssh_command_with_ssh_config_and_env(ssh_path, addr, &mut ssh_config, command, Some(env))
}

pub fn build_ssh_command_with_ssh_config(
    ssh_path: &str,
    addr: ScopedSocketAddr,
    config: &mut SshConfig,
    command: Vec<&str>,
) -> Result<Command> {
    build_ssh_command_with_ssh_config_and_env(ssh_path, addr, config, command, None)
}

fn get_addr_port(addr: &ScopedSocketAddr) -> (String, String) {
    let mut addr_str = format!("{}", addr);
    let colon_port = addr_str.split_off(addr_str.rfind(':').expect("socket format includes port"));

    // Remove the enclosing [] used in IPv6 socketaddrs
    let addr_start = if addr_str.starts_with("[") { 1 } else { 0 };
    let addr_end = addr_str.len() - if addr_str.ends_with("]") { 1 } else { 0 };
    let addr_arg = addr_str[addr_start..addr_end].to_string();

    (addr_arg, colon_port[1..].to_string())
}

fn spawn_controlmaster(
    ssh_path: &str,
    controlmaster_dir: PathBuf,
    ssh_keys: &[String],
    addr: &ScopedSocketAddr,
) -> Result<PathBuf, SpawnControlMasterError> {
    // Okay we need to create the socket path, but unix sockets
    // have a limit of 109 characters, which an ipv6 address will eat the
    // vast majority of. Let's go ahead and hash it
    let mut hasher = DefaultHasher::new();
    addr.hash(&mut hasher);
    let hash_value = hasher.finish();
    log::info!("Checking ControlMaster for {}. Hash: {}", addr, hash_value);
    if !std::fs::exists(&controlmaster_dir)? {
        std::fs::DirBuilder::new()
            .mode(0o700) // Read write execute for owner ownly.
            .create(&controlmaster_dir)?;
    }
    let socket_path = controlmaster_dir.join(hash_value.to_string());
    if socket_path.to_string_lossy().len() > MAX_SOCKET_LEN {
        return Err(SpawnControlMasterError::SocketPathTooLong {
            path: socket_path,
            max_allowed: MAX_SOCKET_LEN,
        });
    }

    if std::fs::exists(&socket_path)? {
        log::debug!("ControlMaster for {} already exists at: {}", addr, socket_path.display());
        return Ok(socket_path);
    }

    let mut c = Command::new(ssh_path);
    // Enable ControlMaster
    c.arg("-M");
    // ControlMaster path
    c.arg("-S");
    c.arg(&socket_path);
    // Forks SSH into the background and prevents it from executing a remote command.
    // This keeps the ControlMaster connection open without an interactive shell.
    c.arg("-fN");
    // Keep the connection alive for 5 minutes max
    c.args(["-o", "ControlPersist=5m"]);
    // No config file
    c.args(["-F", "none"]);
    // Use our custom Ssh Config
    let cfg = SshConfig::new()?;
    c.args(cfg.to_args());

    // Add identity keys
    for key in ssh_keys {
        c.arg("-i").arg(key);
    }

    let (addr_arg, port_arg) = get_addr_port(addr);
    c.arg("-p").arg(port_arg);
    c.arg(addr_arg);

    log::debug!("Spawning ssh command for ControlMaster");
    c.stdout(std::process::Stdio::null());
    c.stderr(std::process::Stdio::piped());
    c.stdin(std::process::Stdio::null());
    let output = c.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let ssherr = SshError::from(stderr);
        return Err(SpawnControlMasterError::ControlMasterStartError(ssherr));
    }

    Ok(socket_path)
}

#[derive(Debug)]
pub enum ControlMasterMode {
    None,
    Explicit,
    Managed,
}

#[derive(Error, Debug)]
pub enum ParseControlMasterModeError {
    #[error("Invalid ssh ControlMaster mode: {}", mode)]
    InvalidMode { mode: String },
}

#[derive(Error, Debug)]
pub enum ManageSshControlMasterError {
    #[error("ssh ControlMaster directory was not specified")]
    ControlMasterDirNotSpecified,
    #[error("ssh ControlMaster path was not specified")]
    ControlMasterPathNotSpecified,
    #[error("Error parsing ssh ControlMaster mode")]
    ParseError(#[from] ParseControlMasterModeError),
    #[error("Error reading configuration")]
    ConfigError(#[from] ffx_config::ConfigError),
    #[error("Error spawning ssh")]
    SpawnError(#[from] SpawnControlMasterError),
}

const MAX_SOCKET_LEN: usize = 100;

#[derive(Error, Debug)]
pub enum SpawnControlMasterError {
    #[error("Error parsing ssh configuration")]
    ParseSshConfig(#[from] ParseSshConfigError),
    #[error("Socket path \"{path}\" is too long. Maximum is {max_allowed}")]
    SocketPathTooLong { path: PathBuf, max_allowed: usize },
    #[error("ssh ControlMaster failed to start.")]
    ControlMasterStartError(#[source] SshError),
    #[error("Error spawning ssh")]
    SpawnError(#[from] std::io::Error),
}

impl TryFrom<String> for ControlMasterMode {
    type Error = ParseControlMasterModeError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "explicit" => Ok(Self::Explicit),
            "managed" => Ok(Self::Managed),
            "none" => Ok(Self::None),
            other => Err(ParseControlMasterModeError::InvalidMode { mode: other.to_owned() }),
        }
    }
}

impl TryFrom<Option<String>> for ControlMasterMode {
    type Error = ParseControlMasterModeError;
    fn try_from(value: Option<String>) -> Result<Self, Self::Error> {
        match value {
            None => Ok(Self::None),
            Some(v) => ControlMasterMode::try_from(v),
        }
    }
}

fn get_controlmaster_path(
    env: Option<&EnvironmentContext>,
    ssh_path: &str,
    addr: &ScopedSocketAddr,
    ssh_keys: &Vec<String>,
) -> Result<Option<PathBuf>, ManageSshControlMasterError> {
    let Some(env) = env else {
        return Ok(None);
    };

    let controlmaster_mode_string: Option<String> = env.get(SSH_CONTROLMASTER_MODE)?;
    let controlmaster_mode = ControlMasterMode::try_from(controlmaster_mode_string)?;

    match controlmaster_mode {
        ControlMasterMode::None => Ok(None),
        ControlMasterMode::Explicit => {
            // We are told to get the value explicitly
            // Map this error
            let path: Option<String> = env.get(SSH_CONTROLMASTER_PATH)?;
            let Some(path) = path else {
                return Err(ManageSshControlMasterError::ControlMasterPathNotSpecified);
            };
            Ok(Some(PathBuf::from(path)))
        }
        ControlMasterMode::Managed => {
            let controlmaster_dir: Option<String> = env.get(SSH_CONTROLMASTER_DIR)?;
            let Some(controlmaster_dir) = controlmaster_dir else {
                return Err(ManageSshControlMasterError::ControlMasterDirNotSpecified);
            };
            let path = spawn_controlmaster(ssh_path, controlmaster_dir.into(), &ssh_keys, addr)?;
            Ok(Some(path))
        }
    }
}

/// Builds the ssh command using the specified ssh configuration and path to the ssh command.
fn build_ssh_command_with_ssh_config_and_env(
    ssh_path: &str,
    addr: ScopedSocketAddr,
    config: &mut SshConfig,
    command: Vec<&str>,
    env: Option<&EnvironmentContext>,
) -> Result<Command> {
    if ssh_path.is_empty() {
        return Err(anyhow!("missing SSH command"));
    }

    let keys =
        if let Some(env) = env { get_ssh_key_paths_from_env(env)? } else { get_ssh_key_paths()? };

    if let Some(env) = env {
        if let Some(keepalive_timeout) = env.query(KEEPALIVE_TIMEOUT_CONFIG).get::<Option<u64>>()? {
            config.set_server_alive_count_max(keepalive_timeout as u16)?
        }
    }

    // Okay there are two ways we can get here
    // if we have config value ssh.target.control_path, dont spawn one, just use it
    // if we have config value ssh.controlmaster_dir, check the contents of that dir for a
    // properly named socket, use that one. Otherwise create one and then use iter
    // if neither of those config values are set, just dont use a ControlMaster
    let controlmaster_path = get_controlmaster_path(env, ssh_path, &addr, &keys)?;

    let mut c = Command::new(ssh_path);
    apply_auth_sock(&mut c);
    c.args(["-F", "none"]);
    c.args(config.to_args());

    // And then we'll just use the contromaster part from above here if it exists
    if let Some(control_path) = controlmaster_path {
        c.arg("-S");
        c.arg(control_path);
    }

    for key in keys {
        c.arg("-i").arg(key);
    }

    match addr.addr() {
        SocketAddr::V4(_) => c.arg("-o").arg("AddressFamily=inet"),
        SocketAddr::V6(_) => c.arg("-o").arg("AddressFamily=inet6"),
    };

    if env.map(|c| ffx_config::logging::debugging_on(c)).unwrap_or(false) {
        c.arg("-vv");
    }

    let (addr_arg, port_arg) = get_addr_port(&addr);
    c.arg("-p").arg(port_arg);
    c.arg(addr_arg);

    c.args(&command);

    return Ok(c);
}

/// Build the ssh command using the default ssh command and configuration.
pub fn build_ssh_command(addr: ScopedSocketAddr, command: Vec<&str>) -> Result<Command> {
    build_ssh_command_with_ssh_path("ssh", addr, command)
}

/// Build the ssh command using a provided sshconfig file.
pub fn build_ssh_command_with_config_file(
    config_file: &PathBuf,
    addr: ScopedSocketAddr,
    command: Vec<&str>,
) -> Result<Command> {
    let keys = get_ssh_key_paths()?;

    let mut c = Command::new("ssh");
    apply_auth_sock(&mut c);
    c.arg("-F").arg(config_file);

    for k in keys {
        c.arg("-i").arg(k);
    }

    let mut addr_str = format!("{}", addr);
    let colon_port = addr_str.split_off(addr_str.rfind(':').expect("socket format includes port"));

    // Remove the enclosing [] used in IPv6 socketaddrs
    let addr_start = if addr_str.starts_with("[") { 1 } else { 0 };
    let addr_end = addr_str.len() - if addr_str.ends_with("]") { 1 } else { 0 };
    let addr_arg = &addr_str[addr_start..addr_end];

    c.arg("-p").arg(&colon_port[1..]);
    c.arg(addr_arg);

    c.args(&command);

    return Ok(c);
}

#[cfg(test)]
mod test {
    use super::*;
    use ffx_config::ConfigLevel;
    use pretty_assertions::assert_eq;
    use std::io::BufRead;

    #[fuchsia::test]
    async fn test_build_ssh_command_ipv4() {
        let config = SshConfig::new().expect("default ssh config");
        let addr: SocketAddr = "192.168.0.1:22".parse().unwrap();

        let result =
            build_ssh_command(ScopedSocketAddr::from_socket_addr(addr).unwrap(), vec!["ls"])
                .unwrap();
        let actual_args: Vec<_> = result.get_args().map(|a| a.to_string_lossy()).collect();
        let mut expected_args: Vec<String> = vec!["-F".into(), "none".into()];
        expected_args.extend(config.to_args());
        expected_args.extend(
            ["-i", TEST_SSH_KEY_PATH, "-o", "AddressFamily=inet", "-p", "22", "192.168.0.1", "ls"]
                .map(String::from),
        );
        assert_eq!(actual_args, expected_args);
    }

    #[fuchsia::test]
    async fn test_build_ssh_command_ipv6() {
        let config = SshConfig::new().expect("default ssh config");
        let addr: SocketAddr = "[fe80::12%1]:8022".parse().unwrap();
        // This presumes the host device running the test is linux and has a `lo` loopback device.
        let result =
            build_ssh_command(ScopedSocketAddr::from_socket_addr(addr).unwrap(), vec!["ls"])
                .unwrap();
        let actual_args: Vec<_> = result.get_args().map(|a| a.to_string_lossy()).collect();
        let mut expected_args: Vec<String> = vec!["-F".into(), "none".into()];
        expected_args.extend(config.to_args());
        expected_args.extend(
            [
                "-i",
                TEST_SSH_KEY_PATH,
                "-o",
                "AddressFamily=inet6",
                "-p",
                "8022",
                "fe80::12%lo",
                "ls",
            ]
            .map(String::from),
        );
        assert_eq!(actual_args, expected_args);
    }

    #[fuchsia::test]
    async fn test_apply_auth_sock() {
        let env = ffx_config::test_init().await.unwrap();
        let expect_path =
            env.isolate_root.path().join("ssh-auth.sock").to_string_lossy().to_string();
        env.context
            .query("ssh.auth-sock")
            .level(Some(ConfigLevel::User))
            .set(expect_path.clone().into())
            .expect("setting auth sock config");

        let mut cmd = Command::new("env");
        apply_auth_sock(&mut cmd);
        let lines =
            cmd.output().unwrap().stdout.lines().filter_map(|res| res.ok()).collect::<Vec<_>>();

        let expected_var = format!("SSH_AUTH_SOCK={}", expect_path);
        assert!(
            lines.iter().any(|line| line.starts_with(&expected_var)),
            "Looking for {} in {}",
            expected_var,
            lines.join("\n")
        );
    }

    #[fuchsia::test]
    async fn test_build_ssh_command_with_ssh_config() {
        let mut config = SshConfig::new().expect("default ssh config");
        let addr: SocketAddr = "[fe80::12]:8022".parse().unwrap();

        // Override some options
        config.set("LogLevel", "DEBUG3").expect("setting loglevel");

        let result = build_ssh_command_with_ssh_config(
            "ssh",
            ScopedSocketAddr::from_socket_addr(addr).unwrap(),
            &mut config,
            vec!["ls"],
        )
        .unwrap();
        let actual_args: Vec<_> =
            result.get_args().map(|a| a.to_string_lossy().to_string()).collect();

        // Check the default
        assert_eq!(config.get("CheckHostIP").expect("CheckHostIP value").unwrap(), "no");
        assert!(actual_args.contains(&"CheckHostIP=no".to_string()));

        // Check the override
        assert!(actual_args.contains(&"LogLevel=DEBUG3".to_string()));
    }

    #[fuchsia::test]
    fn test_host_pipe_err_from_str() {
        assert_eq!(SshError::from("Permission denied"), SshError::PermissionDenied);
        assert_eq!(SshError::from("Connection refused"), SshError::ConnectionRefused);
        assert_eq!(SshError::from("Name or service not known"), SshError::UnknownNameOrService);
        assert_eq!(SshError::from("Connection timed out"), SshError::Timeout);
        assert_eq!(
            SshError::from("Host key verification failedddddd"),
            SshError::KeyVerificationFailure
        );
        assert_eq!(SshError::from("There is No route to host"), SshError::NoRouteToHost);
        assert_eq!(SshError::from("The Network is unreachable"), SshError::NetworkUnreachable);
        assert_eq!(SshError::from("Invalid argument"), SshError::InvalidArgument);
        assert_eq!(SshError::from("ABI 123 is not compatible"), SshError::TargetIncompatible);

        let unknown_str = "OIHWOFIHOIWHFW";
        assert_eq!(SshError::from(unknown_str), SshError::Unknown(String::from(unknown_str)));
    }

    #[test]
    fn test_get_addr_port_ipv4() {
        let addr: SocketAddr = "192.168.0.1:8022".parse().unwrap();
        let scoped_addr = ScopedSocketAddr::from_socket_addr(addr).unwrap();
        let (addr, port) = get_addr_port(&scoped_addr);
        assert_eq!(addr, "192.168.0.1");
        assert_eq!(port, "8022");
    }

    #[test]
    fn test_get_addr_port_ipv6() {
        let addr: SocketAddr = "[fe80::12%1]:8022".parse().unwrap();
        let scoped_addr = ScopedSocketAddr::from_socket_addr(addr).unwrap();
        let (addr, port) = get_addr_port(&scoped_addr);
        assert_eq!(addr, "fe80::12%lo");
        assert_eq!(port, "8022");
    }

    #[test]
    fn test_get_addr_port_ipv6_no_scope() {
        let addr: SocketAddr = "[fe80::12]:22".parse().unwrap();
        let scoped_addr = ScopedSocketAddr::from_socket_addr(addr).unwrap();
        let (addr, port) = get_addr_port(&scoped_addr);
        assert_eq!(addr, "fe80::12");
        assert_eq!(port, "22");
    }

    #[test]
    fn test_controlmaster_mode_try_from() {
        assert!(matches!(
            ControlMasterMode::try_from("explicit".to_string()).unwrap(),
            ControlMasterMode::Explicit
        ));
        assert!(matches!(
            ControlMasterMode::try_from("managed".to_string()).unwrap(),
            ControlMasterMode::Managed
        ));
        assert!(matches!(
            ControlMasterMode::try_from("none".to_string()).unwrap(),
            ControlMasterMode::None
        ));

        let result = ControlMasterMode::try_from("invalid-mode".to_string());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(format!("{}", err), "Invalid ssh ControlMaster mode: invalid-mode");
    }

    #[fuchsia::test]
    async fn test_get_controlmaster_path_mode_none() {
        let env = ffx_config::test_init().await.unwrap();
        env.context
            .query("ssh.controlmaster.mode")
            .level(Some(ConfigLevel::User))
            .set("none".into())
            .unwrap();

        let addr: SocketAddr = "127.0.0.1:22".parse().unwrap();
        let scoped_addr = ScopedSocketAddr::from_socket_addr(addr).unwrap();
        let ssh_keys = vec!["key".to_string()];

        let res =
            get_controlmaster_path(Some(&env.context), "ssh", &scoped_addr, &ssh_keys).unwrap();
        assert_eq!(res, None);
    }

    #[fuchsia::test]
    async fn test_get_controlmaster_path_mode_explicit() {
        let env = ffx_config::test_init().await.unwrap();
        env.context
            .query("ssh.controlmaster.mode")
            .level(Some(ConfigLevel::User))
            .set("explicit".into())
            .unwrap();
        let expected_path = "/tmp/test_socket";
        env.context
            .query("ssh.controlmaster.path")
            .level(Some(ConfigLevel::User))
            .set(expected_path.into())
            .unwrap();

        let addr: SocketAddr = "127.0.0.1:22".parse().unwrap();
        let scoped_addr = ScopedSocketAddr::from_socket_addr(addr).unwrap();
        let ssh_keys = vec!["key".to_string()];

        let res =
            get_controlmaster_path(Some(&env.context), "ssh", &scoped_addr, &ssh_keys).unwrap();
        assert_eq!(res, Some(PathBuf::from(expected_path)));
    }

    #[fuchsia::test]
    async fn test_get_controlmaster_path_mode_explicit_no_path() {
        let env = ffx_config::test_init().await.unwrap();
        env.context
            .query("ssh.controlmaster.mode")
            .level(Some(ConfigLevel::User))
            .set("explicit".into())
            .unwrap();

        let addr: SocketAddr = "127.0.0.1:22".parse().unwrap();
        let scoped_addr = ScopedSocketAddr::from_socket_addr(addr).unwrap();
        let ssh_keys = vec!["key".to_string()];

        let res = get_controlmaster_path(Some(&env.context), "ssh", &scoped_addr, &ssh_keys);
        assert!(matches!(res, Err(ManageSshControlMasterError::ControlMasterPathNotSpecified)));
    }

    #[fuchsia::test]
    async fn test_get_controlmaster_path_mode_managed_no_dir() {
        let env = ffx_config::test_init().await.unwrap();
        env.context
            .query("ssh.controlmaster.mode")
            .level(Some(ConfigLevel::User))
            .set("managed".into())
            .unwrap();

        let addr: SocketAddr = "127.0.0.1:22".parse().unwrap();
        let scoped_addr = ScopedSocketAddr::from_socket_addr(addr).unwrap();
        let ssh_keys = vec!["key".to_string()];

        let res = get_controlmaster_path(Some(&env.context), "ssh", &scoped_addr, &ssh_keys);
        assert!(matches!(res, Err(ManageSshControlMasterError::ControlMasterDirNotSpecified)));
    }

    #[fuchsia::test]
    async fn test_get_controlmaster_path_socket_path_too_long() {
        let env = ffx_config::test_init().await.unwrap();
        env.context
            .query("ssh.controlmaster.mode")
            .level(Some(ConfigLevel::User))
            .set("managed".into())
            .unwrap();

        let long_dir = "a".repeat(MAX_SOCKET_LEN);
        env.context
            .query("ssh.controlmaster.dir")
            .level(Some(ConfigLevel::User))
            .set(long_dir.into())
            .unwrap();

        let addr: SocketAddr = "127.0.0.1:22".parse().unwrap();
        let scoped_addr = ScopedSocketAddr::from_socket_addr(addr).unwrap();
        let ssh_keys = vec!["key".to_string()];

        let res = get_controlmaster_path(Some(&env.context), "ssh", &scoped_addr, &ssh_keys);
        assert!(matches!(
            res,
            Err(ManageSshControlMasterError::SpawnError(
                SpawnControlMasterError::SocketPathTooLong { .. }
            ))
        ));
    }
}
