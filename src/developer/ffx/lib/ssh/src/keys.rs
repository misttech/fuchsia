// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base64::display::Base64Display;
use base64::prelude::{BASE64_STANDARD, Engine as _};
use ffx_config::EnvironmentContext;
use ffx_config::api::ConfigError;
use fho::FfxContext;
use fuchsia_async::Task;
use hyper::body::Buf;
use ring::rand::{self, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Debug, Display};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::{env, fmt, str};
use tokio::net::TcpStream as AsyncTcpStream;

// This is the port that, on the device, service the authorized_keys file.
const AUTHORIZED_KEYS_HTTP_PORT: u16 = 9797;

fn auth_keys_filter_map(source: SshKeySource, line: &str) -> Option<SshKey> {
    let not_comments = !line.is_empty() && !line.starts_with('#');
    let parts: Vec<&str> = line.split_whitespace().collect();
    let sufficient_parts = parts.len() >= 2;

    if not_comments && sufficient_parts {
        let key_type = parts[0].to_string();
        let key = parts[1].to_string();
        let comment = if parts.len() > 2 { Some(parts[2..].join(" ")) } else { None };
        let mut sources = HashSet::new();
        sources.insert(source);
        Some(SshKey { key_type, key, comment, sources })
    } else {
        None
    }
}

async fn query_device_authorized_keys(
    mut addr: std::net::SocketAddr,
    port: u16,
) -> fho::Result<HashSet<SshKey>> {
    addr.set_port(port);
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .with_user_message(|| format!("Unable to connect to {addr:?}"))?;
    stream.set_nonblocking(true).bug()?;
    let stream = AsyncTcpStream::from_std(stream).bug()?;
    let (mut request_sender, connection) = hyper::client::conn::handshake(stream)
        .await
        .map_err(|e| fho::user_error!("Failed to initiate http handshake with device: {e:?}"))?;
    let _conn_task = Task::local(connection);
    let mut response = request_sender
        .send_request(
            hyper::Request::builder()
                .header("Host", format!("{addr}"))
                .method("GET")
                .body(hyper::Body::from(""))
                .bug()?,
        )
        .await
        .map_err(|e| fho::user_error!("Failed to send HTTP request to device at {addr}: {e:?}"))?;
    if response.status() != hyper::StatusCode::OK {
        fho::return_user_error!("Received an HTTP error from the device: {response:?}");
    }
    let body_bytes = hyper::body::to_bytes(response.body_mut())
        .await
        .user_message("failed to read http body")?;
    let body_str = String::from_utf8_lossy(body_bytes.chunk());
    if body_str.is_empty() {
        fho::return_user_error!(
            "Received empty string for authorized_keys file. You may need to reflash the device."
        );
    }
    Ok(body_str
        .lines()
        .filter_map(|line| auth_keys_filter_map(SshKeySource::Device, line))
        .collect())
}

/// Attempts to download ssh keys from the remote address via HTTP. Forces usage of port 9797.
///
/// This will return a list of SSH keys (if `Okay(_)` it is guaranteed to be non-empty).
/// If no keys are found that match the authorized_keys on the fuchsia device, an error will be
/// returned detailing the directories inspected, and what public keys were read.
// This doesn't use the target info holder to prevent a circular dependency. Furthermore we're
// trying to avoid using the FIDL structures too much in internal code.
pub async fn find_matching_ssh_keys(addr: std::net::SocketAddr) -> fho::Result<HashSet<SshKey>> {
    let local_ssh_dirs = local_ssh_key_dirs()?;
    let local_keys = get_ssh_public_keys(&local_ssh_dirs)?;
    find_matching_ssh_keys_impl(local_ssh_dirs, local_keys, addr, AUTHORIZED_KEYS_HTTP_PORT).await
}

// Helper method to make testing easier.
async fn find_matching_ssh_keys_impl(
    local_ssh_dirs: Vec<PathBuf>,
    mut local_keys: HashSet<SshKey>,
    addr: std::net::SocketAddr,
    port: u16,
) -> fho::Result<HashSet<SshKey>> {
    let device_authorized_keys = query_device_authorized_keys(addr, port).await?;
    let mut found_keys = HashSet::new();
    for device_key in device_authorized_keys.iter() {
        // We're using `take` here because `local_keys` contains the file sources, which
        // is for guiding the user (to show that there are multiple source from whence the
        // ssh keys came).
        if let Some(k) = local_keys.take(&device_key) {
            found_keys.insert(k);
        }
    }

    // The majority of the logic here is just making something displayable for the user to read in
    // the full error formatting.
    if found_keys.is_empty() {
        let device_keys = device_authorized_keys
            .iter()
            .map(|key| format!("{key}"))
            .collect::<Vec<_>>()
            .join("\n\t-- ");
        let ssh_agent_info = ssh_agent_keys_message(&local_keys);
        let non_agent_keys_msg = local_non_agent_keys_message(&local_ssh_dirs, &local_keys);
        fho::return_user_error!(
            "None of the following device SSH public keys matched any local ssh keys:\n\t-- {}\n\n{}\n\n{}

You may need to reflash the device or reconfigure your SSH agent. Please consult
https://fuchsia.dev/fuchsia-src/development/tools/ffx/workflows/create-ssh-keys-for-devices
for more details",
            device_keys,
            ssh_agent_info,
            non_agent_keys_msg,
        );
    }
    Ok(found_keys)
}

// Turns the searched directories into an info message for when we didn't find any ssh public keys
// that matched locally.
fn local_non_agent_keys_message(
    searched_dirs: &Vec<PathBuf>,
    local_keys: &HashSet<SshKey>,
) -> String {
    let non_agent_keys = local_keys
        .iter()
        .filter_map(|k| {
            if k.sources.is_empty() {
                None
            } else {
                let mut k = k.clone();
                let dirs = std::mem::replace(&mut k.sources, Default::default());
                Some(format!("{k}\n\t\t-- Found in {dirs:?}"))
            }
        })
        .collect::<Vec<_>>()
        .join("\n\t-- ");

    if non_agent_keys.is_empty() {
        format!(
            "No local ssh keys were found. We looked in the following locations:\n\t-- {}",
            searched_dirs
                .iter()
                .map(|d| format!("{}", d.display()))
                .collect::<Vec<_>>()
                .join("\n\t-- ")
        )
    } else {
        format!(
            "When searching local directories, we found the following public keys:\n\t-- {}",
            non_agent_keys
        )
    }
}

// Turns the ssh agent keys into an info message showing all the ones found (or if none were found)
// in the event that we didn't find any matching ssh public keys.
fn ssh_agent_keys_message(local_keys: &HashSet<SshKey>) -> String {
    let ssh_agent_keys = local_keys
        .iter()
        .filter_map(|k| if k.sources.is_empty() { Some(format!("{k}")) } else { None })
        .collect::<Vec<_>>()
        .join("\n\t-- ");
    if ssh_agent_keys.is_empty() {
        format!("No public keys were found from the ssh-agent")
    } else {
        format!("When querying the ssh-agent we found the following keys:\n\t-- {ssh_agent_keys}")
    }
}

fn fuchsia_ssh_key_dir() -> Option<PathBuf> {
    Some(PathBuf::from(env::var("FUCHSIA_DIR").ok()?).join(".ssh"))
}

