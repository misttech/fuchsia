// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Error, Result, anyhow, bail};
use argh::FromArgs;
use camino::Utf8PathBuf;
use depfile::Depfile;

use starnix_container::StarnixContainerGenerator;

#[derive(Debug, PartialEq)]
struct NamedImage {
    name: String,
    path: Utf8PathBuf,
}

impl FromStr for NamedImage {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let (name, path) =
            s.split_once('=').ok_or_else(|| anyhow!("expected format <name>=<path>"))?;
        if name.is_empty() {
            bail!("image name cannot be empty");
        }
        if name.contains('/') {
            bail!("image name cannot contain '/'");
        }
        if path.is_empty() {
            bail!("image path cannot be empty");
        }
        Ok(Self { name: name.to_string(), path: Utf8PathBuf::from(path) })
    }
}

/// Construct a starnix container that can include an Android system and HALs.
#[derive(FromArgs)]
struct Command {
    /// name of the starnix container.
    #[argh(option)]
    name: String,

    /// directory to place outputs into.
    #[argh(option)]
    outdir: Utf8PathBuf,

    /// path to package archive containing additional resources to include.
    #[argh(option)]
    base: Utf8PathBuf,

    /// ext4 image to include in the container, formatted as <name>=<path>.
    #[argh(option)]
    image: Vec<NamedImage>,

    /// path to the ramdisk images (which will be concatenated).
    #[argh(option)]
    ramdisk: Vec<Utf8PathBuf>,

    /// path to hal package archive.
    #[argh(option)]
    hal: Vec<Utf8PathBuf>,

    /// path to a depfile to write.
    #[argh(option)]
    depfile: Option<Utf8PathBuf>,

    /// path to fstab, will go in /odm which overrides the one in /vendor
    #[argh(option)]
    fstab: Option<Utf8PathBuf>,

    /// path to extra init scripts, will go in /odm/etc/init. Can be passed more than once.
    #[argh(option)]
    init: Vec<Utf8PathBuf>,

    /// whether to skip including HALs as subpackages.
    #[argh(switch)]
    skip_subpackages: bool,
}

fn main() -> Result<()> {
    let cmd: Command = argh::from_env();
    let mut images = BTreeMap::new();
    for NamedImage { name, path } in cmd.image {
        if images.insert(name.clone(), path).is_some() {
            bail!("duplicate image name: {name}");
        }
    }

    let container = StarnixContainerGenerator {
        name: cmd.name,
        outdir: cmd.outdir,
        base: cmd.base,
        hals: cmd.hal,
        skip_subpackages: cmd.skip_subpackages,
        images,
        ramdisk: cmd.ramdisk,
        fstab: cmd.fstab,
        init: cmd.init,
        file_overrides: vec![],
    };

    let mut depfile = Depfile::new();
    container.build(&mut depfile)?;

    if let Some(path) = cmd.depfile {
        depfile.write_to(path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_named_image() {
        assert_eq!(
            NamedImage::from_str("product=path/to/product.img").unwrap(),
            NamedImage {
                name: "product".to_string(),
                path: Utf8PathBuf::from("path/to/product.img"),
            }
        );
        assert!(NamedImage::from_str("missing_equals").is_err());
        assert!(NamedImage::from_str("=path").is_err());
        assert!(NamedImage::from_str("a/b=path").is_err());
        assert!(NamedImage::from_str("name=").is_err());
    }
}
