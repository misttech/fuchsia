// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::directory::ErofsDirectory;
use crate::pager::ErofsPager;
use anyhow::Context as _;
use erofs::ErofsParser;
use erofs::readers::VmoReader;
use fidl_fuchsia_io as fio;
use std::sync::Arc;
use vfs::execution_scope::ExecutionScope;

/// Holds the volume-level state for an active EROFS instance.
pub struct ErofsVolume {
    /// The parser for this EROFS volume.
    parser: ErofsParser,
    /// Reference to the unified pager.
    pager: Arc<ErofsPager>,
}

impl ErofsVolume {
    pub fn new(backing_vmo: zx::Vmo, pager: Arc<ErofsPager>) -> Result<Self, anyhow::Error> {
        let reader =
            Arc::new(VmoReader::new(Arc::new(backing_vmo)).context("Failed to create VmoReader")?);
        let parser = ErofsParser::new(reader).context("Failed to create ErofsParser")?;
        Ok(Self { parser, pager })
    }

    /// Sets up and serves an EROFS volume from a backing VMO.
    pub fn serve(
        backing_vmo: zx::Vmo,
        pager: Arc<ErofsPager>,
        flags: fio::Flags,
        root: fidl::endpoints::ServerEnd<fio::DirectoryMarker>,
    ) -> Result<(), anyhow::Error> {
        let scope = ExecutionScope::new();
        let volume = Arc::new(Self::new(backing_vmo, pager)?);
        let root_node = volume.parser().root_node();
        let root_dir = Arc::new(ErofsDirectory::new(volume, root_node));

        vfs::directory::serve_on(root_dir, flags, scope, root);
        Ok(())
    }

    /// Returns a reference to the parser.
    pub fn parser(&self) -> &ErofsParser {
        &self.parser
    }

    /// Returns a reference to the pager.
    pub fn pager(&self) -> &ErofsPager {
        &self.pager
    }
}