fn local_ssh_key_dirs() -> fho::Result<Vec<PathBuf>> {
    let mut dirs = vec![
        // Regular SSH directory on *nix systems.
        PathBuf::from(env::var("HOME").user_message("Could not find home directory")?).join(".ssh"),
    ];
    if let Some(fuchsia_dir) = fuchsia_ssh_key_dir() {
        dirs.push(fuchsia_dir)
    }
    Ok(dirs)
}

fn find_ssh_keys_in_dirs(dirs: &Vec<PathBuf>) -> fho::Result<SshKeySet> {
    let mut keys = SshKeySet::new();
    for dir in dirs {
        for pub_key_file in fs::read_dir(&dir)
            .with_user_message(|| format!("reading {}", dir.display()))?
            .into_iter()
            .filter_map(|entry_res| entry_res.ok())
            .map(|e| e.path())
            .filter(|path| {
                path.is_file()
                    && path.extension().and_then(|s| s.to_str()).map_or(false, |ext| ext == "pub")
            })
        {
            let content = fs::read_to_string(&pub_key_file)
                .with_user_message(|| format!("reading {}", pub_key_file.display()))?;
            content
                .lines()
                .filter_map(|line| {
                    auth_keys_filter_map(SshKeySource::File(pub_key_file.clone()), line)
                })
                .for_each(|k| {
                    keys.insert(k);
                });
        }
    }
    Ok(keys)
}

fn get_ssh_agent_identities() -> SshKeySet {
    let mut keys = SshKeySet::new();
    match Command::new("ssh-add").arg("-L").output() {
        Ok(res) => {
            if res.status.success() {
                let stdout_str = String::from_utf8_lossy(&res.stdout);
                stdout_str
                    .lines()
                    .filter_map(|line| auth_keys_filter_map(SshKeySource::SshAgent, line))
                    .for_each(|k| {
                        keys.insert(k);
                    });
            }
        }
        Err(_e) => {}
    }
    keys
}

/// Tries to find all public keys from both the ssh-agent and from local directories.
/// an `Ok(_)` result is guaranteed to be non-empty. Not finding any SSH keys will result in
/// returning an error.
fn get_ssh_public_keys(dirs: &Vec<PathBuf>) -> fho::Result<HashSet<SshKey>> {
    let local_keys = find_ssh_keys_in_dirs(dirs)?;
    let agent_keys = get_ssh_agent_identities();
    let local_keys = local_keys.union(agent_keys);
    let local_keys = local_keys.into_hashset();
    if local_keys.is_empty() {
        fho::return_user_error!(
            "Unable to locate local SSH keys from either the ssh agent or any of the following directories:\n\t-- {}",
            dirs.iter().map(|d| format!("{}", d.display())).collect::<Vec<_>>().join("\n\t-- ")
        );
    }
    Ok(local_keys)
}

/// A structure for tracking SSH keys. This makes sure that as keys are added, their overall paths
/// are tracked. After inserting all keys, this can be converted into a HashSet where each key will
/// contain all the directories in which the key can be found.
struct SshKeySet {
    inner: HashMap<SshKey, HashSet<SshKeySource>>,
}

impl SshKeySet {
    fn new() -> Self {
        Self { inner: Default::default() }
    }

    fn insert(&mut self, mut key: SshKey) {
        let sources = std::mem::replace(&mut key.sources, Default::default());
        self.inner
            .entry(key)
            .and_modify(|d| {
                d.extend(sources.clone());
            })
            .or_insert(sources);
    }

    fn into_hashset(self) -> HashSet<SshKey> {
        let res = self
            .inner
            .into_iter()
            .map(|(mut k, v)| {
                k.sources.extend(v);
                k
            })
            .collect();
        res
    }

    fn union(mut self, other: Self) -> Self {
        for (mut key, dirs) in other.inner.into_iter() {
            key.sources.extend(dirs);
            self.insert(key)
        }
        self
    }
}

#[derive(serde::Serialize, Eq, PartialEq, Clone, Hash)]
pub enum SshKeySource {
    File(PathBuf),
    SshAgent,
    Device,
}

impl Display for SshKeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let display = match self {
            Self::File(pb) => format!("{pb:?}"),
            Self::SshAgent => "agent".to_owned(),
            Self::Device => "device".to_owned(),
        };
        write!(f, "{display}")
    }
}

impl Debug for SshKeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        // Just use Display impl.
        write!(f, "{self}")
    }
}

#[derive(serde::Serialize, Eq, Clone, Debug)]
pub struct SshKey {
    pub key_type: String,
    pub key: String,
    pub comment: Option<String>,
    pub sources: HashSet<SshKeySource>,
}

impl Hash for SshKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Comment/source is not relevant for hashing.
        self.key_type.hash(state);
        self.key.hash(state);
    }
}

impl PartialEq for SshKey {
    fn eq(&self, other: &Self) -> bool {
        self.key_type == other.key_type && self.key == other.key
    }
}

impl fmt::Display for SshKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "key type: {}, key: {}{}",
            self.key_type,
            self.key,
            match self.sources.len() {
                0_usize => format!(""),
                1_usize => {
                    format!(", found in: {:?}", self.sources.iter().next().unwrap())
                }
                2_usize.. => {
                    format!(", found in: {:?}", self.sources)
                }
            }
        )
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub enum SshKeyErrorKind {
    BadKeyType,
    BadBase64Encoding,
    BadConfiguration,
    BadFilePermission,
    BadKeyFormat,
    BadUTFEncoding,
    GenerationError,
    KeyAlreadyExists,
    IOError,
    KeyMismatch,
    FileNotFound,
}

impl fmt::Display for SshKeyErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone)]
struct SshKeyInternalError {
    pub kind: SshKeyErrorKind,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct SshKeyError {
    pub kind: SshKeyErrorKind,
    pub message: String,
}
impl fmt::Display for SshKeyInternalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.message)
    }
}
impl fmt::Display for SshKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.message)
    }
}

impl Error for SshKeyError {}

impl From<std::io::Error> for SshKeyInternalError {
    fn from(value: std::io::Error) -> Self {
        match value.kind() {
            std::io::ErrorKind::AlreadyExists => SshKeyInternalError {
                kind: SshKeyErrorKind::KeyAlreadyExists,
                message: format!("{value:?}"),
            },
            _ => SshKeyInternalError {
                kind: SshKeyErrorKind::IOError,
                message: format!("{value:?}"),
            },
        }
    }
}

impl From<str::Utf8Error> for SshKeyInternalError {
    fn from(value: str::Utf8Error) -> Self {
        SshKeyInternalError { kind: SshKeyErrorKind::BadUTFEncoding, message: format!("{value:?}") }
    }
}

impl From<base64::DecodeError> for SshKeyInternalError {
    fn from(value: base64::DecodeError) -> Self {
        SshKeyInternalError {
            kind: SshKeyErrorKind::BadBase64Encoding,
            message: format!("{value:?}"),
        }
    }
}

impl From<ConfigError> for SshKeyError {
    fn from(value: ConfigError) -> Self {
        SshKeyError { kind: SshKeyErrorKind::BadConfiguration, message: format!("{value:?}") }
    }
}

impl From<SshKeyInternalError> for SshKeyError {
    fn from(value: SshKeyInternalError) -> Self {
        let kind = match value.kind {
            SshKeyErrorKind::BadBase64Encoding | SshKeyErrorKind::BadUTFEncoding => {
                SshKeyErrorKind::BadKeyFormat
            }
            _ => value.kind,
        };
        SshKeyError { kind: kind, message: value.message }
    }
}

