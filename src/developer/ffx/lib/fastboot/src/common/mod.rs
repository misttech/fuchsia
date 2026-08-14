// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::common::vars::{
    IS_USERSPACE_VAR, LOCKED_VAR, MAX_DOWNLOAD_SIZE_VAR, PARTITION_SIZE, PARTITION_START,
    PRODUCT_VAR, REVISION_VAR, STREAM_SEGMENT_SIZE,
};
use crate::error::FfxFastbootError;
use crate::file_resolver::FileResolver;
use crate::manifest::{from_in_tree, from_local_product_bundle, from_path, from_sdk};
use crate::util::Event;
use base64::engine::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

type Result<T> = std::result::Result<T, FfxFastbootError>;
use assembly_partitions_config::UploadMethod;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use ffx_config::EnvironmentContext;
use ffx_fastboot_interface::fastboot_interface::{FastbootInterface, RebootEvent, UploadProgress};
use ffx_fastboot_interface::stream::generate_command_list;
use ffx_fastboot_interface::util::{U32_SIZE, convert_log_err};
use ffx_flash_manifest::{ManifestParams, OemFile, SSH_OEM_COMMAND};
use futures::prelude::*;
use futures::try_join;
use pbms::is_local_product_bundle;
use sdk::SdkVersion;
use sparse::reader::SparseReader;
use sparse::{build_sparse_files, resparse_sparse_img};
use std::fs::File;
use std::io::Read;
use std::num::{NonZeroU64, ParseIntError};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use zerocopy::IntoBytes;

pub const MISSING_CREDENTIALS: &str = "The flash manifest is missing the credential files to unlock this device.\n\
     Please unlock the target and try again.";

pub mod crypto;
pub mod vars;

pub trait Partition {
    fn name(&self) -> &str;
    fn file(&self) -> &str;
    fn variable(&self) -> Option<&str>;
    fn variable_value(&self) -> Option<&str>;
}

pub trait Product<P> {
    fn name(&self) -> &String;
    fn bootloader_partitions(&self) -> &Vec<P>;
    fn partitions(&self) -> &Vec<P>;
    fn oem_files(&self) -> &Vec<OemFile>;
}

#[async_trait]
pub trait Flash {
    async fn flash<F, T>(
        &self,
        messenger: &Sender<Event>,
        file_resolver: &mut F,
        fastboot_interface: &mut T,
        cmd: ManifestParams,
        ssh_key_upload_method: Option<&UploadMethod>,
    ) -> Result<()>
    where
        F: FileResolver + Sync + Send,
        T: FastbootInterface;
}

#[async_trait]
pub trait Unlock {
    async fn unlock<F, T>(
        &self,
        _messenger: &Sender<Event>,
        _file_resolver: &mut F,
        _fastboot_interface: &mut T,
    ) -> Result<()>
    where
        F: FileResolver + Sync + Send,
        T: FastbootInterface,
    {
        return Err(FfxFastbootError::UnlockNotSupported);
    }
}

#[async_trait]
pub trait Boot {
    async fn boot<F, T>(
        &self,
        messenger: Sender<Event>,
        file_resolver: &mut F,
        slot: String,
        fastboot_interface: &mut T,
        cmd: ManifestParams,
    ) -> Result<()>
    where
        F: FileResolver + Sync + Send,
        T: FastbootInterface;
}

pub const MISSING_PRODUCT: &str = "Manifest does not contain product";

const LOCK_COMMAND: &str = "vx-lock";

pub const UNLOCK_ERR: &str = "The product requires the target to be unlocked. \
                                     Please unlock target and try again.";

pub async fn stage_file<F: FileResolver + Sync, T: FastbootInterface>(
    prog_client: Sender<UploadProgress>,
    file_resolver: &mut F,
    resolve: bool,
    file: &str,
    fastboot_interface: &mut T,
) -> Result<()> {
    let file_to_upload =
        if resolve { file_resolver.get_file(file).await? } else { file.to_string() };
    log::debug!("Preparing to stage {}", file_to_upload);
    fastboot_interface.stage(&file_to_upload, prog_client).await?;
    Ok(())
}

async fn do_flash<F: FastbootInterface>(
    name: &str,
    messenger: &Sender<Event>,
    fastboot_interface: &mut F,
    file_to_upload: &str,
    timeout: Duration,
) -> Result<()> {
    let (prog_client, mut prog_server): (Sender<UploadProgress>, Receiver<UploadProgress>) =
        mpsc::channel(1);
    try_join!(
        fastboot_interface
            .flash(name, file_to_upload, prog_client, timeout)
            .map_err(FfxFastbootError::Interface),
        async {
            loop {
                match prog_server.recv().await {
                    Some(upload) => {
                        messenger.send(Event::Upload(upload)).await?;
                    }
                    None => {
                        messenger
                            .send(Event::FlashPartition { partition_name: name.to_string() })
                            .await?;
                        return Ok(());
                    }
                }
            }
        }
    )?;
    Ok(())
}

