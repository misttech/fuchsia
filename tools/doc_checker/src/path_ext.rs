// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ffi::OsStr;
use std::path::{Component, Path};

/// Extension methods for `std::path::Path` specific to Fuchsia
/// documentation checking.
pub trait DocPathExt {
    /// Returns true if this path lies within developer-facing tools
    /// (e.g., under 'skills').
    fn is_ignored_doc(&self) -> bool;

    /// Returns true if this path represents a doc navbar.
    fn is_navbar_doc(&self) -> bool;

    /// Returns true if the file is a macOS metadata file (starts with '._').
    fn is_macos_hidden_doc(&self) -> bool;

    /// Returns true if this path has hidden/private components (starts with
    /// '_') relative to the documentation roots.
    ///
    /// Strips `root_dir` and `reference_docs_root` prefixes before performing
    /// the check, to prevent false-positive ignore matches if the checkout
    /// directory itself contains an underscore.
    fn is_hidden_doc(&self, root_dir: &Path, reference_docs_root: Option<&Path>) -> bool;
}

impl DocPathExt for Path {
    fn is_ignored_doc(&self) -> bool {
        self.components().any(|c| match c {
            Component::Normal(name) => name == OsStr::new("skills"),
            _ => false,
        })
    }

    fn is_navbar_doc(&self) -> bool {
        self.file_name() == Some(OsStr::new("navbar.md"))
    }

    fn is_macos_hidden_doc(&self) -> bool {
        self.file_name().and_then(|name| name.to_str()).map_or(false, |s| s.starts_with("._"))
    }

    fn is_hidden_doc(&self, root_dir: &Path, reference_docs_root: Option<&Path>) -> bool {
        let rel_p = self
            .strip_prefix(root_dir)
            .or_else(|_| reference_docs_root.map(|r| self.strip_prefix(r)).unwrap_or(Ok(self)))
            .unwrap_or(self);
        rel_p.components().any(|c| match c {
            Component::Normal(s) => s.to_str().unwrap_or_default().starts_with('_'),
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_is_ignored_doc() {
        assert!(Path::new("docs/skills/SKILL.md").is_ignored_doc());
        assert!(Path::new("vendor/google/skills/yaml/config.yaml").is_ignored_doc());
        assert!(!Path::new("docs/contribute/governance.md").is_ignored_doc());
        assert!(!Path::new("docs/_toc.yaml").is_ignored_doc());
    }

    #[test]
    fn test_is_navbar_doc() {
        assert!(Path::new("docs/navbar.md").is_navbar_doc());
        assert!(Path::new("navbar.md").is_navbar_doc());
        assert!(!Path::new("docs/README.md").is_navbar_doc());
    }

    #[test]
    fn test_is_macos_hidden_doc() {
        assert!(Path::new("docs/._README.md").is_macos_hidden_doc());
        assert!(Path::new("._index.md").is_macos_hidden_doc());
        assert!(!Path::new("docs/README.md").is_macos_hidden_doc());
    }

    #[test]
    fn test_is_hidden_doc() {
        let root_dir = PathBuf::from("/home/user/fuchsia");
        let ref_dir = PathBuf::from("/home/user/reference_docs");

        // Standard file (should not be hidden)
        let p1 = PathBuf::from("/home/user/fuchsia/docs/getting-started.md");
        assert!(!p1.is_hidden_doc(&root_dir, None));

        // File starting with underscore (should be hidden)
        let p2 = PathBuf::from("/home/user/fuchsia/docs/_index.md");
        assert!(p2.is_hidden_doc(&root_dir, None));

        // File inside a hidden folder (should be hidden)
        let p3 = PathBuf::from("/home/user/fuchsia/docs/_common/header.md");
        assert!(p3.is_hidden_doc(&root_dir, None));

        // Standard file in reference docs (should not be hidden)
        let p4 = PathBuf::from("/home/user/reference_docs/sdk/overview.md");
        assert!(!p4.is_hidden_doc(&root_dir, Some(&ref_dir)));

        // File inside a hidden folder in reference docs (should be hidden)
        let p5 = PathBuf::from("/home/user/reference_docs/_internal/helper.md");
        assert!(p5.is_hidden_doc(&root_dir, Some(&ref_dir)));
    }

    #[test]
    fn test_is_hidden_doc_with_underscore_in_workspace_roots() {
        // Scenario where workspace path contains underscore (e.g., /home/user/_workspace)
        let root_dir = PathBuf::from("/home/user/_workspace/fuchsia");
        let ref_dir = PathBuf::from("/home/user/_workspace/reference");

        // Standard file should NOT be hidden, even though workspace path has underscore
        let p1 = PathBuf::from("/home/user/_workspace/fuchsia/docs/getting-started.md");
        assert!(!p1.is_hidden_doc(&root_dir, Some(&ref_dir)));

        let p2 = PathBuf::from("/home/user/_workspace/reference/sdk/overview.md");
        assert!(!p2.is_hidden_doc(&root_dir, Some(&ref_dir)));

        // Hidden file inside workspace with underscore should still be detected correctly
        let p3 = PathBuf::from("/home/user/_workspace/fuchsia/docs/_common/header.md");
        assert!(p3.is_hidden_doc(&root_dir, Some(&ref_dir)));

        let p4 = PathBuf::from("/home/user/_workspace/reference/_internal/helper.md");
        assert!(p4.is_hidden_doc(&root_dir, Some(&ref_dir)));
    }
}
