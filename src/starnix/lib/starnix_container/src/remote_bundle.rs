// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assembly_config_schema::product_settings::{StarnixFileOperation, StarnixFileOverride};
use ext4_extract::remote_bundle;
use ext4_extract::remote_bundle::Owner;
use ext4_metadata::{ExtendedAttributes, ROOT_INODE_NUM};
use static_assertions::const_assert;
use std::collections::HashMap;
use std::io::Result;

use camino::Utf8PathBuf;

/// Read-only by everyone because `remote_bundle`s are never writeable and we don't put anything
/// executable in HAL metadata.
const FILE_MODE: u16 = 0o0444 + linux_uapi::S_IFREG as u16;
const_assert!(linux_uapi::S_IFREG < 2_u32.pow(16)); // catch overflow

/// Default mode for new files created via overrides (regular file, rw-r--r--).
const DEFAULT_NEW_FILE_MODE: u16 = 0o100000 | 0o644;

/// Default mode for new directories created via overrides (directory, rwxr-xr-x).
const DEFAULT_NEW_DIR_MODE: u16 = 0o040000 | 0o755;

/// Read-only and enterable by everyone because `remote_bundle`s are never writeable.
pub const DIRECTORY_MODE: u16 = 0o0555 + linux_uapi::S_IFDIR as u16;
const_assert!(linux_uapi::S_IFDIR < 2_u32.pow(16)); // catch overflow

type GetExtendedAttributesFn = fn(path: &[&str]) -> ExtendedAttributes;

pub struct Writer {
    pub inner: remote_bundle::Writer,
    next_inode: u64,
    get_xattrs_for: GetExtendedAttributesFn,
}

/// Wrapper over [remote_bundle::Writer] that supplies inode numbers and attributes
/// commonly used in an android image.
impl Writer {
    /// Creates a remote bundle writer which will store its files at `out_dir`.
    pub fn new(
        out_dir: &impl AsRef<str>,
        get_xattrs_for: GetExtendedAttributesFn,
    ) -> Result<Writer> {
        Ok(Writer {
            inner: remote_bundle::Writer::new(
                out_dir,
                ROOT_INODE_NUM,
                DIRECTORY_MODE,
                Owner::root(),
                get_xattrs_for(&[]),
            )?,
            next_inode: ROOT_INODE_NUM + 1,
            get_xattrs_for,
        })
    }

    /// Add the contents of `data` as a file at `path` in the remote bundle.
    pub fn add_file(&mut self, path: &[&str], data: &mut impl std::io::Read) -> Result<()> {
        let inode = self.alloc_inode();
        self.inner.add_file(
            path,
            data,
            inode,
            FILE_MODE,
            Owner::root(),
            (self.get_xattrs_for)(path),
        )
    }

    /// Add an empty directory at `path` in the remote bundle.
    pub fn add_directory(&mut self, path: &[&str]) {
        let inode = self.alloc_inode();
        self.inner.add_directory(
            path,
            inode,
            DIRECTORY_MODE,
            Owner::root(),
            (self.get_xattrs_for)(path),
        )
    }

    fn alloc_inode(&mut self) -> u64 {
        let inode = self.next_inode;
        self.next_inode += 1;
        inode
    }
}

/// Result of applying overrides to an image's metadata.
pub(crate) struct ApplyOverridesResult {
    pub(crate) metadata: ext4_metadata::Metadata,
    pub(crate) skipped_inodes: std::collections::HashSet<u64>,
    pub(crate) new_files: Vec<(u64, Utf8PathBuf)>,
}