async fn flash_partition_sparse<F: FastbootInterface>(
    name: &str,
    messenger: &Sender<Event>,
    file_to_upload: &str,
    fastboot_interface: &mut F,
    max_download_size: u64,
    timeout: Duration,
) -> Result<()> {
    log::debug!("Preparing to flash {} in sparse mode", file_to_upload);

    let sparse_files = build_sparse_files(
        name,
        file_to_upload,
        std::env::temp_dir().as_path(),
        max_download_size,
    )?;

    messenger
        .send(Event::Upload(UploadProgress::OnReady {
            partition: name.to_owned(),
            files: sparse_files.len().try_into()?,
        }))
        .await?;

    for tmp_file_path in sparse_files {
        let tmp_file_name = tmp_file_path.to_str().unwrap();
        do_flash(name, messenger, fastboot_interface, tmp_file_name, timeout).await?;
    }

    Ok(())
}

pub async fn flash_partition<F: FileResolver + Sync, T: FastbootInterface>(
    messenger: Sender<Event>,
    file_resolver: &mut F,
    name: &str,
    file: &str,
    fastboot_interface: &mut T,
    min_timeout_secs: u64,
    flash_timeout_rate_mb_per_second: f64,
) -> Result<()> {
    let file_to_upload = file_resolver.get_file(file).await?;
    log::debug!("Preparing to upload {}", file_to_upload);
    flash_partition_impl(
        messenger,
        name,
        &file_to_upload,
        fastboot_interface,
        min_timeout_secs,
        flash_timeout_rate_mb_per_second,
    )
    .await
}

async fn flash_basic_impl<T: FastbootInterface>(
    messenger: &Sender<Event>,
    name: &str,
    file_to_upload: &str,
    fastboot_interface: &mut T,
    min_timeout_secs: u64,
    flash_timeout_rate_mb_per_second: f64,
) -> Result<()> {
    // If the given file to flash is bigger than what the device can download
    // at once, we need to make a sparse image out of the given file
    let mut file_handle = File::open(&file_to_upload).map_err(|e| FfxFastbootError::FileOpen {
        path: PathBuf::from(&file_to_upload),
        source: e,
    })?;
    let file_size = file_handle
        .metadata()
        .map_err(|e| FfxFastbootError::FileMetadata {
            path: PathBuf::from(&file_to_upload),
            source: e,
        })?
        .len();

    // Calculate the flashing timeout
    let min_timeout = min_timeout_secs;
    let timeout_rate = flash_timeout_rate_mb_per_second;
    let megabytes = (file_size as f64) / 1_000_000.0;
    let mut timeout = (megabytes / timeout_rate) as u64;
    timeout = std::cmp::max(timeout, min_timeout);
    let timeout = Duration::seconds(timeout as i64);
    log::debug!("Estimated timeout: {}s for {}MB", timeout, megabytes);

    // Get as a u32 because of fastboot protocol requirements.
    let max_download_size: u64 =
        get_hex_int::<u32>(MAX_DOWNLOAD_SIZE_VAR, fastboot_interface).await?.into();
    log::trace!("Device Max Download Size: {}", max_download_size);
    log::trace!("File size: {}", file_size);

    let start_time = Utc::now();

    if max_download_size < file_size {
        // Next check if the file given is ALREADY in the sparse image format
        match SparseReader::is_sparse_file(&mut file_handle) {
            Ok(true) => {
                log::debug!(
                    "Image is too big to fit into target RAM and is a sparse image. Re-sparsing"
                );

                log::debug!("Is already a sparse file. Building Reader");
                let mut reader = SparseReader::new(file_handle)?;
                log::debug!("Building sparse image");
                let sparse_files = resparse_sparse_img(
                    &mut reader,
                    std::env::temp_dir().as_path(),
                    max_download_size,
                )?;

                messenger
                    .send(Event::Upload(UploadProgress::OnReady {
                        partition: name.to_owned(),
                        files: sparse_files.len().try_into()?,
                    }))
                    .await?;

                for tmp_file_path in sparse_files {
                    let tmp_file_name = tmp_file_path.to_str().unwrap();
                    do_flash(name, &messenger, fastboot_interface, tmp_file_name, timeout).await?;
                }
            }
            Err(_) | Ok(false) => {
                log::debug!("Image is  too big to fit into target RAM; flashing in sparse mode");
                flash_partition_sparse(
                    name,
                    &messenger,
                    &file_to_upload,
                    fastboot_interface,
                    max_download_size,
                    timeout,
                )
                .await?;
            }
        }
    } else {
        messenger
            .send(Event::Upload(UploadProgress::OnReady { partition: name.to_owned(), files: 1 }))
            .await?;
        do_flash(name, messenger, fastboot_interface, &file_to_upload, timeout).await?;
    }
    messenger
        .send(Event::FlashPartitionFinished {
            partition_name: name.to_string(),
            duration: Utc::now().signed_duration_since(start_time),
        })
        .await?;
    Ok(())
}

trait FromHexStr: Sized {
    fn from_hex_str(val: &str) -> std::result::Result<Self, ParseIntError>;
}

macro_rules! from_hex_str_impl {
    ($int_type:ty) => {
        impl FromHexStr for $int_type {
            fn from_hex_str(val: &str) -> std::result::Result<Self, ParseIntError> {
                Self::from_str_radix(val.trim_start_matches("0x"), 16)
            }
        }
    };
}

from_hex_str_impl!(u32);
from_hex_str_impl!(u64);

