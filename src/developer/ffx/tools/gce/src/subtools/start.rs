// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::StartCommand;
use discovery::gce_watcher;
use ffx_config::EnvironmentContext;
use ffx_gce::{
    AttachedDisk, AttachedDiskInitializeParams, DEFAULT_GCE_DISK_SIZE, GceContext, GceTunnel,
    GceTunnelConfig, GceVmShape, GuestOsFeature, Image, Instance, Metadata, MetadataItem,
    NetworkInterface, RawDisk, StartResult, VbmetaKeys, compute_bundle_hash, gce_vm_shape,
    generate_instance_name, prepare_gce_disk_archive, product_bundle_architecture,
    read_gce_ssh_pubkey, resolve_config_string,
};
use ffx_writer::{MachineWriter, ToolIO as _};
use fho::{FfxMain, FfxTool, Result, return_user_error, user_error};
use prettytable::format::FormatBuilder;
use prettytable::{Table, row};
use std::io::Write;
use std::path::{Path, PathBuf};

const DEFAULT_BUCKET_SUFFIX: &str = "fuchsia-images";
const IMAGE_SERIAL_PREFIX: &str = "fuchsia-serial:";
const DEFAULT_DISK_SIZE_GB: i64 = (DEFAULT_GCE_DISK_SIZE / (1024 * 1024 * 1024)) as i64;
const DEFAULT_SERIAL_PORT: u32 = 1;

#[derive(Debug, FfxTool)]
pub struct StartTool {
    #[command]
    cmd: StartCommand,
    context: EnvironmentContext,
}

impl FfxMain for StartTool {
    type Writer = MachineWriter<StartResult>;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let instance_name = self
            .cmd
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(generate_instance_name);
        gce_watcher::Instance::validate_name(&instance_name).map_err(|e| user_error!("{e}"))?;

        let gce =
            GceContext::new(self.context.clone(), self.cmd.project.clone(), self.cmd.zone.clone())
                .await
                .map_err(|e| user_error!("{e}"))?;
        let instance = gce_watcher::Instance::new(&gce.project, &gce.zone, &instance_name)
            .map_err(|e| user_error!("{e}"))?;

        let (running_inst, instance_serial) = match gce
            .client
            .get_instance(&instance.project, &instance.zone, &instance.name)
            .await
            .map_err(|e| user_error!("Failed to query GCE instance '{}': {e}", instance.name))?
        {
            Some(inst) => {
                let existing_serial = extract_instance_serial(&inst);
                match inst.status.as_deref() {
                    Some("RUNNING") => {
                        writeln!(
                            writer.stderr(),
                            "Instance '{}' is already running in zone '{}'.",
                            instance.name,
                            instance.zone
                        )?;
                        (inst, existing_serial)
                    }
                    Some("TERMINATED" | "STOPPED") => {
                        writeln!(
                            writer.stderr(),
                            "Starting existing stopped instance '{}' in zone '{}'...",
                            instance.name,
                            instance.zone
                        )?;
                        let op = gce
                            .client
                            .start_instance(&instance.project, &instance.zone, &instance.name)
                            .await
                            .map_err(|e| {
                                user_error!("Failed to start instance '{}': {e}", instance.name)
                            })?;
                        gce.client
                            .wait_for_zone_operation(&instance.project, &instance.zone, &op)
                            .await
                            .map_err(|e| {
                                user_error!(
                                    "Failed waiting for instance '{}' to start: {e}",
                                    instance.name
                                )
                            })?;
                        let updated = gce
                            .client
                            .get_instance(&instance.project, &instance.zone, &instance.name)
                            .await
                            .map_err(|e| {
                                user_error!(
                                    "Failed to query started instance '{}': {e}",
                                    instance.name
                                )
                            })?
                            .ok_or_else(|| {
                                user_error!(
                                    "Instance '{}' was not found after starting",
                                    instance.name
                                )
                            })?;
                        (updated, existing_serial)
                    }
                    Some(other) => {
                        return_user_error!(
                            "Instance '{}' is in transitional state '{}'. Wait a few seconds and \
                             run `ffx gce start` again, or run `ffx gce stop {}` to delete it and \
                             start over.",
                            instance.name,
                            other,
                            instance.name
                        );
                    }
                    None => {
                        return_user_error!(
                            "GCE did not report a status for instance '{}'. Check it with \
                             `ffx gce show {}`.",
                            instance.name,
                            instance.name
                        );
                    }
                }
            }
            None => self.create_new_instance(&gce, &instance, &mut writer).await?,
        };

