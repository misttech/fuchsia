// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use async_trait::async_trait;
use ffx_package_ota_manifest_show_args::ShowCommand;
use ffx_writer::{MachineWriter, ToolIO as _};
use fho::{Error, FfxMain, FfxTool, Result};
use prettytable::format::FormatBuilder;
use prettytable::{Table, cell, row};
use ring::signature::UnparsedPublicKey;
use serde::Serialize;
use update_package::UpdateMode;
use update_package::manifest::{Blob, Image, ImageType, Slot};

#[derive(Serialize)]
pub struct OtaManifestOutput {
    manifest_version: u32,
    signatures: Vec<String>,
    build_info_version: String,
    board: String,
    epoch: u64,
    mode: String,
    blob_base_url: String,
    images: Vec<ImageOutput>,
    blobs: Vec<BlobOutput>,
}

#[derive(Serialize)]
struct ImageOutput {
    slot: String,
    image_type: String,
    sha256: String,
    blob: BlobOutput,
}

#[derive(Serialize)]
struct BlobOutput {
    uncompressed_size: u64,
    fuchsia_merkle_root: String,
}

impl OtaManifestOutput {
    fn new(
        raw: update_package::signed_manifest::RawManifest<'_>,
    ) -> Result<Self, update_package::manifest::OtaManifestError> {
        let manifest = update_package::manifest::parse_ota_manifest(raw.manifest_payload)?;
        Ok(Self {
            manifest_version: raw.version,
            signatures: raw.signatures.into_iter().map(hex::encode).collect(),
            build_info_version: manifest.build_info_version.to_string(),
            board: manifest.board,
            epoch: manifest.epoch,
            mode: match manifest.mode {
                UpdateMode::Normal => "Normal".to_string(),
                UpdateMode::ForceRecovery => "ForceRecovery".to_string(),
            },
            blob_base_url: manifest.blob_base_url,
            images: manifest.images.into_iter().map(Into::into).collect(),
            blobs: manifest.blobs.into_iter().map(Into::into).collect(),
        })
    }
}

impl From<Image> for ImageOutput {
    fn from(image: Image) -> Self {
        Self {
            slot: match image.slot {
                Slot::AB => "A/B".to_string(),
                Slot::R => "Recovery".to_string(),
            },
            image_type: match image.image_type {
                ImageType::Asset(ref asset) => format!("Asset({:?})", asset),
                ImageType::Firmware(ref fw) => format!("Firmware({})", fw),
            },
            sha256: image.sha256.to_string(),
            blob: image.blob.into(),
        }
    }
}

impl From<Blob> for BlobOutput {
    fn from(blob: Blob) -> Self {
        Self {
            uncompressed_size: blob.uncompressed_size,
            fuchsia_merkle_root: blob.fuchsia_merkle_root.to_string(),
        }
    }
}

#[derive(FfxTool)]
pub struct ShowTool {
    #[command]
    cmd: ShowCommand,
}

fho::embedded_plugin!(ShowTool);

#[async_trait(?Send)]
impl FfxMain for ShowTool {
    type Writer = MachineWriter<OtaManifestOutput>;

    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        let manifest_bytes = std::fs::read(&self.cmd.manifest)
            .with_context(|| format!("Reading manifest file: {}", self.cmd.manifest))
            .map_err(|e| Error::User(e))?;

        let raw_manifest = update_package::signed_manifest::parse_raw(&manifest_bytes)
            .with_context(|| "Parsing signed manifest")
            .map_err(|e| Error::User(e))?;

        if let Some(key_path) = &self.cmd.public_key {
            let key_bytes = std::fs::read(key_path)
                .with_context(|| format!("Reading public key: {}", key_path))
                .map_err(|e| Error::User(e))?;
            let key = UnparsedPublicKey::new(&ring::signature::ED25519, key_bytes);
            raw_manifest
                .verify(&[key])
                .with_context(|| "Verifying signed manifest")
                .map_err(|e| Error::User(e))?;
        }