async fn get_hex_int<T: FromHexStr>(
    variable: &str,
    interface: &mut impl FastbootInterface,
) -> std::result::Result<T, FfxFastbootError> {
    let value = interface.get_var(variable).await?;
    T::from_hex_str(value.as_str())
        .inspect_err(|_| {
            log::error!(
                "Fastboot {variable} var was not a valid {}: {value}",
                std::any::type_name::<T>()
            )
        })
        .map_err(|e| e.into())
}

fn streaming_err_helper(message: String) -> FfxFastbootError {
    let err = FfxFastbootError::StreamingFlash { message };
    log::error!("{err}");
    err
}

async fn streaming_flash_impl<T: FastbootInterface>(
    messenger: &Sender<Event>,
    name: &str,
    filepath: &str,
    fastboot_interface: &mut T,
    segment_size_bytes: u64,
    partition_start_byte: u64,
    partition_size_bytes: u64,
    timeout: Duration,
) -> Result<bool> {
    let max_download_bytes: u64 = NonZeroU64::new(
        get_hex_int::<u32>(MAX_DOWNLOAD_SIZE_VAR, fastboot_interface).await?.into(),
    )
    .ok_or_else(|| streaming_err_helper(format!("Max download size of 0")))?
    .into();

    if max_download_bytes % U32_SIZE != 0 {
        return Err(streaming_err_helper(format!("Bad max download size: {max_download_bytes}")));
    }

    let segment_size_bytes: u64 = NonZeroU64::new(segment_size_bytes)
        .ok_or_else(|| streaming_err_helper(format!("Bad segment size: {segment_size_bytes}")))?
        .into();

    if segment_size_bytes % U32_SIZE != 0 {
        return Err(streaming_err_helper(format!(
            "Bad streaming segment size: {segment_size_bytes}"
        )));
    }

    if segment_size_bytes > max_download_bytes {
        return Err(streaming_err_helper(format!(
            "Max download < segment size: {max_download_bytes} < {segment_size_bytes}"
        )));
    }

    if partition_start_byte % U32_SIZE != 0 {
        return Err(streaming_err_helper(format!("Bad partition start: {partition_start_byte}")));
    }

    let mut file_handle = File::open(&filepath)
        .map_err(|e| FfxFastbootError::FileOpen { path: PathBuf::from(&filepath), source: e })?;
    let expected = file_handle
        .metadata()
        .map_err(|e| FfxFastbootError::FileMetadata { path: PathBuf::from(&filepath), source: e })?
        .len();

    // Must complete check host side because device never knows the full image size.
    if partition_size_bytes < expected {
        return Err(streaming_err_helper(format!(
            "Tried to stream a {expected} byte image to a {partition_size_bytes} byte partition"
        )));
    }
    if expected % U32_SIZE != 0 {
        // Possibly an error, but basic flash should handle it correctly anyway.
        log::debug!("Un-streamable file size: {expected}");
        return Ok(false);
    }
    if let Ok(true) = SparseReader::is_sparse_file(&mut file_handle) {
        // TODO(b/529455096): sparse files could be
        // converted to a streaming flash representation.
        log::debug!("Streaming flash is incompatible with sparse images");
        return Ok(false);
    }

    // The file contents is a Vec<u32> because 'fill' value checks compare u32 values,
    // and it's easier to start with a stricter alignment and loosen it when required.
    let mut file_contents = vec![0u32; convert_log_err(expected / U32_SIZE)?];
    // TODO(b/529455096): async I/O for file read
    let bytes_read = file_handle
        .read(file_contents.as_mut_slice().as_mut_bytes())
        .inspect_err(|e| log::error!("{e}"))?;
    if bytes_read != usize::try_from(expected)? {
        let err = FfxFastbootError::FileRead { actual: bytes_read.try_into()?, expected };
        log::error!("{err}");
        return Err(err);
    }

    let start_time = Utc::now();
    let commands = generate_command_list(
        file_contents.into_boxed_slice(),
        max_download_bytes.into(),
        segment_size_bytes.into(),
        partition_start_byte.into(),
    );

    let (prog_client, prog_server) = mpsc::channel(commands.len());

    let server_task = async |mut prog_server: Receiver<UploadProgress>| -> Result<()> {
        while let Some(upload) = prog_server.recv().await {
            messenger.send(Event::Upload(upload)).await?;
        }
        Ok(())
    };

    let stream_task = async |prog_client: Sender<UploadProgress>| -> Result<()> {
        // TODO: map the damn error
        let _ = prog_client.send(UploadProgress::OnStarted { size: expected }).await;
        for command in commands {
            fastboot_interface.stream(name, command, &prog_client, timeout).await?;
        }
        // TODO: map the damn error
        let _ = prog_client.send(UploadProgress::OnFinished).await;
        Ok(())
    };

    // We don't want to yoke the stream task and server task too tightly
    // since it's possible for the server task to be slow and to gate
    // progress for the stream task. Unlikely, but it's still better to bundle up all
    // the streaming commands into a single task and then log that progress on a best
    // effort basis.
    messenger
        .send(Event::Upload(UploadProgress::OnReady { partition: name.to_owned(), files: 1 }))
        .await?;
    try_join!(stream_task(prog_client), server_task(prog_server))?;

    let duration = Utc::now().signed_duration_since(start_time);
    messenger
        .send(Event::FlashPartitionFinished { partition_name: name.to_owned(), duration })
        .await?;

    Ok(true)
}

