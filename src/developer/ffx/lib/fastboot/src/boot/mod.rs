// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::common::stage_file;
use crate::file_resolver::FileResolver;
use anyhow::{anyhow, Result};
use byteorder::{ByteOrder, LittleEndian};
use ffx_fastboot_interface::fastboot_interface::{FastbootInterface, UploadProgress};
use std::fs::{metadata, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use tempfile::{tempdir, TempDir};
use tokio::sync::mpsc::Sender;

const PAGE_SIZE: u32 = 4096;
const BOOT_MAGIC: &str = "ANDROID!";
const BOOT_SIZE: usize = 8;
const V4_HEADER_SIZE: u32 = 1580;

fn copy<R: Read, W: Write>(mut reader: BufReader<R>, writer: &mut BufWriter<W>) -> Result<()> {
    loop {
        let buffer = reader.fill_buf()?;
        let length = buffer.len();
        if length == 0 {
            return Ok(());
        }
        writer.write_all(buffer)?;
        reader.consume(length);
    }
}

async fn get_boot_image<F: FileResolver + Sync>(
    file_resolver: &mut F,
    zbi: &String,
    vbmeta: &Option<String>,
    temp_dir: &TempDir,
) -> Result<PathBuf> {
    match vbmeta {
        None => {
            let mut path = PathBuf::new();
            path.push(file_resolver.get_file(&zbi).await?);
            Ok(path)
        }
        Some(v) => {
            // if vbmeta exists, concat the two into a single boot image file
            let zbi_path = file_resolver.get_file(&zbi).await?;
            let v_path = file_resolver.get_file(&v).await?;
            let mut path = PathBuf::new();
            path.push(temp_dir.path());
            path.push("boot_image.bin");
            let mut outfile = BufWriter::new(File::create(&path)?);
            let zbi_file = BufReader::new(File::open(&zbi_path)?);
            let vbmeta_file = BufReader::new(File::open(&v_path)?);
            copy(zbi_file, &mut outfile)?;
            outfile.flush()?;
            copy(vbmeta_file, &mut outfile)?;
            outfile.flush()?;
            Ok(path)
        }
    }
}

pub async fn boot<F: FileResolver + Sync, T: FastbootInterface>(
    messenger: Sender<UploadProgress>,
    file_resolver: &mut F,
    zbi: String,
    vbmeta: Option<String>,
    fastboot_interface: &mut T,
) -> Result<()> {
    let temp_dir = tempdir()?;
    let boot_image = get_boot_image(file_resolver, &zbi, &vbmeta, &temp_dir).await?;

    let page_mask: u32 = PAGE_SIZE - 1;
    let kernal_size: u32 = metadata(&boot_image)?.len().try_into()?;
    let kernal_actual: u32 = (kernal_size + page_mask) & (!page_mask);

    let mut path = PathBuf::new();
    path.push(temp_dir.path());
    path.push("bootimg.bin");

    let mut outfile = BufWriter::new(File::create(&path)?);

    let mut header: [u8; 4096] = [0u8; 4096];
    header[0..BOOT_SIZE].copy_from_slice(&BOOT_MAGIC.as_bytes()[..]);
    LittleEndian::write_u32(&mut header[BOOT_SIZE..BOOT_SIZE + 4], kernal_size);
    LittleEndian::write_u32(&mut header[BOOT_SIZE + 12..BOOT_SIZE + 16], V4_HEADER_SIZE);
    LittleEndian::write_u32(
        &mut header[BOOT_SIZE + 32..BOOT_SIZE + 36],
        4, /* header version*/
    );
    outfile.write_all(&header)?;

    let in_file = BufReader::new(File::open(&boot_image)?);
    copy(in_file, &mut outfile)?;

    // Pad to page size.
    let padding = kernal_actual - kernal_size;
    let padding_bytes: [u8; 4096] = [0u8; 4096];
    outfile.write_all(&padding_bytes[..padding.try_into()?])?;
    outfile.flush()?;

    stage_file(
        messenger,
        file_resolver,
        false, /* resolve */
        path.to_str().ok_or_else(|| anyhow!("Could not get temp boot image path"))?,
        fastboot_interface,
    )
    .await?;

    fastboot_interface.boot().await.map_err(|e| anyhow!("Fastboot error: {:?}", e))?;

    Ok(())
}