pub(crate) fn apply_overrides(
    original_metadata: ext4_metadata::Metadata,
    overrides: Vec<StarnixFileOverride>,
    image_name: &str,
) -> anyhow::Result<ApplyOverridesResult> {
    struct Context {
        new_metadata: ext4_metadata::Metadata,
        override_map: HashMap<Utf8PathBuf, StarnixFileOverride>,
        skipped_inodes: std::collections::HashSet<u64>,
        new_files: Vec<(u64, Utf8PathBuf)>,
        max_inode: u64,
    }

    fn copy_node(
        ctx: &mut Context,
        orig: &ext4_metadata::Metadata,
        inode: u64,
        current_path: &mut Utf8PathBuf,
        image_name: &str,
    ) -> anyhow::Result<()> {
        ctx.max_inode = std::cmp::max(ctx.max_inode, inode);

        let node = orig.get(inode).unwrap();

        // Check if there is an override for this path.
        if let Some(o) = ctx.override_map.remove(current_path.as_path()) {
            match o.operation {
                StarnixFileOperation::Remove => {
                    ctx.skipped_inodes.insert(inode);
                    return Ok(()); // Drop this node.
                }
                StarnixFileOperation::Overwrite(src_path) => {
                    ctx.skipped_inodes.insert(inode);
                    ctx.new_files.push((inode, src_path));

                    let mode = o.mode.unwrap_or(node.mode);
                    let uid = o.uid.unwrap_or(node.uid);
                    let gid = o.gid.unwrap_or(node.gid);

                    ctx.new_metadata.insert_file(
                        inode,
                        mode,
                        uid,
                        gid,
                        node.extended_attributes.clone(),
                    );
                    let path_components: Vec<&str> = current_path.iter().collect();
                    if !path_components.is_empty() {
                        ctx.new_metadata.add_child(&path_components, inode);
                    }
                    return Ok(());
                }
                StarnixFileOperation::Create(_) => {
                    anyhow::bail!(
                        "File to create already exists in image {}: {}",
                        image_name,
                        current_path
                    );
                }
            }
        }

        // No override, copy as is.
        match node.info() {
            ext4_metadata::NodeInfo::Directory(dir) => {
                ctx.new_metadata.insert_directory(
                    inode,
                    node.mode,
                    node.uid,
                    node.gid,
                    node.extended_attributes.clone(),
                );
                let path_components: Vec<&str> = current_path.iter().collect();
                if !path_components.is_empty() && inode != ext4_metadata::ROOT_INODE_NUM {
                    ctx.new_metadata.add_child(&path_components, inode);
                }

                // Iterate over children directly without cloning.
                for (name, child_inode) in &dir.children {
                    current_path.push(name.as_str());
                    copy_node(ctx, orig, *child_inode, current_path, image_name)?;
                    current_path.pop();
                }
            }
            ext4_metadata::NodeInfo::File(_) => {
                ctx.new_metadata.insert_file(
                    inode,
                    node.mode,
                    node.uid,
                    node.gid,
                    node.extended_attributes.clone(),
                );
                let path_components: Vec<&str> = current_path.iter().collect();
                if !path_components.is_empty() {
                    ctx.new_metadata.add_child(&path_components, inode);
                }
            }
            ext4_metadata::NodeInfo::Symlink(s) => {
                ctx.new_metadata.insert_symlink(
                    inode,
                    s.target.to_string(),
                    node.mode,
                    node.uid,
                    node.gid,
                    node.extended_attributes.clone(),
                );
                let path_components: Vec<&str> = current_path.iter().collect();
                if !path_components.is_empty() {
                    ctx.new_metadata.add_child(&path_components, inode);
                }
            }
        }
        Ok(())
    }

    let mut ctx = Context {
        new_metadata: ext4_metadata::Metadata::new(),
        override_map: overrides
            .into_iter()
            .map(|o| (Utf8PathBuf::from(o.file_path.clone()), o))
            .collect(),
        skipped_inodes: std::collections::HashSet::new(),
        new_files: Vec::new(),
        max_inode: ext4_metadata::ROOT_INODE_NUM,
    };

    let mut current_path = Utf8PathBuf::new();
    copy_node(
        &mut ctx,
        &original_metadata,
        ext4_metadata::ROOT_INODE_NUM,
        &mut current_path,
        image_name,
    )?;

    // Handle remaining overrides (creates).
    for (path, o) in ctx.override_map {
        match o.operation {
            StarnixFileOperation::Create(src_path) => {
                let path_components: Vec<&str> = path.iter().collect();
                let mut current_inode = ext4_metadata::ROOT_INODE_NUM;

                // Create missing parent directories.
                for i in 0..path_components.len() - 1 {
                    let component = path_components[i];
                    let node = ctx.new_metadata.get(current_inode).unwrap();
                    let dir = match node.info() {
                        ext4_metadata::NodeInfo::Directory(d) => d,
                        _ => anyhow::bail!("Path component {} is not a directory", component),
                    };

                    if let Some((_, child_inode)) =
                        dir.children.iter().find(|(k, _)| k.as_str() == component)
                    {
                        current_inode = *child_inode;
                    } else {
                        ctx.max_inode += 1;
                        let new_dir_inode = ctx.max_inode;
                        ctx.new_metadata.insert_directory(
                            new_dir_inode,
                            DEFAULT_NEW_DIR_MODE,
                            0,
                            0,
                            ext4_metadata::ExtendedAttributes::default(),
                        );
                        ctx.new_metadata.add_child(&path_components[0..=i], new_dir_inode);
                        current_inode = new_dir_inode;
                    }
                }

                // Now create the file.
                ctx.max_inode += 1;
                ctx.new_metadata.insert_file(
                    ctx.max_inode,
                    o.mode.unwrap_or(DEFAULT_NEW_FILE_MODE),
                    o.uid.unwrap_or(0),
                    o.gid.unwrap_or(0),
                    ext4_metadata::ExtendedAttributes::default(),
                );
                ctx.new_metadata.add_child(&path_components, ctx.max_inode);
                ctx.new_files.push((ctx.max_inode, src_path));
            }
            StarnixFileOperation::Overwrite(_) => {
                anyhow::bail!("File to overwrite not found in image {}: {}", image_name, path);
            }
            StarnixFileOperation::Remove => {
                anyhow::bail!("File to remove not found in image {}: {}", image_name, path);
            }
        }
    }

    Ok(ApplyOverridesResult {
        metadata: ctx.new_metadata,
        skipped_inodes: ctx.skipped_inodes,
        new_files: ctx.new_files,
    })
}