pub async fn flash_partition_impl<T: FastbootInterface>(
    messenger: Sender<Event>,
    name: &str,
    file_to_upload: &str,
    fb_intf: &mut T,
    min_timeout_secs: u64,
    flash_timeout_rate_mb_per_second: f64,
) -> Result<()> {
    fn parameterized_var(base: &str, parameter: &str) -> String {
        format!("{}:{}", base, parameter)
    }

    let mut try_flash_stream = async || {
        if let Ok(segment_size) = get_hex_int::<u64>(STREAM_SEGMENT_SIZE, fb_intf).await
            && let Ok(partition_start) =
                get_hex_int::<u64>(parameterized_var(PARTITION_START, name).as_str(), fb_intf).await
            && let Ok(partition_size) =
                get_hex_int::<u64>(parameterized_var(PARTITION_SIZE, name).as_str(), fb_intf).await
        {
            streaming_flash_impl(
                &messenger,
                name,
                file_to_upload,
                fb_intf,
                segment_size,
                partition_start,
                partition_size,
                Duration::seconds(min_timeout_secs.try_into().unwrap()),
            )
            .await
            .inspect_err(|e| log::error!("Streaming flash error: {e}"))
        } else {
            Ok(false)
        }
    };

    if let Ok(true) = try_flash_stream().await {
        return Ok(());
    }

    flash_basic_impl(
        &messenger,
        name,
        file_to_upload,
        fb_intf,
        min_timeout_secs,
        flash_timeout_rate_mb_per_second,
    )
    .await
}

pub async fn verify_hardware(
    revision: &String,
    product_matches: &[String],
    fastboot_interface: &mut impl FastbootInterface,
) -> Result<()> {
    let rev = fastboot_interface.get_var(REVISION_VAR).await?;
    if let Some(r) = rev.split("-").next() {
        if r == *revision || rev == *revision {
            return Ok(());
        }
    }

    let mut found_product = None;
    if !product_matches.is_empty() {
        match fastboot_interface.get_var(PRODUCT_VAR).await {
            Ok(product) => {
                // Any match of the given set of products means success.
                if product_matches.contains(&product) {
                    return Ok(());
                }
                found_product = Some(product);
            }
            Err(e) => {
                log::warn!("Failed to get product variable from device: {e}");
            }
        }
    }

    return Err(FfxFastbootError::HardwareMismatch {
        expected: revision.clone(),
        found: rev,
        attempted_products: product_matches.to_vec(),
        found_product,
    });
}

pub async fn verify_variable_value(
    var: &str,
    value: &str,
    fastboot_interface: &mut impl FastbootInterface,
) -> Result<bool> {
    log::debug!("Verifying value for variable {} equals {}", var, value);
    Ok(fastboot_interface.get_var(var).await.map(|res| res == value)?)
}

pub async fn reboot_bootloader<F: FastbootInterface>(
    messenger: &Sender<Event>,
    fastboot_interface: &mut F,
) -> Result<()> {
    messenger.send(Event::RebootStarted).await?;
    let (reboot_client, mut reboot_server): (Sender<RebootEvent>, Receiver<RebootEvent>) =
        mpsc::channel(1);
    let start_time = Utc::now();
    try_join!(
        fastboot_interface.reboot_bootloader(reboot_client).map_err(FfxFastbootError::Interface),
        async move {
            match reboot_server.recv().await {
                Some(RebootEvent::OnReboot) => {
                    return Ok(());
                }
                None => {
                    return Err(FfxFastbootError::RebootSignalMissing);
                }
            };
        }
    )?;

    let d = Utc::now().signed_duration_since(start_time);
    log::debug!("Reboot duration: {:.2}s", (d.num_milliseconds() / 1000));
    messenger.send(Event::Rebooted(d)).await?;
    Ok(())
}

/// Uploads a file using the given [UploadMethod].
pub async fn upload_file<F: FileResolver + Sync, T: FastbootInterface>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    resolve: bool,
    file: &str,
    method: &UploadMethod,
    fastboot_interface: &mut T,
) -> Result<()> {
    let file_path = match resolve {
        true => file_resolver.get_file(file).await?,
        false => file.to_string(),
    };

    match method {
        // Staged: stage the data then issue the command.
        UploadMethod::Staged { command } => {
            let (client, mut server) = mpsc::channel(1);
            try_join!(
                async {
                    fastboot_interface
                        .stage(&file_path, client)
                        .await
                        .map_err(FfxFastbootError::Interface)
                },
                async {
                    loop {
                        match server.recv().await {
                            Some(e) => {
                                messenger.send(Event::Upload(e)).await?;
                            }
                            None => {
                                return Ok(());
                            }
                        }
                    }
                }
            )?;

            messenger.send(Event::Oem { oem_command: command.clone() }).await?;
            fastboot_interface.oem(command).await?;
        }
        // Inline: chunk the data into consecutive OEM commands.
        UploadMethod::Inline {
            command_prefix,
            command_max_length,
            init_command,
            finalize_command,
        } => {
            let file_content = std::fs::read(&file_path).map_err(|e| {
                FfxFastbootError::FileOpen { path: PathBuf::from(&file_path), source: e }
            })?;
            let encoded = BASE64_STANDARD.encode(file_content);

            // Determine how much data we can fit in each command.
            let data_len =
                method.command_data_length(fastboot::MAX_COMMAND_LENGTH).map_err(|_| {
                    FfxFastbootError::InlineUploadOverflow {
                        prefix: command_prefix.clone(),
                        max_len: *command_max_length,
                    }
                })?;

            // If the device needs an initialization command, send it first.
            if let Some(init_cmd) = init_command {
                messenger.send(Event::Oem { oem_command: init_cmd.clone() }).await?;
                fastboot_interface.oem(init_cmd).await?;
            }

            for chunk in encoded.as_bytes().chunks(data_len) {
                // `unwrap()` will always succeed here because Base64 only uses single-byte data,
                // so no matter where the chunk splits the result is always valid UTF-8.
                let chunk_str = std::str::from_utf8(chunk).unwrap();
                let cmd = format!("{}{}", command_prefix, chunk_str);
                messenger.send(Event::Oem { oem_command: cmd.clone() }).await?;
                fastboot_interface.oem(&cmd).await?;
            }

            // If the device needs a finalization command, send it at the end.
            if let Some(finalize_command) = finalize_command {
                messenger.send(Event::Oem { oem_command: finalize_command.clone() }).await?;
                fastboot_interface.oem(finalize_command).await?;
            }
        }
    }
    Ok(())
}