        writeln!(
            writer.stderr(),
            "Establishing local SSH tunnel to '{}' ({})...",
            instance.name,
            instance.zone
        )?;
        let mut tunnel_config =
            GceTunnelConfig::new(&self.context, &instance.project, &instance.zone, &instance.name)
                .map_err(|e| user_error!("{e}"))?;
        if let Some(serial) = instance_serial {
            tunnel_config = tunnel_config.with_serial_number(serial);
        }

        let tunnel_data = GceTunnel::start_tunnel_with_config(&self.context, tunnel_config)
            .await
            .map_err(|e| user_error!("{e}"))?;

        let result = StartResult {
            name: instance.name.clone(),
            project: instance.project.clone(),
            zone: instance.zone.clone(),
            status: running_inst.status.clone().unwrap_or_else(|| "RUNNING".to_string()),
            internal_ip: running_inst.internal_ip().map(str::to_owned),
            external_ip: running_inst.external_ip().map(str::to_owned),
            ssh_port: Some(tunnel_data.ssh_port),
        };

        output_start_result(&result, &mut writer)?;
        writer.flush()?;

        if self.cmd.serial {
            writeln!(
                writer.stderr(),
                "Streaming serial console output for '{}' (Ctrl-C to exit)...",
                instance.name
            )?;
            super::serial::stream_serial_output(
                &gce,
                &instance,
                DEFAULT_SERIAL_PORT,
                None,
                true,
                &mut writer,
            )
            .await?;
        }

        Ok(())
    }
}