/// Paths to the private and public SSH keys that are used by ffx.
/// These can be loaded from the configuration keys `ssh.pub`
/// and `ssh.priv` by using SshKeyFiles::load().
/// Typical usage is to load from the configuration and then create the keys
/// if they are missing when initializing a device via flashing/paving
///  or starting an emulator instance.
///
/// let ssh_keys = SshKeyFiles::load().await?;
/// ssh_keys.create_keys_if_needed()?;
///
/// This is preferred since generating the private key when attempting to access
/// a device already initialized is pointless.
///
///
#[derive(Debug, Default)]
pub struct SshKeyFiles {
    pub authorized_keys: PathBuf,
    pub private_key: PathBuf,
}

const KEYTYPE_STR: &str = "ssh-ed25519";
const KEYTYPE: &[u8] = b"ssh-ed25519";
const COMMENT: &str = "Generated by ffx for Fuchsia";
const AUTH_MAGIC: &[u8] = b"openssh-key-v1\0";

impl SshKeyFiles {
    /// loads the file paths from the config properties `ssh.pub` and `ssh.priv`.
    /// If none of the paths configured are to files that exist, the paths will
    ///  be set to the default sources, which is the first element in the config settings.
    pub async fn load(ctx: &EnvironmentContext) -> Result<Self, SshKeyError> {
        // initialize to the first path in the list, then iterate through the list to select
        // the first file that exists.
        let authorized_keys_files: Vec<PathBuf> = ctx.query("ssh.pub").build().get(ctx)?;
        if authorized_keys_files.is_empty() {
            return Err(SshKeyError {
                kind: SshKeyErrorKind::BadConfiguration,
                message: "No paths configured for `ssh.pub`.".into(),
            });
        }
        let mut authorized_keys = authorized_keys_files[0].to_path_buf();
        for path in &authorized_keys_files {
            if path.exists() {
                authorized_keys = path.to_path_buf();
                break;
            }
        }

        let key_files: Vec<PathBuf> = ctx.query("ssh.priv").build().get(ctx)?;
        if key_files.is_empty() {
            return Err(SshKeyError {
                kind: SshKeyErrorKind::BadConfiguration,
                message: format!("No paths configured for `ssh.priv`."),
            });
        }
        let mut private_key = key_files[0].to_path_buf();
        for path in &key_files {
            if path.exists() {
                private_key = path.to_path_buf();
                break;
            }
        }
        Ok(SshKeyFiles { authorized_keys, private_key })
    }