pub async fn stage_oem_files<F: FileResolver + Sync, T: FastbootInterface>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    resolve: bool,
    oem_files: &Vec<OemFile>,
    fastboot_interface: &mut T,
) -> Result<()> {
    for oem_file in oem_files {
        upload_file(
            messenger,
            file_resolver,
            resolve,
            oem_file.file(),
            &UploadMethod::Staged { command: oem_file.command().to_string() },
            fastboot_interface,
        )
        .await?;
    }
    Ok(())
}

pub async fn set_slot_a_active(fastboot_interface: &mut impl FastbootInterface) -> Result<()> {
    if fastboot_interface.erase("misc").await.is_err() {
        log::debug!("Could not erase misc partition");
    }
    fastboot_interface.set_active("a").await?;
    Ok(())
}

pub async fn flash_partitions<F: FileResolver + Sync, P: Partition, T: FastbootInterface>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    partitions: &Vec<P>,
    fastboot_interface: &mut T,
    min_timeout_secs: u64,
    flash_timeout_rate_mb_per_second: f64,
) -> Result<()> {
    for partition in partitions {
        match (partition.variable(), partition.variable_value()) {
            (Some(var), Some(value)) => {
                if verify_variable_value(var, value, fastboot_interface).await? {
                    flash_partition(
                        messenger.clone(),
                        file_resolver,
                        partition.name(),
                        partition.file(),
                        fastboot_interface,
                        min_timeout_secs,
                        flash_timeout_rate_mb_per_second,
                    )
                    .await?;
                }
            }
            _ => {
                flash_partition(
                    messenger.clone(),
                    file_resolver,
                    partition.name(),
                    partition.file(),
                    fastboot_interface,
                    min_timeout_secs,
                    flash_timeout_rate_mb_per_second,
                )
                .await?
            }
        }
    }
    Ok(())
}

pub async fn flash<F, Part, P, T>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    product: &P,
    fastboot_interface: &mut T,
    cmd: ManifestParams,
    ssh_key_upload_method: Option<&UploadMethod>,
) -> Result<()>
where
    F: FileResolver + Sync,
    Part: Partition,
    P: Product<Part>,
    T: FastbootInterface,
{
    messenger
        .send(Event::FlashProduct {
            product_name: product.name().clone(),
            partition_count: product.bootloader_partitions().len() + product.partitions().len(),
        })
        .await?;
    flash_bootloader(messenger, file_resolver, product, fastboot_interface, &cmd).await?;
    flash_product(
        messenger,
        file_resolver,
        product,
        fastboot_interface,
        &cmd,
        ssh_key_upload_method,
    )
    .await
}

pub async fn is_userspace_fastboot(
    fastboot_interface: &mut impl FastbootInterface,
) -> Result<bool> {
    match fastboot_interface.get_var(IS_USERSPACE_VAR).await {
        Ok(rev) => Ok(rev == "yes"),
        _ => Ok(false),
    }
}

pub async fn flash_bootloader<F, Part, P, T>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    product: &P,
    fastboot_interface: &mut T,
    cmd: &ManifestParams,
) -> Result<()>
where
    F: FileResolver + Sync,
    Part: Partition,
    P: Product<Part>,
    T: FastbootInterface,
{
    flash_partitions(
        messenger,
        file_resolver,
        product.bootloader_partitions(),
        fastboot_interface,
        cmd.flash_min_timeout_seconds,
        cmd.flash_timeout_rate_mb_per_second,
    )
    .await?;
    if product.bootloader_partitions().len() > 0
        && !cmd.no_bootloader_reboot
        && !is_userspace_fastboot(fastboot_interface).await?
    {
        set_slot_a_active(fastboot_interface).await?;
        reboot_bootloader(messenger, fastboot_interface).await?;
    }
    Ok(())
}