impl StartTool {
    async fn create_new_instance(
        &self,
        gce: &GceContext,
        instance: &gce_watcher::Instance,
        writer: &mut MachineWriter<StartResult>,
    ) -> Result<(Instance, Option<String>)> {
        let pb_path =
            resolve_product_bundle(&self.context, self.cmd.product_bundle.as_deref()).await?;
        let vbmeta_keys = resolve_vbmeta_keys(
            &self.context,
            self.cmd.vbmeta_key.clone(),
            self.cmd.vbmeta_metadata.clone(),
        )?;
        let bucket = gce.resolve_bucket(self.cmd.bucket.as_deref(), DEFAULT_BUCKET_SUFFIX);

        // The VM shape has to match the image, so resolve the architecture before either the
        // image or the instance is created.
        let arch = product_bundle_architecture(&pb_path)
            .map_err(|e| user_error!("Failed to determine product bundle architecture: {e}"))?;
        let shape = gce_vm_shape(arch).map_err(|e| user_error!("{e}"))?;
        let machine_type =
            resolve_machine_type(&self.context, self.cmd.machine_type.as_deref(), shape);

        // The key is baked into the image and set as instance metadata, so without it the VM
        // would boot but never be reachable over SSH.
        let ssh_pubkey = read_gce_ssh_pubkey(&self.context)
            .map_err(|e| user_error!("Failed to load SSH public key: {e}"))?
            .ok_or_else(|| {
                user_error!(
                    "No SSH public key is configured, so the GCE VM would be unreachable. Run \
                     `ffx config check-ssh-keys --create`."
                )
            })?;

        let bundle_hash = compute_bundle_hash(&pb_path, &ssh_pubkey, vbmeta_keys.as_ref())
            .map_err(|e| user_error!("Failed to compute product bundle hash: {e}"))?;
        let image_name = format!("fuchsia-image-{bundle_hash}");

        let image_serial = match gce
            .client
            .get_image(&instance.project, &image_name)
            .await
            .map_err(|e| user_error!("Failed to query GCE image '{image_name}': {e}"))?
        {
            Some(existing_img) => {
                let existing_serial =
                    extract_serial_from_description(existing_img.description.as_deref());
                if self.cmd.reuse_image || existing_serial.is_some() {
                    writeln!(
                        writer.stderr(),
                        "Reusing existing GCE image '{image_name}' in project '{}'...",
                        instance.project
                    )?;
                    existing_serial
                } else {
                    writeln!(
                        writer.stderr(),
                        "Replacing legacy GCE image '{image_name}' without serial metadata..."
                    )?;
                    let del_op =
                        gce.client.delete_image(&instance.project, &image_name).await.map_err(
                            |e| user_error!("Failed to delete legacy image '{image_name}': {e}"),
                        )?;
                    gce.client
                        .wait_for_global_operation(&instance.project, &del_op)
                        .await
                        .map_err(|e| {
                            user_error!(
                                "Failed waiting for legacy image '{image_name}' deletion: {e}"
                            )
                        })?;
                    Some(
                        self.build_and_register_image(
                            gce,
                            instance,
                            &pb_path,
                            vbmeta_keys.as_ref(),
                            &bucket,
                            &image_name,
                            shape.image_architecture,
                            writer,
                        )
                        .await?,
                    )
                }
            }
            None => Some(
                self.build_and_register_image(
                    gce,
                    instance,
                    &pb_path,
                    vbmeta_keys.as_ref(),
                    &bucket,
                    &image_name,
                    shape.image_architecture,
                    writer,
                )
                .await?,
            ),
        };

        gce.client
            .ensure_ssh_firewall_rule(&instance.project, "default", writer.stderr())
            .await
            .map_err(|e| user_error!("Failed to configure SSH firewall rule: {e}"))?;

        let mut metadata_items = vec![
            MetadataItem { key: "serial-port-enable".to_string(), value: "true".to_string() },
            MetadataItem { key: "ssh-keys".to_string(), value: format!("fuchsia:{ssh_pubkey}") },
        ];
        if let Some(ref serial) = image_serial {
            metadata_items
                .push(MetadataItem { key: "fuchsia-serial".to_string(), value: serial.clone() });
        }

        let new_instance = Instance {
            name: Some(instance.name.clone()),
            machine_type: Some(format!("zones/{}/machineTypes/{machine_type}", instance.zone)),
            disks: vec![AttachedDisk {
                boot: true,
                auto_delete: true,
                initialize_params: Some(AttachedDiskInitializeParams {
                    source_image: Some(format!(
                        "projects/{}/global/images/{image_name}",
                        instance.project
                    )),
                    disk_size_gb: Some(DEFAULT_DISK_SIZE_GB),
                }),
                interface: None,
            }],
            network_interfaces: vec![NetworkInterface {
                network: Some("global/networks/default".to_string()),
                network_ip: None,
                access_configs: Vec::new(),
                nic_type: Some(shape.nic_type.to_string()),
            }],
            metadata: Some(Metadata { items: metadata_items }),
            ..Default::default()
        };

        writeln!(
            writer.stderr(),
            "Creating GCE instance '{}' ({machine_type}) in zone '{}'...",
            instance.name,
            instance.zone
        )?;
        let insert_op = gce
            .client
            .insert_instance(&instance.project, &instance.zone, &new_instance)
            .await
            .map_err(|e| {
                // Not every machine type is offered in every zone, which is especially easy to
                // hit with the arm64 default.
                user_error!(
                    "Failed to create GCE instance '{}': {e}\nCheck that '{machine_type}' is \
                     offered there with `gcloud compute machine-types list --project={} \
                     --zones={} --filter=\"name={machine_type}\"`.",
                    instance.name,
                    instance.project,
                    instance.zone
                )
            })?;
        gce.client
            .wait_for_zone_operation(&instance.project, &instance.zone, &insert_op)
            .await
            .map_err(|e| {
                user_error!("Failed waiting for GCE instance '{}' creation: {e}", instance.name)
            })?;

        let created = gce
            .client
            .get_instance(&instance.project, &instance.zone, &instance.name)
            .await
            .map_err(|e| {
                user_error!("Failed to fetch created GCE instance '{}': {e}", instance.name)
            })?
            .ok_or_else(|| user_error!("Created GCE instance '{}' was not found", instance.name))?;
        Ok((created, image_serial))
    }