    /// Generates the ed25519 key pair and saves openssh authorized_keys and private key file,
    /// if the paths point to files that do not exist.
    /// if append is true, the public key matching self.private key is appended to
    /// the self.authorized_keys file. There is no check for duplication, so
    /// append should only be true if check_keys returns KeyMismatch.
    pub fn create_keys_if_needed(&self, append: bool) -> Result<(), SshKeyError> {
        let mut public_key: Vec<u8> = vec![];
        let mut do_write_public_key = append;

        // Validate the paths are non-empty
        if self.private_key.display().to_string().is_empty() {
            return Err(SshKeyError {
                kind: SshKeyErrorKind::BadConfiguration,
                message: "private key path cannot be empty".into(),
            });
        }
        if self.authorized_keys.display().to_string().is_empty() {
            return Err(SshKeyError {
                kind: SshKeyErrorKind::BadConfiguration,
                message: "authorized keys path cannot be empty".into(),
            });
        }

        if !self.private_key.exists() {
            // There is no private key, so generate a new key pair
            // resulting in a byte array .der encoded.
            log::info!("Creating SSH key pair: {}", self.private_key.display());
            let rng = SystemRandom::new();
            let bytes = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|m| SshKeyError {
                kind: SshKeyErrorKind::GenerationError,
                message: format!("could not generate pkcs8 document: {:?}", m),
            })?;
            let key_pair = Ed25519KeyPair::from_pkcs8(bytes.as_ref()).map_err(|m| SshKeyError {
                kind: SshKeyErrorKind::GenerationError,
                message: format!("could not get keypair from pkcs8 document: {:?}", m),
            })?;

            // If somehow between the check and now the key is created, return early
            match write_private_key(&self.private_key, &key_pair, bytes.as_ref(), &rng) {
                Ok(_) => {}
                Err(e) if e.kind == SshKeyErrorKind::KeyAlreadyExists => {
                    log::debug!("Key already exists in create_keys_if_needed");
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            public_key = key_pair.public_key().as_ref().to_vec();
            do_write_public_key = true;
        } else if do_write_public_key || !self.authorized_keys.exists() {
            // If we get here we need to get the public key from the private key.
            // If reading fails, the file is corrupted or it is not the format expected.
            // The easiest corrective action would be for the user to delete the file or generate authorized keys using ssh-keygen.
            // Since there is no way to know if the key is being used, an error is returned since   there is no safe way to continue
            // and make sure the keys are valid.
            let key_type: String;
            (key_type, public_key) = read_public_key_from_private(&self.private_key)?;

            //write out the public key, only if the key type is OK
            // ffx by default uses ed25519 keys. If the private key is something else (for example rsa), print
            // bail giving instructions on how to generate the public key.
            if key_type != KEYTYPE_STR {
                return Err(SshKeyError {
                    kind: SshKeyErrorKind::BadKeyType,
                    message: format!("The private key in {priv} is type {key_type}. This program can only verify ed25529 keys.\
            \n To re-add the public key to {auth}, run\
            \n ssh-keygen -y -f {priv} >> {auth}", priv=self.private_key.to_string_lossy(), auth=self.authorized_keys.to_string_lossy()),
                });
            }
            do_write_public_key = true;
        }

        if do_write_public_key {
            // check to write the authorized keys file for when the private existed, and when it was generated.
            write_public_key(&self.authorized_keys, &public_key).map_err(|e| SshKeyError {
                kind: SshKeyErrorKind::IOError,
                message: format!("{e}"),
            })?;
        }

        Ok(())
    }

    /// Checks the validity of the public and private key files.
    /// |repair_if_needed| is true, then any problems with the keys
    /// are corrected if possible.
    /// If it is not possible, an error is returned, and the recommended
    /// course of action is to delete the keys and try again.
    pub fn check_keys(&self, repair_if_needed: bool) -> Result<String, SshKeyError> {
        let mut message: String = String::from("");
        let mut del_priv_key = false;
        let mut recreate_keys = false;
        let mut append_public_key = false;

        match self.analyze_check_keys() {
            // If OK, then return OK.
            Ok(_) => {
                log::info!("SSH Public/Private keys match");
                return Ok("SSH Public/Private keys match".into());
            }
            Err(e) => {
                // If there is an error, and repair_if_needed is not set,
                // return the error.
                if !repair_if_needed {
                    return Err(e);
                }
                match e.kind {
                    // Bad Key Type. This means the private key is
                    // not in the supported format. There is nothing to
                    // do, so return the error. We don't want to delete it
                    // since it may be used by some other process/workflow.
                    SshKeyErrorKind::BadKeyType => return Err(e),
                    // Bad Format. This means one of the files is
                    // not a properly formatted key file. In this case,
                    // we know it cannot be used by another process/workflow
                    // since it cannot be read, so delete it and recreate the
                    // keys.
                    SshKeyErrorKind::BadKeyFormat => {
                        recreate_keys = true;
                        del_priv_key = true;
                        message = format!("{}. Regenerating a private key.", e.message)
                    }
                    // Bad File permission. This means the private key file permission is
                    // wrong. Fix it.
                    SshKeyErrorKind::BadFilePermission => {
                        let meta = self.private_key.metadata().map_err(|e| SshKeyError {
                            kind: SshKeyErrorKind::IOError,
                            message: format!("{e}"),
                        })?;
                        let mut permissions = meta.permissions();
                        permissions.set_mode(0o600);
                        fs::set_permissions(&self.private_key, permissions).map_err(|e| {
                            SshKeyError { kind: SshKeyErrorKind::IOError, message: format!("{e}") }
                        })?;
                    }
                    // Key Mismatch. This is the case of a valid private key, but the matching
                    // public key is not found. In this case, add the public key to the
                    // authorized key file.
                    SshKeyErrorKind::KeyMismatch => {
                        message = format!("{e}");
                        recreate_keys = true;
                        del_priv_key = false;
                        append_public_key = true;
                    }
                    // Any other errors, recreate the keys, reusing the private key if present.
                    _ => {
                        message = format!("{e}");
                        recreate_keys = true;
                    }
                };
            }
        };

        if recreate_keys {
            if del_priv_key && self.private_key.exists() {
                fs::remove_file(&self.private_key).map_err(|e| SshKeyError {
                    kind: SshKeyErrorKind::IOError,
                    message: format!("Cannot delete {:?}: {e}", self.private_key),
                })?;
            }
            // If we get here, there was an error condition that requires the keys to
            // be (re)created. This may include generating a new private key, or just
            // the authorized public keys, or both.
            match self.create_keys_if_needed(append_public_key) {
                Ok(_) => message = format!("Keys repaired: {message}."),
                Err(e) => {
                    // If there was an error, print it, delete the keys,
                    // and recreate.
                    log::error!(
                        "Error repairing SSH keys {e:?}. Please check configuration and/or delete existing key files and retry."
                    );

                    return Err(e);
                }
            };
        }
        Ok(message)
    }

    /// Checks that the corresponding public key from the private key file
    /// is listed in the authorized keys file.
    fn analyze_check_keys(&self) -> Result<(), SshKeyError> {
        if !self.private_key.exists() {
            return Err(SshKeyError {
                kind: SshKeyErrorKind::FileNotFound,
                message: format!(
                    "Private key {} does not exist",
                    self.private_key.to_string_lossy()
                ),
            });
        } else {
            let meta = self.private_key.metadata().map_err(|e| SshKeyError {
                kind: SshKeyErrorKind::IOError,
                message: format!("{e}"),
            })?;
            let mode = meta.permissions().mode();
            if mode != 0o100600 {
                return Err(SshKeyError {
                    kind: SshKeyErrorKind::BadFilePermission,
                    message: format!(
                        "Private key {} has the wrong file permissions. SSH requires 0o600, found {mode:#o}",
                        self.private_key.display()
                    ),
                });
            }
        }
        if !self.authorized_keys.exists() {
            return Err(SshKeyError {
                kind: SshKeyErrorKind::FileNotFound,
                message: format!(
                    "Authorized key file {} does not exist",
                    self.authorized_keys.to_string_lossy()
                ),
            });
        }

        let (key_type, public_key) = match read_public_key_from_private(&self.private_key) {
            Ok((key_type, public_key)) => (key_type, public_key),
            Err(e) => {
                log::debug!("Internal error for read_public_key_from_private: {e:?}");
                return Err(SshKeyError {
                    kind: SshKeyErrorKind::BadKeyFormat,
                    message: format!(
                        "Could not read data from private key {}",
                        self.private_key.display()
                    ),
                });
            }
        };
        let entry = build_public_key_entry(&key_type, &public_key).map_err(|e| SshKeyError {
            kind: SshKeyErrorKind::BadKeyFormat,
            message: format!("{e}"),
        })?;

        // ffx by default uses ed25519 keys. If the private key is something else (for example rsa), print
        // bail giving instructions on how to generate the public key.
        if key_type != KEYTYPE_STR {
            return Err(SshKeyError {
                kind: SshKeyErrorKind::BadKeyType,
                message: format!("The private key in {priv} is type {key_type}. This program can only verify ed25529 keys.\
            \n To re-add the public key to {auth}, run\
            \n ssh-keygen -y -f {priv} >> {auth}", priv=self.private_key.to_string_lossy(), auth=self.authorized_keys.to_string_lossy()),
            });
        }

        let file = File::open(&self.authorized_keys)
            .map_err(|e| SshKeyError { kind: SshKeyErrorKind::IOError, message: format!("{e}") })?;
        // Read the file line by line, and return an iterator of the lines of the file.
        // Note: This check is only for the keys that could be generated by ad-hoc or by ffx directly and used
        // on the target Fuchsia device to secure the ssh connection to the device.
        // Specifically, we're loooking for a ssh-ed25519 key type that is not part of a key ring. Optional
        // fields before the key type are not expected.
        if !BufReader::new(file)
            .lines()
            .into_iter()
            .map(|l| l.unwrap())
            .any(|l| l.starts_with(&entry))
        {
            return Err(SshKeyError {
                kind: SshKeyErrorKind::KeyMismatch,
                message: format!(
                    "Could not find matching public key for the private key {}",
                    self.private_key.to_string_lossy()
                ),
            });
        }
        Ok(())
    }
}

/// Formats the key data for the authorized_keys file.
fn get_public_key_data(pubkey: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut out_bytes: Vec<u8> = vec![];

    // public key is 2 "cstrings", which are strings with no null terminator preceded by the
    // length.
    write_cstring(&mut out_bytes, KEYTYPE)?;
    write_cstring(&mut out_bytes, pubkey)?;

    Ok(out_bytes)
}

/// Builds the authorized_keys entry for the given public key.
fn build_public_key_entry(key_type: &str, public_key: &[u8]) -> Result<String, std::io::Error> {
    let public_key_data = get_public_key_data(public_key)?;
    let pubkey_b64 =
        Base64Display::new(&public_key_data, &base64::engine::general_purpose::STANDARD);
    Ok(format!("{} {}", key_type, pubkey_b64))
}

/// Appends the public key information to the authorized_keys file.
fn write_public_key(path: &PathBuf, public_key: &[u8]) -> Result<(), SshKeyInternalError> {
    log::info!("Writing authorized_keys file: {}", path.display());

    let mut w = if !path.exists() {
        if let Some(parent) = path.parent() {
            DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        };
        OpenOptions::new().write(true).read(true).create_new(true).mode(0o600).open(&path)?
    } else {
        // append to the file.
        OpenOptions::new().write(true).append(true).open(&path)?
    };
    writeln!(
        &mut w,
        "{} {}",
        build_public_key_entry(str::from_utf8(KEYTYPE)?, public_key)?,
        COMMENT
    )?;
    // File is closed when it goes out of scope.
    Ok(())
}

/// Writes the private key file.
fn write_private_key(
    path: &PathBuf,
    key_pair: &Ed25519KeyPair,
    document: &[u8],
    rng: &SystemRandom,
) -> Result<(), SshKeyInternalError> {
    // private key file
    let none = b"none";

    // magic pattern to identify this data, null terminated.
    let mut priv_out_bytes: Vec<u8> = vec![];
    priv_out_bytes.write_all(AUTH_MAGIC)?;

    // ciphername
    write_cstring(&mut priv_out_bytes, none)?;

    // kdfname (none), and length 0
    write_cstring(&mut priv_out_bytes, none)?;
    priv_out_bytes.write_all(&[0, 0, 0, 0])?;

    // number of keys, always 1.
    priv_out_bytes.write_all(&[0, 0, 0, 1])?;

    // public key - this is the same contents as appears in the authorized_keys file.
    let public_key_data = get_public_key_data(key_pair.public_key().as_ref())?;
    write_cstring(&mut priv_out_bytes, &public_key_data)?;

    // private key.
    let mut key_bytes: Vec<u8> = vec![];

    // random u32 checkbytes, write it 2 times.
    let rand_bytes: [u8; 4] = rand::generate(rng).unwrap().expose();
    key_bytes.write_all(&rand_bytes)?;
    key_bytes.write_all(&rand_bytes)?;

    // The type of key.
    write_cstring(&mut key_bytes, KEYTYPE)?;

    // Extract the secret part of the key from the pkcs8 document.
    // the first 16 bytes are the version and algorithm oid. The private
    // key data starts at 16, and is 32 bytes
    // secret key, should be 32 bytes.
    let secret = &document[16..48];

    // pub key 32 bytes.
    write_cstring(&mut key_bytes, key_pair.public_key().as_ref())?;

    // the private key is the secret with the public appended for a
    // total of 64 bytes.
    let mut private_key_data: Vec<u8> = Vec::from(secret);
    private_key_data.extend_from_slice(key_pair.public_key().as_ref());
    write_cstring(&mut key_bytes, &private_key_data)?;

    // add the comment.
    write_cstring(&mut key_bytes, COMMENT.as_bytes())?;

    // padding
    let mut i: u8 = 0;
    while key_bytes.len() % 8 != 0 {
        i += 1;
        key_bytes.write_all(&[i])?;
    }

    write_cstring(&mut priv_out_bytes, &key_bytes)?;

    let begin = "-----BEGIN OPENSSH PRIVATE KEY-----\n";
    let end = "-----END OPENSSH PRIVATE KEY-----\n";

    if let Some(parent) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    };
    let mut w =
        OpenOptions::new().write(true).read(true).create_new(true).mode(0o600).open(&path)?;
    writeln!(&mut w, "{}", begin)?;
    writeln!(
        &mut w,
        "{}",
        Base64Display::new(&priv_out_bytes, &base64::engine::general_purpose::STANDARD)
    )?;
    writeln!(&mut w, "{}", end)?;
    // File is closed when it goes out of scope.
    Ok(())
}