pub async fn flash_product<F, Part, P, T>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    product: &P,
    fastboot_interface: &mut T,
    cmd: &ManifestParams,
    ssh_key_upload_method: Option<&UploadMethod>,
) -> Result<()>
where
    F: FileResolver + Sync,
    Part: Partition,
    P: Product<Part>,
    T: FastbootInterface,
{
    flash_partitions(
        &messenger,
        file_resolver,
        product.partitions(),
        fastboot_interface,
        cmd.flash_min_timeout_seconds,
        cmd.flash_timeout_rate_mb_per_second,
    )
    .await?;
    if !cmd.no_bootloader_reboot && is_userspace_fastboot(fastboot_interface).await? {
        reboot_bootloader(messenger, fastboot_interface).await?;
    }

    // Upload OEM files from the commandline.
    stage_oem_files(messenger, file_resolver, false, &cmd.oem_stage, fastboot_interface).await?;

    // Upload the SSH key OEM file.
    if let Some(ssh_key) = &cmd.ssh_key {
        // Upload the SSH keys using the mechanism indicated by the product being flashed.
        let default_method = UploadMethod::Staged { command: SSH_OEM_COMMAND.to_string() };
        let method = ssh_key_upload_method.unwrap_or(&default_method);
        upload_file(messenger, file_resolver, false, ssh_key, method, fastboot_interface).await?;
    }

    // Upload the OEM files from the product.
    stage_oem_files(messenger, file_resolver, true, product.oem_files(), fastboot_interface).await
}

pub async fn flash_and_reboot<F, Part, P, T>(
    messenger: &Sender<Event>,
    file_resolver: &mut F,
    product: &P,
    fastboot_interface: &mut T,
    cmd: ManifestParams,
    ssh_key_upload_method: Option<&UploadMethod>,
) -> Result<()>
where
    F: FileResolver + Sync,
    Part: Partition,
    P: Product<Part>,
    T: FastbootInterface,
{
    flash(messenger, file_resolver, product, fastboot_interface, cmd, ssh_key_upload_method)
        .await?;
    finish(fastboot_interface).await
}

pub async fn finish<F: FastbootInterface>(fastboot_interface: &mut F) -> Result<()> {
    set_slot_a_active(fastboot_interface).await?;
    // LINT.IfChange
    fastboot_interface.continue_boot().await.map_err(FfxFastbootError::ContinueBootFailed)?;
    // LINT.ThenChange(//tools/lib/ffxutil/flash.go)
    Ok(())
}

pub async fn is_locked(fastboot_interface: &mut impl FastbootInterface) -> Result<bool> {
    verify_variable_value(LOCKED_VAR, "no", fastboot_interface).await.map(|l| !l)
}

pub async fn lock_device(fastboot_interface: &mut impl FastbootInterface) -> Result<()> {
    fastboot_interface.oem(LOCK_COMMAND).await?;
    Ok(())
}