        let output = OtaManifestOutput::new(raw_manifest).map_err(|e| Error::User(e.into()))?;

        if writer.is_machine() {
            writer.machine(&output).map_err(|e| Error::Unexpected(e.into()))?;
        } else {
            write_table(&mut writer, &output, self.cmd.print_blobs)
                .map_err(|e| Error::Unexpected(e.into()))?;
        }

        Ok(())
    }
}

fn write_table(
    writer: &mut impl std::io::Write,
    output: &OtaManifestOutput,
    show_blobs: bool,
) -> std::io::Result<()> {
    writeln!(writer, "Manifest Version: {}", output.manifest_version)?;
    for (i, sig) in output.signatures.iter().enumerate() {
        writeln!(writer, "Signature {}: {}", i, sig)?;
    }
    writeln!(writer, "Build Info Version: {}", output.build_info_version)?;
    writeln!(writer, "Board: {}", output.board)?;
    writeln!(writer, "Epoch: {}", output.epoch)?;
    writeln!(writer, "Mode: {}", output.mode)?;
    writeln!(writer, "Blob Base URL: {}", output.blob_base_url)?;

    let total_image_size: u64 = output.images.iter().map(|img| img.blob.uncompressed_size).sum();
    writeln!(
        writer,
        "Images ({} items, total uncompressed size: {} bytes):",
        output.images.len(),
        total_image_size
    )?;
    let format = FormatBuilder::new().column_separator(' ').padding(1, 0).build();
    if !output.images.is_empty() {
        let mut table = Table::new();
        table.set_format(format);
        table.set_titles(row!["TYPE", "SLOT", "MERKLE", "SIZE"]);
        for image in &output.images {
            table.add_row(row![
                image.image_type,
                image.slot,
                image.blob.fuchsia_merkle_root,
                image.blob.uncompressed_size
            ]);
        }
        writeln!(writer, "{table}")?;
    }

    let total_blob_size: u64 = output.blobs.iter().map(|b| b.uncompressed_size).sum();
    writeln!(
        writer,
        "Blobs ({} items, total uncompressed size: {} bytes):",
        output.blobs.len(),
        total_blob_size
    )?;
    if show_blobs {
        if !output.blobs.is_empty() {
            let mut table = Table::new();
            table.set_format(format);
            table.set_titles(row!["MERKLE", "SIZE"]);
            for blob in &output.blobs {
                table.add_row(row![blob.fuchsia_merkle_root, blob.uncompressed_size]);
            }
            writeln!(writer, "{table}")?;
        }
    } else {
        writeln!(writer, "(omitted, use --blobs to show)")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_writer::{Format, TestBuffers};
    use std::io::Write as _;
    use std::str::FromStr;
    use tempfile::NamedTempFile;
    use update_package::SystemVersion;
    use update_package::manifest::OtaManifest;

    fn make_ota_manifest() -> OtaManifest {
        OtaManifest {
            build_info_version: SystemVersion::from_str("1.2.3.4").unwrap(),
            board: "test-board".to_string(),
            epoch: 1,
            mode: UpdateMode::Normal,
            blob_base_url: "http://example.com".to_string(),
            images: vec![
                Image {
                    slot: Slot::AB,
                    image_type: ImageType::Firmware("test-fw".to_string()),
                    sha256: "0".repeat(64).parse().unwrap(),
                    blob: Blob {
                        uncompressed_size: 1234,
                        fuchsia_merkle_root: "1".repeat(64).parse().unwrap(),
                    },
                },
                Image {
                    slot: Slot::R,
                    image_type: ImageType::Asset(update_package::manifest::AssetType::Zbi),
                    sha256: "a".repeat(64).parse().unwrap(),
                    blob: Blob {
                        uncompressed_size: 9999,
                        fuchsia_merkle_root: "b".repeat(64).parse().unwrap(),
                    },
                },
            ],
            blobs: vec![
                Blob {
                    uncompressed_size: 5678,
                    fuchsia_merkle_root: "2".repeat(64).parse().unwrap(),
                },
                Blob {
                    uncompressed_size: 12345,
                    fuchsia_merkle_root: "c".repeat(64).parse().unwrap(),
                },
            ],
        }
    }

    fn make_keypair() -> ring::signature::Ed25519KeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    #[fuchsia::test]
    async fn test_show_json() {
        use serde_json::json;

        let keypair = make_keypair();
        let manifest = make_ota_manifest();
        let bytes = update_package::signed_manifest::generate(manifest, &keypair).unwrap();

        let raw_manifest = update_package::signed_manifest::parse_raw(&bytes).unwrap();
        let expected_sig = hex::encode(&raw_manifest.signatures[0]);

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        let manifest_path = camino::Utf8PathBuf::try_from(file.path().to_path_buf()).unwrap();

        let cmd = ShowCommand { manifest: manifest_path, public_key: None, print_blobs: false };
        let tool = ShowTool { cmd };
        let buffers = TestBuffers::default();
        let writer = <ShowTool as FfxMain>::Writer::new_test(Some(Format::Json), &buffers);

        tool.main(writer).await.unwrap();

        let (out, err) = buffers.into_strings();
        assert_eq!(err, "");

        let out_json: serde_json::Value = serde_json::from_str(&out).expect("valid JSON output");
        let expected_json = json!({
            "manifest_version": 1,
            "signatures": [expected_sig],
            "build_info_version": "1.2.3.4",
            "board": "test-board",
            "epoch": 1,
            "mode": "Normal",
            "blob_base_url": "http://example.com",
            "images": [
                {
                    "slot": "A/B",
                    "image_type": "Firmware(test-fw)",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                    "blob": {
                        "uncompressed_size": 1234,
                        "fuchsia_merkle_root": "1111111111111111111111111111111111111111111111111111111111111111"
                    }
                },
                {
                    "slot": "Recovery",
                    "image_type": "Asset(Zbi)",
                    "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "blob": {
                        "uncompressed_size": 9999,
                        "fuchsia_merkle_root": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    }
                }
            ],
            "blobs": [
                {
                    "uncompressed_size": 5678,
                    "fuchsia_merkle_root": "2222222222222222222222222222222222222222222222222222222222222222"
                },
                {
                    "uncompressed_size": 12345,
                    "fuchsia_merkle_root": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                }
            ]
        });
        assert_eq!(out_json, expected_json);
    }

    #[fuchsia::test]
    async fn test_show_table() {
        let keypair = make_keypair();
        let manifest = make_ota_manifest();
        let bytes = update_package::signed_manifest::generate(manifest, &keypair).unwrap();

        let raw_manifest = update_package::signed_manifest::parse_raw(&bytes).unwrap();
        let expected_sig = hex::encode(&raw_manifest.signatures[0]);

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        let manifest_path = camino::Utf8PathBuf::try_from(file.path().to_path_buf()).unwrap();

        let cmd = ShowCommand { manifest: manifest_path, public_key: None, print_blobs: true };
        let tool = ShowTool { cmd };
        let buffers = TestBuffers::default();
        let writer = <ShowTool as FfxMain>::Writer::new_test(None, &buffers);

        tool.main(writer).await.unwrap();

        let (out, err) = buffers.into_strings();
        assert_eq!(err, "");

        let expected_table = format!(
            r#"Manifest Version: 1
Signature 0: {expected_sig}
Build Info Version: 1.2.3.4
Board: test-board
Epoch: 1
Mode: Normal
Blob Base URL: http://example.com
Images (2 items, total uncompressed size: 11233 bytes):
 TYPE               SLOT      MERKLE                                                            SIZE
 Firmware(test-fw)  A/B       1111111111111111111111111111111111111111111111111111111111111111  1234
 Asset(Zbi)         Recovery  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  9999

Blobs (2 items, total uncompressed size: 18023 bytes):
 MERKLE                                                            SIZE
 2222222222222222222222222222222222222222222222222222222222222222  5678
 cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc  12345

"#,
            expected_sig = expected_sig
        );
        assert_eq!(out, expected_table);
    }
}