    async fn build_and_register_image(
        &self,
        gce: &GceContext,
        instance: &gce_watcher::Instance,
        pb_path: &std::path::Path,
        vbmeta_keys: Option<&VbmetaKeys>,
        bucket: &str,
        image_name: &str,
        image_arch: &str,
        writer: &mut MachineWriter<StartResult>,
    ) -> Result<String> {
        let archive_dir = tempfile::tempdir().map_err(|e| {
            user_error!("Failed to create temporary directory for image archive: {e}")
        })?;
        let tar_gz_path = archive_dir.path().join("disk.tar.gz");

        writeln!(
            writer.stderr(),
            "Synthesizing UEFI GPT disk image from '{}'...",
            pb_path.display()
        )?;
        let serial = prepare_gce_disk_archive(&self.context, pb_path, vbmeta_keys, &tar_gz_path)
            .map_err(|e| user_error!("Failed to prepare GCE disk archive: {e}"))?;

        let object_name = format!("{image_name}.tar.gz");
        writeln!(writer.stderr(), "Uploading disk archive to gs://{bucket}/{object_name}...")?;
        gce.client.ensure_bucket(&instance.project, bucket).await.map_err(|e| {
            user_error!(
                "Failed to ensure GCS bucket '{bucket}' in project '{}': {e}",
                instance.project
            )
        })?;
        let mut active_bucket = bucket;
        if let Err(err) = gce.client.upload_gcs_file(bucket, &object_name, &tar_gz_path).await {
            if !err.is_forbidden_or_not_found()
                || gce.has_explicit_bucket(self.cmd.bucket.as_deref())
                || bucket == instance.project
            {
                return Err(user_error!(
                    "Failed to upload disk image to gs://{bucket}/{object_name}: {err}"
                ));
            }
            let fallback = instance.project.as_str();
            writeln!(
                writer.stderr(),
                "Bucket 'gs://{bucket}' is unavailable; \
                 falling back to gs://{fallback}/{object_name}..."
            )?;
            gce.client.ensure_bucket(&instance.project, fallback).await.map_err(|e| {
                user_error!(
                    "Failed to ensure GCS bucket '{fallback}' in project '{}': {e}",
                    instance.project
                )
            })?;
            gce.client.upload_gcs_file(fallback, &object_name, &tar_gz_path).await.map_err(
                |e| {
                    user_error!("Failed to upload disk image to gs://{fallback}/{object_name}: {e}")
                },
            )?;
            active_bucket = fallback;
        }

        writeln!(writer.stderr(), "Registering GCE custom image '{image_name}'...")?;
        let image = Image {
            name: Some(image_name.to_string()),
            description: Some(format!("{IMAGE_SERIAL_PREFIX}{serial}")),
            architecture: Some(image_arch.to_string()),
            raw_disk: Some(RawDisk {
                source: format!("https://storage.googleapis.com/{active_bucket}/{object_name}"),
            }),
            guest_os_features: vec![
                GuestOsFeature { feature_type: "UEFI_COMPATIBLE".to_string() },
                GuestOsFeature { feature_type: "GVNIC".to_string() },
            ],
            ..Default::default()
        };
        let op = gce
            .client
            .insert_image(&instance.project, &image)
            .await
            .map_err(|e| user_error!("Failed to register GCE image '{image_name}': {e}"))?;
        gce.client.wait_for_global_operation(&instance.project, &op).await.map_err(|e| {
            user_error!("Failed waiting for GCE image '{image_name}' registration: {e}")
        })?;

        Ok(serial)
    }
}