pub async fn from_manifest<C, F>(
    context: &EnvironmentContext,
    messenger: Sender<Event>,
    input: C,
    fastboot_interface: &mut F,
) -> Result<()>
where
    C: Into<ManifestParams>,
    F: FastbootInterface,
{
    let cmd: ManifestParams = input.into();
    match &cmd.manifest {
        Some(manifest) => {
            if !manifest.is_file() {
                return Err(FfxFastbootError::ManifestNotAFile { path: manifest.to_path_buf() });
            }
            from_path(&messenger, manifest.to_path_buf(), fastboot_interface, cmd).await
        }
        None => {
            if let Some(path) = cmd.product_bundle.as_ref().filter(|s| is_local_product_bundle(s)) {
                from_local_product_bundle(
                    &messenger,
                    PathBuf::from(&*path),
                    fastboot_interface,
                    cmd,
                )
                .await
            } else {
                let sdk = context.get_sdk()?;
                let mut path = sdk.get_path_prefix().to_path_buf();
                path.push("flash.json"); // Not actually used, placeholder value needed.
                match sdk.get_version() {
                    SdkVersion::InTree => {
                        from_in_tree(context, &messenger, fastboot_interface, cmd).await
                    }
                    SdkVersion::Version(_) => {
                        from_sdk(context, &messenger, fastboot_interface, cmd).await
                    }
                    _ => Err(FfxFastbootError::UnknownSdkType),
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::file_resolver::test::TestResolver;
    use fastboot::reply::Reply;
    use fastboot::test_transport::TestTransport;
    use ffx_fastboot_interface::fastboot_proxy::FastbootProxy;
    use ffx_fastboot_interface::interface_factory::{
        InterfaceFactory, InterfaceFactoryBase, InterfaceFactoryError,
    };
    use ffx_fastboot_interface::test::setup;
    use ffx_fastboot_interface::util::multi_chain;
    use ffx_flash_manifest::v2::FlashManifest;
    use ffx_flash_manifest::{BootParams, Command};
    use serde_json::{from_str, json};
    use std::io::{Seek, SeekFrom, Write};
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;

    #[derive(Default, Debug, Clone)]
    struct TestTransportFactory {}

    #[async_trait]
    impl InterfaceFactoryBase<TestTransport> for TestTransportFactory {
        async fn open(&mut self) -> std::result::Result<TestTransport, InterfaceFactoryError> {
            Ok(TestTransport::new())
        }

        async fn close(&self) {}

        async fn rediscover(&mut self) -> std::result::Result<(), InterfaceFactoryError> {
            Ok(())
        }
    }

    impl InterfaceFactory<TestTransport> for TestTransportFactory {}

    /// Runs a fastboot sequence of uploading a file via inline upload.
    ///
    /// # Arguments
    ///
    /// * `data`: the data to upload
    /// * `method`: [UploadMethod] parameters
    ///
    /// # Returns
    ///
    /// The resulting list of commands issued to the device, or `Err` if [upload_file] failed.
    async fn run_upload_file(data: &[u8], method: UploadMethod) -> Result<Vec<String>> {
        let (state, mut mock) = setup();

        let data_file = NamedTempFile::new().unwrap();
        std::fs::write(data_file.path(), data).unwrap();

        let (messenger, _receiver) = mpsc::channel(100);

        upload_file(
            &messenger,
            &mut TestResolver::new(),
            false,
            &data_file.path().to_string_lossy(),
            &method,
            &mut mock,
        )
        .await?;

        let state = state.lock().unwrap();
        Ok(state.oem_commands.clone())
    }

    #[fuchsia::test]
    async fn upload_inline() {
        let cmds = run_upload_file(
            b"foo bar baz",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                command_max_length: 24,
                init_command: Some("init-write".to_string()),
                finalize_command: Some("done-write".to_string()),
            },
        )
        .await
        .unwrap();

        // Base64("foo bar baz") == "Zm9vIGJhciBiYXo=" (16 bytes)
        // "oem write-chunk " command prefix is 16 bytes.
        // 24 max length - 16 prefix = 8 data per command, so we fill exactly 2 commands.
        assert_eq!(
            cmds,
            [
                "oem init-write",
                "oem write-chunk Zm9vIGJh",
                "oem write-chunk ciBiYXo=",
                "oem done-write"
            ]
        );
    }

    #[fuchsia::test]
    async fn upload_inline_partial_command() {
        let cmds = run_upload_file(
            b"foo bar",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                command_max_length: 24,
                init_command: Some("init-write".to_string()),
                finalize_command: Some("done-write".to_string()),
            },
        )
        .await
        .unwrap();

        // Base64("foo bar") == "Zm9vIGJhcg==" (12 bytes)
        // "oem write-chunk " command prefix is 16 bytes.
        // 24 max length - 16 prefix = 8 data per command, so we send 1 full and 1 partial command.
        assert_eq!(
            cmds,
            [
                "oem init-write",
                "oem write-chunk Zm9vIGJh",
                "oem write-chunk cg==",
                "oem done-write"
            ]
        );
    }

    #[fuchsia::test]
    async fn upload_inline_no_init_or_finalize() {
        let cmds = run_upload_file(
            b"foo bar baz",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                command_max_length: 24,
                init_command: None,
                finalize_command: None,
            },
        )
        .await
        .unwrap();

        // Base64("foo bar baz") == "Zm9vIGJhciBiYXo=" (16 bytes)
        // "oem write-chunk " command prefix is 16 bytes.
        // 24 max length - 16 prefix = 8 data per command, so we fill exactly 2 commands.
        assert_eq!(cmds, ["oem write-chunk Zm9vIGJh", "oem write-chunk ciBiYXo=",]);
    }

    #[fuchsia::test]
    async fn upload_inline_single_chunk() {
        let cmds = run_upload_file(
            b"1234\nABCD",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                command_max_length: 64,
                init_command: Some("init-write".to_string()),
                finalize_command: None,
            },
        )
        .await
        .unwrap();

        // Base64("1234\nABCD") == "MTIzNApBQkNE"
        assert_eq!(cmds, ["oem init-write", "oem write-chunk MTIzNApBQkNE"]);
    }

    #[fuchsia::test]
    async fn upload_inline_overflow() {
        let err = run_upload_file(
            b"foo",
            UploadMethod::Inline {
                command_prefix: "write-chunk ".to_string(),
                // Command length is too short to fit the prefix with any data.
                command_max_length: 10,
                init_command: None,
                finalize_command: None,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, FfxFastbootError::InlineUploadOverflow { .. }));
    }

    #[fuchsia::test()]
    async fn test_streaming_flash_fails_cleanly() -> std::result::Result<(), anyhow::Error> {
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();

        let tmp_img_files = [(); 4].map(|_| NamedTempFile::new().expect("tmp access failed"));
        let partition_to_path = tmp_img_files
            .iter()
            .zip(["zircon_a", "zircon_b", "vbmeta_a", "vbmeta_b"].iter())
            .map(|(tmpfile, partition_name)| {
                [partition_name, tmpfile.path().to_str().expect("non-unicode tmp path")]
            })
            .collect::<Vec<[&str; 2]>>();

        let manifest = json!({
            "hw_revision": "fastboot",
            "products": [
                {
                    "name": "nostream",
                    "requires_unlock": false,
                    "bootloader_partitions": [],
                    "partitions": partition_to_path.as_slice(),
                    "oem_files": []
                }
            ],
        });

        let v: FlashManifest = from_str(&manifest.to_string())?;
        let (state, mut proxy) = setup();
        {
            let mut state = state.lock().unwrap();
            state.set_multiple_vars([
                (IS_USERSPACE_VAR, "no"),
                (REVISION_VAR, "fastboot"),
                (MAX_DOWNLOAD_SIZE_VAR, "8192"),
            ]);
        }
        let (client, mut server) = mpsc::channel(100);
        v.flash(
            &client,
            &mut TestResolver::new(),
            &mut proxy,
            ManifestParams {
                manifest: Some(PathBuf::from(tmp_file_name)),
                product: "nostream".to_string(),
                op: Command::Boot(BootParams { zbi: None, vbmeta: None, slot: "a".to_string() }),
                ..Default::default()
            },
            None,
        )
        .await?;
        let s = state.lock().unwrap();

        // There are four partitions, and it's easier to check for each one.
        assert_eq!(s.get_var_call_count(STREAM_SEGMENT_SIZE), (false, 4));
        server.close();
        let mut messages = vec![];
        while let Some(m) = server.recv().await {
            // The duration uses a real clock, and refactoring to inject a
            // test clock would be a large change.
            if !matches!(&m, Event::FlashPartitionFinished { partition_name: _, duration: _ }) {
                messages.push(m);
            }
        }

        let expected = vec![
            Event::FlashProduct { product_name: "nostream".to_string(), partition_count: 4 },
            Event::Upload(UploadProgress::OnReady { partition: "zircon_a".to_string(), files: 1 }),
            Event::Upload(UploadProgress::OnStarted { size: 1 }),
            Event::Upload(UploadProgress::OnProgress { bytes_written: 1 }),
            Event::Upload(UploadProgress::OnFinished),
            Event::FlashPartition { partition_name: "zircon_a".to_string() },
            Event::Upload(UploadProgress::OnReady { partition: "zircon_b".to_string(), files: 1 }),
            Event::Upload(UploadProgress::OnStarted { size: 1 }),
            Event::Upload(UploadProgress::OnProgress { bytes_written: 1 }),
            Event::Upload(UploadProgress::OnFinished),
            Event::FlashPartition { partition_name: "zircon_b".to_string() },
            Event::Upload(UploadProgress::OnReady { partition: "vbmeta_a".to_string(), files: 1 }),
            Event::Upload(UploadProgress::OnStarted { size: 1 }),
            Event::Upload(UploadProgress::OnProgress { bytes_written: 1 }),
            Event::Upload(UploadProgress::OnFinished),
            Event::FlashPartition { partition_name: "vbmeta_a".to_string() },
            Event::Upload(UploadProgress::OnReady { partition: "vbmeta_b".to_string(), files: 1 }),
            Event::Upload(UploadProgress::OnStarted { size: 1 }),
            Event::Upload(UploadProgress::OnProgress { bytes_written: 1 }),
            Event::Upload(UploadProgress::OnFinished),
            Event::FlashPartition { partition_name: "vbmeta_b".to_string() },
        ];
        assert_eq!(expected, messages);

        Ok(())
    }

    #[fuchsia::test]
    async fn test_stream_flash() -> Result<()> {
        const SEGMENT_SIZE_BYTES: usize = 0x1000;

        let fill_zero = std::iter::repeat(0u8).take(SEGMENT_SIZE_BYTES * 4);
        let fill_data = (0u8..=255).cycle().take(SEGMENT_SIZE_BYTES * 4);
        let buf = multi_chain!(fill_data.clone(), fill_zero.clone(), fill_data.clone())
            .collect::<Vec<_>>();

        let (mut file, temp_path) = NamedTempFile::new().unwrap().into_parts();
        file.write_all(buf.as_slice()).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.flush().unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Okay("0x1000".to_owned()),    // Stream segment size
            Reply::Okay("0x2000".to_owned()),    // Partition zircon_a start
            Reply::Okay("0x1000000".to_owned()), // Partition zircon_a size
            Reply::Okay("0x4000".to_owned()),    // Max download size
            Reply::Data(0x4000),                 // Download request
            Reply::Okay("".to_owned()),          // Download
            Reply::Okay("".to_owned()),          // Stream flash
            Reply::Okay("".to_owned()),          // Stream fill
            Reply::Data(0x4000),                 // Download request
            Reply::Okay("".to_owned()),          // Download
            Reply::Okay("".to_owned()),          // Stream flash
        ]);

        let mut fastboot_client = FastbootProxy::<TestTransport>::new(
            "stream".to_string(),
            test_transport,
            TestTransportFactory {},
        );

        let (var_client, mut var_server): (Sender<Event>, Receiver<Event>) = mpsc::channel(3);
        use Event::*;
        use UploadProgress::*;

        let mut server_actual = vec![];
        let mut server_task = async || {
            while let Some(m) = var_server.recv().await {
                let m = if let FlashPartitionFinished { partition_name, duration: _ } = m {
                    // Cheat to avoid massive refactor around passing a test clock
                    FlashPartitionFinished { partition_name, duration: Duration::seconds(0) }
                } else {
                    m
                };

                server_actual.push(m);
            }
            Ok(())
        };

        let mut resolver = TestResolver::new();
        let flash_partition_task = flash_partition(
            var_client,
            &mut resolver,
            "zircon_a",
            temp_path.to_str().unwrap(),
            &mut fastboot_client,
            360,
            1000.0,
        );

        try_join!(flash_partition_task, server_task())?;

        let server_expected = &[
            Upload(OnReady { partition: "zircon_a".to_owned(), files: 1 }),
            Upload(OnStarted { size: 0xc000 }),
            Upload(OnProgress { bytes_written: 0x4000 }),
            Upload(OnProgress { bytes_written: 0x8000 }),
            Upload(OnProgress { bytes_written: 0xC000 }),
            Upload(OnFinished),
            FlashPartitionFinished {
                partition_name: "zircon_a".to_owned(),
                duration: Duration::seconds(0),
            },
        ];

        assert_eq!(&server_actual, server_expected);
        Ok(())
    }
}