/// Reads the public key from the private key file.
fn read_public_key_from_private(path: &PathBuf) -> Result<(String, Vec<u8>), SshKeyInternalError> {
    let mut started = false;
    let mut encoded: String = String::from("");
    let priv_key_file = File::open(path)?;
    for line_result in BufReader::new(priv_key_file).lines() {
        let line = line_result?;
        if line.starts_with("----") && line.contains("BEGIN OPENSSH PRIVATE KEY") {
            started = true;
            continue;
        }
        if line.starts_with("----") && line.contains("END OPENSSH PRIVATE KEY") {
            //done
            break;
        }
        if started {
            // append all lines be between begin and end, trimming whitespace.
            encoded.push_str(line.trim());
        }
    }
    // decode the base64 string into bytes.
    let data = BASE64_STANDARD.decode(&encoded)?;
    let mut buf = Cursor::new(data);

    let mut element: Vec<u8> = vec![];

    // read the magic, it is null terminated.
    buf.read_until(0, &mut element)?;
    if element != AUTH_MAGIC {
        return Err(SshKeyInternalError {
            kind: SshKeyErrorKind::BadKeyFormat,
            message: format!("Invalid private key header {:?}", &element),
        });
    }

    // read cipher and kdf settings, both none.
    element = read_cstring(&mut buf)?;
    if "none" != str::from_utf8(&element)? {
        return Err(SshKeyInternalError {
            kind: SshKeyErrorKind::BadKeyFormat,
            message: format!("Invalid private key header, expected 'none' {:?}", &element),
        });
    }
    element = read_cstring(&mut buf)?;
    if "none" != str::from_utf8(&element)? {
        return Err(SshKeyInternalError {
            kind: SshKeyErrorKind::BadKeyFormat,
            message: format!("Invalid private key header, expected 'none' {:?}", &element),
        });
    }
    let mut u32_bytes = [0u8; 4];
    buf.read_exact(&mut u32_bytes)?;
    if u32::from_be_bytes(u32_bytes) != 0 {
        return Err(SshKeyInternalError {
            kind: SshKeyErrorKind::BadKeyFormat,
            message: format!("Invalid private key header, expected 0, got {:?}", &u32_bytes),
        });
    }

    // read number of keys, should only be 1.
    buf.read_exact(&mut u32_bytes)?;
    if u32::from_be_bytes(u32_bytes) != 1 {
        return Err(SshKeyInternalError {
            kind: SshKeyErrorKind::BadKeyFormat,
            message: format!("Invalid private key count, expected 1, got {:?}", &u32_bytes),
        });
    }

    // read the public key data
    element = read_cstring(&mut buf)?;

    // this is keytype|key. Read the type, then return the key
    let mut keydata = Cursor::new(&element);
    let key_type = read_cstring(&mut keydata)?;
    let pubkey = read_cstring(&mut keydata)?;

    Ok((str::from_utf8(&key_type)?.to_string(), pubkey))
}

fn write_cstring(buf: &mut dyn Write, bytes: &[u8]) -> Result<(), std::io::Error> {
    let len: u32 = bytes.len().try_into().expect("usize cast to u32");
    buf.write_all(&len.to_be_bytes())?;
    buf.write_all(bytes)?;
    Ok(())
}

fn read_cstring(buf: &mut dyn Read) -> Result<Vec<u8>, std::io::Error> {
    let mut size = [0u8; 4];
    buf.read_exact(&mut size)?;
    let len = u32::from_be_bytes(size);
    if len > 0 {
        let sz: usize = len.try_into().unwrap();
        let mut ret: Vec<u8> = vec![0; sz];
        buf.read_exact(&mut ret)?;
        return Ok(ret);
    }
    Ok(vec![])
}

#[cfg(test)]
mod test {
    use super::*;
    use ffx_config::{ConfigLevel, test_init};
    use serde_json::json;
    use std::io::{Read, Write};
    use tempfile::TempDir;

    #[fuchsia::test]
    async fn test_load() {
        // Set up the test environment and set the ssh key paths
        let env = test_init().expect("test env init");
        env.context
            .query("ssh.pub")
            .level(Some(ConfigLevel::User))
            .build()
            .set(
                &env.context,
                json!(["$ENV_PATH_THAT_IS_NOT_SET", "/expected/default", "someother"]),
            )
            .expect("set ssh.pub");
        env.context
            .query("ssh.priv")
            .level(Some(ConfigLevel::User))
            .build()
            .set(
                &env.context,
                json!([
                    "$ENV_PATH_THAT_IS_NOT_SET_2",
                    "/expected/default/private",
                    "someother/place"
                ]),
            )
            .expect("set ssh.priv");

        // set the config

        let ssh_files = match SshKeyFiles::load(&env.context).await {
            Ok(ssh) => ssh,
            Err(e) => panic!("load failed: {e:?}"),
        };
        assert!(&ssh_files.authorized_keys.display().to_string() == "/expected/default");
        assert!(&ssh_files.private_key.display().to_string() == "/expected/default/private");
    }