fn resolve_machine_type(
    context: &EnvironmentContext,
    flag: Option<&str>,
    shape: GceVmShape,
) -> String {
    resolve_config_string(context, flag, "gce.machine_type")
        .unwrap_or_else(|| shape.default_machine_type.to_string())
}

async fn resolve_product_bundle(
    context: &EnvironmentContext,
    product_bundle_flag: Option<&Path>,
) -> Result<PathBuf> {
    let pb_str = match product_bundle_flag.filter(|p| !p.as_os_str().is_empty()) {
        Some(path) => path.to_string_lossy().to_string(),
        None => pbms::get_product_bundle_path(context)
            .map_err(|e| user_error!("Failed to read configured product bundle path: {e}"))?,
    };
    let loaded = pbms::load_product_bundle(context, &pb_str)
        .await
        .map_err(|e| user_error!("Error loading product bundle: {e}"))?;
    Ok(loaded.loaded_from_path().as_std_path().to_path_buf())
}

fn resolve_vbmeta_keys(
    context: &EnvironmentContext,
    vbmeta_key_flag: Option<PathBuf>,
    vbmeta_metadata_flag: Option<PathBuf>,
) -> Result<Option<VbmetaKeys>> {
    let non_empty = |p: PathBuf| (!p.as_os_str().is_empty()).then_some(p);

    let key = vbmeta_key_flag
        .and_then(non_empty)
        .or_else(|| context.get::<PathBuf, _>("gce.vbmeta.key").ok().and_then(non_empty));

    let metadata = vbmeta_metadata_flag
        .and_then(non_empty)
        .or_else(|| context.get::<PathBuf, _>("gce.vbmeta.metadata").ok().and_then(non_empty));

    match (key, metadata) {
        (Some(key), Some(metadata)) => {
            if !key.exists() {
                return_user_error!(
                    "VBMeta key file '{}' does not exist. Provide a valid path via `--vbmeta-key` or `ffx config set gce.vbmeta.key <path>`.",
                    key.display()
                );
            }
            if !metadata.exists() {
                return_user_error!(
                    "VBMeta metadata file '{}' does not exist. Provide a valid path via `--vbmeta-metadata` or `ffx config set gce.vbmeta.metadata <path>`.",
                    metadata.display()
                );
            }
            Ok(Some(VbmetaKeys { key, metadata }))
        }
        (Some(_), None) => return_user_error!(
            "VBMeta key was specified without VBMeta metadata. Provide `--vbmeta-metadata <path>` or configure `gce.vbmeta.metadata`."
        ),
        (None, Some(_)) => return_user_error!(
            "VBMeta metadata was specified without a VBMeta key. Provide `--vbmeta-key <path>` or configure `gce.vbmeta.key`."
        ),
        (None, None) => Ok(None),
    }
}

fn extract_instance_serial(inst: &Instance) -> Option<String> {
    inst.metadata
        .as_ref()?
        .items
        .iter()
        .find(|item| item.key == "fuchsia-serial")
        .map(|item| item.value.trim().to_string())
        .filter(|val| !val.is_empty())
}