    #[test]
    fn test_enum_display() {
        let v = SshKeyErrorKind::BadKeyFormat;
        assert_eq!(format!("{v}"), "BadKeyFormat")
    }
    #[test]
    fn test_create_with_existing() {
        let tmp_dir = TempDir::new().expect("create temp dir");

        let auth_key_path = tmp_dir.path().join("authorized_keys");
        let private_path = tmp_dir.path().join("privatekey");

        // scope to force the file to close.
        {
            let mut tmp_file = File::create(&auth_key_path).expect("create authorized ");
            let test_private_key = include_str!("../testdata/test1_ed25519");
            tmp_file.write_all(b"unchanged\n").expect("write authorized keys bytes");
            let mut priv_file = File::create(&private_path).expect("create private key path");
            priv_file.write_all(test_private_key.as_bytes()).expect("write private key bytes");
        }

        let ssh_files = SshKeyFiles { authorized_keys: auth_key_path, private_key: private_path };
        if let Err(e) = ssh_files.create_keys_if_needed(false) {
            panic!("create_keys_if_needed failed: {e:?}");
        }

        let contents = fs::read_to_string(ssh_files.authorized_keys).expect("read authorized keys");
        let lines: Vec<&str> = contents.lines().collect();

        // existing keys should not be modified by create_keys_if_needed.
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "unchanged");
    }

    #[test]
    fn test_create_with_missing_auth_keys() {
        let tmp_dir = TempDir::new().expect("create temp dir");

        let auth_key_path = tmp_dir.path().join("authorized_keys");
        let private_path = tmp_dir.path().join("privatekey");

        // scope to force the file to close.
        {
            let test_private_key = include_str!("../testdata/test1_ed25519");
            let mut priv_file = File::create(&private_path).expect("Create priv key path");
            priv_file.write_all(test_private_key.as_bytes()).expect("write contents of priv key");
        }

        let ssh_files = SshKeyFiles { authorized_keys: auth_key_path, private_key: private_path };
        if let Err(e) = ssh_files.create_keys_if_needed(false) {
            panic!("create_keys_if_needed failed: {e:?}");
        }

        let contents =
            fs::read_to_string(ssh_files.authorized_keys).expect("read authorized_keys contents");
        let expected_contents = include_str!("../testdata/test1_authorized_keys");

        assert!(contents == expected_contents);
    }

    #[test]
    fn test_create_with_missing_keys() {
        let tmp_dir = TempDir::new().expect("create temp dir");

        let auth_key_path = tmp_dir.path().join("authorized_keys");
        let private_path = tmp_dir.path().join("privatekey");

        assert!(!&auth_key_path.exists());
        assert!(!&private_path.exists());

        let ssh_files = SshKeyFiles { authorized_keys: auth_key_path, private_key: private_path };
        if let Err(e) = ssh_files.create_keys_if_needed(false) {
            panic!("create_keys_if_needed failed: {e:?}");
        }

        assert!(&ssh_files.authorized_keys.exists());
        assert!(&ssh_files.private_key.exists());
    }

    #[test]
    fn test_write_cstring() {
        let mut data = vec![];
        let mut expected_data: Vec<u8> = vec![0, 0, 0, 5];
        expected_data.extend_from_slice("hello".as_bytes());
        if let Err(e) = write_cstring(&mut data, "hello".as_bytes()) {
            panic!("write_cstring failed: {e:?}");
        }

        assert!(data == expected_data);

        let mut input = Cursor::new(data);
        let read_data = match read_cstring(&mut input) {
            Ok(d) => d,
            Err(e) => panic!("read_cstring failed: {e:?}"),
        };
        assert!(read_data == "hello".as_bytes());
    }

    #[test]
    fn test_create_with_missing_directory_for_keys() {
        let tmp_dir = TempDir::new().expect("creating temp dir");

        let new_dir_path = tmp_dir.path().join("new-dir");
        let auth_key_path = new_dir_path.join("authorized_keys");
        let private_path = new_dir_path.join("privatekey");

        assert!(!&auth_key_path.exists());
        assert!(!&private_path.exists());

        let ssh_files = SshKeyFiles { authorized_keys: auth_key_path, private_key: private_path };
        match ssh_files.create_keys_if_needed(false) {
            Ok(_) => (),
            Err(e) => panic!("create keys if needed error: {e:?}"),
        };

        assert!(&ssh_files.authorized_keys.exists());
        assert!(&ssh_files.private_key.exists());
    }

    #[test]
    fn test_check_keys() {
        let tmp_dir = TempDir::new().expect("creating temp dir");

        let new_dir_path = tmp_dir.path().join("new-dir");
        let auth_key_path = new_dir_path.join("authorized_keys");
        let private_path = new_dir_path.join("privatekey");

        assert!(!&auth_key_path.exists());
        assert!(!&private_path.exists());

        let ssh_files = SshKeyFiles { authorized_keys: auth_key_path, private_key: private_path };
        ssh_files.create_keys_if_needed(false).expect("creating test keys");

        match ssh_files.check_keys(false) {
            Ok(_) => (),
            Err(e) => panic!("create keys if needed error: {e:?}"),
        };
    }

    #[test]
    fn test_check_keys_missing() {
        let tmp_dir = TempDir::new().expect("creating temp dir");

        let new_dir_path = tmp_dir.path().join("new-dir");
        let auth_key_path = new_dir_path.join("authorized_keys");
        let private_path = new_dir_path.join("privatekey");

        assert!(!&auth_key_path.exists());
        assert!(!&private_path.exists());

        let ssh_files = SshKeyFiles { authorized_keys: auth_key_path, private_key: private_path };

        match ssh_files.check_keys(false) {
            Ok(_) => panic!("missing keys should fail"),
            Err(e) => assert_eq!(e.kind, SshKeyErrorKind::FileNotFound, "{e:?}"),
        }
    }

    #[test]
    fn test_check_keys_mismatch() {
        let tmp_dir = TempDir::new().expect("create temp dir");

        let new_dir_path = tmp_dir.path().join("new-dir");
        let auth_key_path = new_dir_path.join("authorized_keys");
        let private_path = new_dir_path.join("privatekey");
        let other_auth_key_path = new_dir_path.join("other_authorized_keys");
        let other_private_path = new_dir_path.join("other_privatekey");

        assert!(!&auth_key_path.exists());
        assert!(!&private_path.exists());

        let ssh_files =
            SshKeyFiles { authorized_keys: auth_key_path.clone(), private_key: private_path };
        match ssh_files.create_keys_if_needed(false) {
            Ok(_) => (),
            Err(e) => panic!("create keys if needed error: {e:?}"),
        };

        let other_ssh_files = SshKeyFiles {
            authorized_keys: other_auth_key_path,
            private_key: other_private_path.clone(),
        };
        match other_ssh_files.create_keys_if_needed(false) {
            Ok(_) => (),
            Err(e) => panic!("create keys if needed error: {e:?}"),
        };

        let mismatched =
            SshKeyFiles { authorized_keys: auth_key_path, private_key: other_private_path };

        match mismatched.check_keys(false) {
            Ok(_) => panic!("mismatched keys should fail"),
            Err(e) => assert_eq!(e.kind, SshKeyErrorKind::KeyMismatch, "{e:?}"),
        };
    }

    #[test]
    fn test_check_keys_mismatch_repaired() {
        let tmp_dir = TempDir::new().expect("create temp dir");

        let new_dir_path = tmp_dir.path().join("new-dir");
        let auth_key_path = new_dir_path.join("authorized_keys");
        let private_path = new_dir_path.join("privatekey");
        let other_auth_key_path = new_dir_path.join("other_authorized_keys");
        let other_private_path = new_dir_path.join("other_privatekey");

        assert!(!&auth_key_path.exists());
        assert!(!&private_path.exists());

        let ssh_files =
            SshKeyFiles { authorized_keys: auth_key_path.clone(), private_key: private_path };
        match ssh_files.create_keys_if_needed(false) {
            Ok(_) => (),
            Err(e) => panic!("create keys if needed error: {e:?}"),
        };

        let other_ssh_files = SshKeyFiles {
            authorized_keys: other_auth_key_path,
            private_key: other_private_path.clone(),
        };
        match other_ssh_files.create_keys_if_needed(false) {
            Ok(_) => (),
            Err(e) => panic!("create keys if needed error: {e:?}"),
        };

        let mismatched =
            SshKeyFiles { authorized_keys: auth_key_path, private_key: other_private_path.clone() };

        match mismatched.check_keys(true) {
            Ok(message) => assert_eq!(
                message,
                format!(
                    "Keys repaired: KeyMismatch:Could not find matching public key for the private key {}.",
                    other_private_path.to_string_lossy()
                )
            ),
            Err(e) => assert_eq!(e.kind, SshKeyErrorKind::KeyMismatch, "{e:?}"),
        };

        //check the mismatched keys are OK.
        match mismatched.check_keys(false) {
            Ok(_) => (),
            Err(e) => panic!("unexpected error {e} for mismatched keys"),
        };

        // other should be ok since nothing should have changed.
        match other_ssh_files.check_keys(false) {
            Ok(_) => (),
            Err(e) => panic!("unexpected error {e} for other keys"),
        }

        // ssh_keys should shill be OK too.
        match ssh_files.check_keys(false) {
            Ok(_) => (),
            Err(e) => panic!("unexpected error {e} ssh keys"),
        }
    }

    #[test]
    fn test_ssh_key_set_insert() {
        let mut key_set = SshKeySet::new();
        let mut key1 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key1_data".to_string(),
            comment: Some("comment1".to_string()),
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir1"))]),
        };

        key_set.insert(key1.clone());

        // The key should be in the inner map, and its sources should contain /dir1
        let mut expected_dirs = HashSet::new();
        expected_dirs.insert(SshKeySource::File(PathBuf::from("/dir1")));
        key1.sources.clear(); // insert clears sources
        assert_eq!(key_set.inner.get(&key1), Some(&expected_dirs));

        // Insert the same key but with a different parent directory
        let key2 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key1_data".to_string(),
            comment: Some("comment2".to_string()), // comment is ignored for hashing
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir2"))]),
        };
        key_set.insert(key2.clone());

        // The sources for key1 should now contain both /dir1 and /dir2
        expected_dirs.insert(SshKeySource::File(PathBuf::from("/dir2")));
        assert_eq!(key_set.inner.get(&key1), Some(&expected_dirs));
    }

    #[test]
    fn test_ssh_key_set_into_hashset() {
        let mut key_set = SshKeySet::new();
        let key1 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key1_data".to_string(),
            comment: Some("comment1".to_string()),
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir1"))]),
        };
        let key2 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key1_data".to_string(),
            comment: Some("comment2".to_string()),
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir2"))]),
        };
        let key3 = SshKey {
            key_type: "ssh-rsa".to_string(),
            key: "key2_data".to_string(),
            comment: None,
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir3"))]),
        };

        key_set.insert(key1);
        key_set.insert(key2);
        key_set.insert(key3);

        let hash_set = key_set.into_hashset();

        assert_eq!(hash_set.len(), 2);

        let expected_key1 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key1_data".to_string(),
            comment: Some("comment1".to_string()), // The comment of the first inserted key is kept.
            sources: HashSet::from([
                SshKeySource::File(PathBuf::from("/dir1")),
                SshKeySource::File(PathBuf::from("/dir2")),
            ]),
        };
        let expected_key2 = SshKey {
            key_type: "ssh-rsa".to_string(),
            key: "key2_data".to_string(),
            comment: None,
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir3"))]),
        };

        assert!(hash_set.contains(&expected_key1));
        assert!(hash_set.contains(&expected_key2));

        // Check sources of the found key
        let found_key1 = hash_set.get(&expected_key1).unwrap();
        assert_eq!(found_key1.sources, expected_key1.sources);
    }

    #[test]
    fn test_ssh_key_set_union() {
        let mut key_set1 = SshKeySet::new();
        let key1 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key1_data".to_string(),
            comment: Some("comment1".to_string()),
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir1"))]),
        };
        let key2 = SshKey {
            key_type: "ssh-rsa".to_string(),
            key: "key2_data".to_string(),
            comment: None,
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir2"))]),
        };
        key_set1.insert(key1);
        key_set1.insert(key2);

        let mut key_set2 = SshKeySet::new();
        let key3 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key1_data".to_string(),
            comment: Some("comment3".to_string()),
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir3"))]),
        };
        let key4 = SshKey {
            key_type: "ssh-dss".to_string(),
            key: "key3_data".to_string(),
            comment: None,
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir4"))]),
        };
        key_set2.insert(key3);
        key_set2.insert(key4);

        let union_set = key_set1.union(key_set2);
        let hash_set = union_set.into_hashset();

        assert_eq!(hash_set.len(), 3);

        let expected_key1 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key1_data".to_string(),
            comment: Some("comment1".to_string()),
            sources: HashSet::from([
                SshKeySource::File(PathBuf::from("/dir1")),
                SshKeySource::File(PathBuf::from("/dir3")),
            ]),
        };
        let expected_key2 = SshKey {
            key_type: "ssh-rsa".to_string(),
            key: "key2_data".to_string(),
            comment: None,
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir2"))]),
        };
        let expected_key3 = SshKey {
            key_type: "ssh-dss".to_string(),
            key: "key3_data".to_string(),
            comment: None,
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir4"))]),
        };

        assert!(hash_set.contains(&expected_key1));
        assert!(hash_set.contains(&expected_key2));
        assert!(hash_set.contains(&expected_key3));

        let found_key1 = hash_set.get(&expected_key1).unwrap();
        assert_eq!(found_key1.sources, expected_key1.sources);
    }

    const TEST_DATA_DIR_1: &str = "../../src/developer/ffx/lib/ssh/testdata/key_parsing";
    const TEST_DATA_DIR_2: &str = "../../src/developer/ffx/lib/ssh/testdata/key_parsing/other_dir";

    #[test]
    fn test_find_ssh_keys() {
        let paths = vec![PathBuf::from(TEST_DATA_DIR_1), PathBuf::from(TEST_DATA_DIR_2)];
        let keys = find_ssh_keys_in_dirs(&paths).expect("valid ssh keys should have been found");
        // There should only be two keys despite the directories and clones of one of the keys.
        assert_eq!(keys.inner.len(), 2);
        let keyset = keys.into_hashset();
        let cloned_key = keyset.iter().filter(|k| k.sources.len() == 3).next().unwrap();
        assert!(cloned_key.sources.iter().any(|loc| *loc
            == SshKeySource::File(PathBuf::from(
                "../../src/developer/ffx/lib/ssh/testdata/key_parsing/other_dir/key_other_copy.pub"
            ))));
        let unique_key = keyset.iter().filter(|k| k.sources.len() == 1).next().unwrap();
        assert!(
            *unique_key.sources.iter().next().unwrap()
                == SshKeySource::File(PathBuf::from(
                    "../../src/developer/ffx/lib/ssh/testdata/key_parsing/other_key.pub"
                ))
        )
    }

    // Helper to create a simple HTTP server.
    fn start_test_server(body: &'static str) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let response =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
                stream.write_all(response.as_bytes()).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                let mut buf = [0; 128];
                // Read until EOF to make sure client has received everything.
                while stream.read(&mut buf).unwrap_or(0) > 0 {}
            }
        });
        addr
    }

    #[fuchsia::test]
    async fn test_query_device_authorized_keys_success() {
        const FAKE_KEYS: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAID/PA/k3f5aSU/22c9LPn/4A3f/g/2Yj/2c/8A3/g/2Y test1@fuchsia\n\
                                 ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQD... test2@fuchsia";
        let server_addr = start_test_server(FAKE_KEYS);

        let keys = query_device_authorized_keys(server_addr, server_addr.port()).await.unwrap();

        assert_eq!(keys.len(), 2);
        let key1 = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "AAAAC3NzaC1lZDI1NTE5AAAAID/PA/k3f5aSU/22c9LPn/4A3f/g/2Yj/2c/8A3/g/2Y".to_string(),
            comment: Some("test1@fuchsia".to_string()),
            sources: HashSet::new(),
        };
        let key2 = SshKey {
            key_type: "ssh-rsa".to_string(),
            key: "AAAAB3NzaC1yc2EAAAADAQABAAABAQD...".to_string(),
            comment: Some("test2@fuchsia".to_string()),
            sources: HashSet::new(),
        };
        assert!(keys.contains(&key1));
        assert!(keys.contains(&key2));
    }

    #[fuchsia::test]
    async fn test_query_device_authorized_keys_empty_response() {
        let server_addr = start_test_server("");

        let result = query_device_authorized_keys(server_addr, server_addr.port()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Received empty string for authorized_keys file."));
    }

    #[fuchsia::test]
    async fn test_find_matching_ssh_keys_impl_success() {
        const DEVICE_KEYS: &str = "ssh-ed25519 key_on_device_and_local comment1\n\
                                   ssh-ed25519 key_on_device_only comment2";
        let server_addr = start_test_server(DEVICE_KEYS);

        let mut local_keys = HashSet::new();
        let matching_key = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key_on_device_and_local".to_string(),
            comment: Some("comment1".to_string()),
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/fake/path"))]),
        };
        let non_matching_key = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key_on_local_only".to_string(),
            comment: Some("comment3".to_string()),
            sources: HashSet::new(),
        };
        local_keys.insert(matching_key.clone());
        local_keys.insert(non_matching_key);

        let local_ssh_dirs = vec![PathBuf::from("/fake/path")];

        let found_keys = find_matching_ssh_keys_impl(
            local_ssh_dirs,
            local_keys,
            server_addr,
            server_addr.port(),
        )
        .await
        .unwrap();

        assert_eq!(found_keys.len(), 1);
        assert!(found_keys.contains(&matching_key));
        let found_key = found_keys.iter().next().unwrap();
        assert_eq!(found_key.sources, matching_key.sources);
    }

    #[fuchsia::test]
    async fn test_find_matching_ssh_keys_impl_no_match() {
        const DEVICE_KEYS: &str = "ssh-ed25519 key_on_device_only comment1";
        let server_addr = start_test_server(DEVICE_KEYS);

        let mut local_keys = HashSet::new();
        let non_matching_key = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "key_on_local_only".to_string(),
            comment: Some("comment2".to_string()),
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/fake/path"))]),
        };
        local_keys.insert(non_matching_key);

        let local_ssh_dirs = vec![PathBuf::from("/fake/path")];

        let result = find_matching_ssh_keys_impl(
            local_ssh_dirs,
            local_keys,
            server_addr,
            server_addr.port(),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains(
                "None of the following device SSH public keys matched any local ssh keys"
            )
        );
        assert!(err.to_string().contains("key_on_device_only"));
        assert!(err.to_string().contains("key_on_local_only"));
    }

    #[test]
    fn test_local_non_agent_keys_message_no_keys() {
        let searched_dirs = vec![PathBuf::from("/dir1"), PathBuf::from("/dir2")];
        let local_keys = HashSet::new();
        let message = local_non_agent_keys_message(&searched_dirs, &local_keys);
        assert_eq!(
            message,
            "No local ssh keys were found. We looked in the following locations:\n\t-- /dir1\n\t-- /dir2"
        );
    }

    #[test]
    fn test_local_non_agent_keys_message_with_keys() {
        let searched_dirs = vec![PathBuf::from("/dir1")];
        let mut local_keys = HashSet::new();
        let key_loc = PathBuf::from("/dir1/key.pub");
        let key = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "some_key".to_string(),
            comment: Some("comment".to_string()),
            sources: HashSet::from([SshKeySource::File(key_loc.clone())]),
        };
        local_keys.insert(key);
        let message = local_non_agent_keys_message(&searched_dirs, &local_keys);
        let expected_key_str = "key type: ssh-ed25519, key: some_key";
        let expected_msg = format!(
            "When searching local directories, we found the following public keys:\n\t-- {}\n\t\t-- Found in {:?}",
            expected_key_str,
            HashSet::from([key_loc])
        );
        assert_eq!(message, expected_msg);
    }

    #[test]
    fn test_ssh_agent_keys_message_no_keys() {
        let mut local_keys = HashSet::new();
        let key = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "some_key".to_string(),
            comment: Some("comment".to_string()),
            sources: HashSet::from([SshKeySource::File(PathBuf::from("/dir1/key.pub"))]),
        };
        local_keys.insert(key);
        let message = ssh_agent_keys_message(&local_keys);
        assert_eq!(message, "No public keys were found from the ssh-agent");
    }

    #[test]
    fn test_ssh_agent_keys_message_with_keys() {
        let mut local_keys = HashSet::new();
        let key = SshKey {
            key_type: "ssh-ed25519".to_string(),
            key: "agent_key".to_string(),
            comment: Some("agent comment".to_string()),
            sources: HashSet::new(),
        };
        local_keys.insert(key);
        let message = ssh_agent_keys_message(&local_keys);
        let expected_key_str = "key type: ssh-ed25519, key: agent_key";
        let expected_msg = format!(
            "When querying the ssh-agent we found the following keys:\n\t-- {}",
            expected_key_str
        );
        assert_eq!(message, expected_msg);
    }
}