fn extract_serial_from_description(description: Option<&str>) -> Option<String> {
    description?
        .trim()
        .strip_prefix(IMAGE_SERIAL_PREFIX)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn output_start_result(res: &StartResult, writer: &mut MachineWriter<StartResult>) -> Result<()> {
    if writer.is_machine() {
        writer.machine(res)?;
    } else {
        writeln!(writer, "Started GCE instance:")?;
        let table_format = FormatBuilder::new().indent(2).padding(0, 2).build();
        let mut table = Table::new();
        table.set_format(table_format);
        table.add_row(row!["Name:", &res.name]);
        table.add_row(row!["Status:", &res.status]);
        table.add_row(row!["Zone:", &res.zone]);
        table.add_row(row!["Project:", &res.project]);
        if let Some(port) = res.ssh_port {
            table.add_row(row!["Local SSH Tunnel:", &format!("127.0.0.1:{port}")]);
        }
        table.add_row(row!["Internal IP:", res.internal_ip.as_deref().unwrap_or("-")]);
        table.add_row(row!["External IP:", res.external_ip.as_deref().unwrap_or("-")]);
        table.print(writer).map_err(|e| user_error!("{e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_writer::{Format, TestBuffers};

    #[fuchsia::test]
    fn test_resolve_machine_type() {
        let env = ffx_config::test_init().expect("test env");
        let arm64 = gce_vm_shape(sdk_metadata::CpuArchitecture::Arm64).expect("arm64 shape");
        assert_eq!(resolve_machine_type(&env.context, None, arm64), "t2a-standard-4");
        // An explicit machine type wins over the architecture default.
        assert_eq!(
            resolve_machine_type(&env.context, Some("e2-standard-8"), arm64),
            "e2-standard-8"
        );
    }

    #[fuchsia::test]
    fn test_resolve_vbmeta_keys() {
        let temp = tempfile::tempdir().unwrap();
        let env = ffx_config::test_init().expect("test env");
        assert!(resolve_vbmeta_keys(&env.context, None, None).unwrap().is_none());
        assert!(resolve_vbmeta_keys(&env.context, Some(temp.path().join("k")), None).is_err());
    }

    #[fuchsia::test]
    fn test_extract_instance_serial() {
        let inst = Instance {
            metadata: Some(Metadata {
                items: vec![MetadataItem {
                    key: "fuchsia-serial".to_string(),
                    value: "GC-1234".to_string(),
                }],
            }),
            ..Default::default()
        };
        assert_eq!(extract_instance_serial(&inst).as_deref(), Some("GC-1234"));
    }

    #[fuchsia::test]
    fn test_extract_serial_from_description() {
        assert_eq!(
            extract_serial_from_description(Some("fuchsia-serial:GC-ABCD")).as_deref(),
            Some("GC-ABCD")
        );
        assert_eq!(extract_serial_from_description(Some("other description")), None);
        assert_eq!(extract_serial_from_description(None), None);
    }

    #[fuchsia::test]
    fn test_output_start_result_machine_json() {
        let test_buffers = TestBuffers::default();
        let mut writer = MachineWriter::new_test(Some(Format::Json), &test_buffers);
        let res = StartResult {
            name: "fuchsia-gce".to_string(),
            project: "my-proj".to_string(),
            zone: "us-central1-a".to_string(),
            status: "RUNNING".to_string(),
            internal_ip: Some("10.128.0.5".to_string()),
            external_ip: Some("35.1.2.3".to_string()),
            ssh_port: Some(41234),
        };

        output_start_result(&res, &mut writer).unwrap();

        let (stdout, stderr) = test_buffers.into_strings();
        assert!(stderr.is_empty());
        let parsed: StartResult = serde_json::from_str(&stdout).expect("valid json output");
        assert_eq!(parsed, res);
    }

    #[fuchsia::test]
    fn test_output_start_result_text() {
        let test_buffers = TestBuffers::default();
        let mut writer = MachineWriter::new_test(None, &test_buffers);
        let res = StartResult {
            name: "fuchsia-gce".to_string(),
            project: "my-proj".to_string(),
            zone: "us-central1-a".to_string(),
            status: "RUNNING".to_string(),
            internal_ip: Some("10.128.0.5".to_string()),
            external_ip: Some("35.1.2.3".to_string()),
            ssh_port: Some(41234),
        };

        output_start_result(&res, &mut writer).unwrap();

        let (stdout, stderr) = test_buffers.into_strings();
        assert!(stderr.is_empty());
        assert!(stdout.contains("Started GCE instance:"));
        assert!(stdout.contains("Name:              fuchsia-gce"));
        assert!(stdout.contains("Status:            RUNNING"));
        assert!(stdout.contains("Zone:              us-central1-a"));
        assert!(stdout.contains("Project:           my-proj"));
        assert!(stdout.contains("Local SSH Tunnel:  127.0.0.1:41234"));
        assert!(stdout.contains("Internal IP:       10.128.0.5"));
        assert!(stdout.contains("External IP:       35.1.2.3"));
    }
}
