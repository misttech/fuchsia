// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, bail};
use assembly_artifact_cache::{ArtifactType, MOSIdentifier};
use serde::{Deserialize, Serialize};

/// The minimum set of MOS artifacts needed to assemble a product.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedArtifactSet {
    /// The platform artifact.
    pub platform: MOSIdentifier,
    /// The product artifact.
    pub product: MOSIdentifier,
    /// The board artifact.
    pub board: MOSIdentifier,
}

impl Eq for VersionedArtifactSet {}

impl PartialEq for VersionedArtifactSet {
    fn eq(&self, other: &Self) -> bool {
        self.platform == other.platform
            && self.product == other.product
            && self.board == other.board
    }
}

impl VersionedArtifactSet {
    /// Create a VersionedArtifactSet instance from a list of MOS resource identifiers.
    pub fn new_from_mos_ids(ids: Vec<MOSIdentifier>) -> Result<Self> {
        // Define a private accumulator struct to build up a versioned artifact set
        // instance while iterating over the list of identifiers.
        #[derive(Default)]
        struct VersionedArtifactSetAccumulator {
            platform: Option<MOSIdentifier>,
            product: Option<MOSIdentifier>,
            board: Option<MOSIdentifier>,
        }
        let mut accumulator = VersionedArtifactSetAccumulator::default();

        // Iterate through the vector exactly once.
        for id in ids {
            match id.artifact_type {
                ArtifactType::Platform => {
                    if accumulator.platform.is_some() {
                        bail!("Found a duplicate platform artifact: {:?}", id);
                    }
                    accumulator.platform = Some(id);
                }
                ArtifactType::Product => {
                    if accumulator.product.is_some() {
                        bail!("Found a duplicate product artifact: {:?}", id);
                    }
                    accumulator.product = Some(id);
                }
                ArtifactType::Board => {
                    if accumulator.board.is_some() {
                        bail!("Found a duplicate board artifact: {:?}", id);
                    }
                    accumulator.board = Some(id);
                }
            }
        }

        let platform = accumulator
            .platform
            .ok_or_else(|| anyhow::anyhow!("A 'platform' artifact was not found."))?;
        let product = accumulator
            .product
            .ok_or_else(|| anyhow::anyhow!("A 'product' artifact was not found."))?;
        let board = accumulator
            .board
            .ok_or_else(|| anyhow::anyhow!("A 'board' artifact was not found."))?;

        Ok(VersionedArtifactSet { platform, product, board })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_mock_identifier(artifact_type: ArtifactType, version: &str) -> MOSIdentifier {
        MOSIdentifier {
            name: format!("{:?}", artifact_type).to_lowercase(),
            artifact_type,
            version: version.to_string(),
            repository: "fuchsia".to_string(),
            cipd: None,
        }
    }

    /// Tests that a `VersionedArtifactSet` can be successfully created from a valid set of MOS identifiers.
    #[test]
    fn test_new_from_mos_ids_success() {
        let ids = vec![
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Product, "a"),
            create_mock_identifier(ArtifactType::Board, "x"),
        ];
        let result = VersionedArtifactSet::new_from_mos_ids(ids);
        assert!(result.is_ok());
        let vas = result.unwrap();
        assert_eq!(vas.platform.version, "1");
        assert_eq!(vas.product.version, "a");
        assert_eq!(vas.board.version, "x");
    }

    /// Tests that the creation of a `VersionedArtifactSet` is independent of the order of MOS identifiers.
    #[test]
    fn test_new_from_mos_ids_order_independent() {
        let ids = vec![
            create_mock_identifier(ArtifactType::Board, "x"),
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Product, "a"),
        ];
        let result = VersionedArtifactSet::new_from_mos_ids(ids);
        assert!(result.is_ok());
    }

    /// Tests that `new_from_mos_ids` returns an error when the platform artifact is missing.
    #[test]
    fn test_new_from_mos_ids_missing_platform() {
        let ids = vec![
            create_mock_identifier(ArtifactType::Product, "a"),
            create_mock_identifier(ArtifactType::Board, "x"),
        ];
        let result = VersionedArtifactSet::new_from_mos_ids(ids);
        assert!(result.is_err());
    }

    /// Tests that `new_from_mos_ids` returns an error when the product artifact is missing.
    #[test]
    fn test_new_from_mos_ids_missing_product() {
        let ids = vec![
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Board, "x"),
        ];
        let result = VersionedArtifactSet::new_from_mos_ids(ids);
        assert!(result.is_err());
    }

    /// Tests that `new_from_mos_ids` returns an error when the board artifact is missing.
    #[test]
    fn test_new_from_mos_ids_missing_board() {
        let ids = vec![
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Product, "a"),
        ];
        let result = VersionedArtifactSet::new_from_mos_ids(ids);
        assert!(result.is_err());
    }

    /// Tests that `new_from_mos_ids` returns an error when there are duplicate platform artifacts.
    #[test]
    fn test_new_from_mos_ids_duplicate_platform() {
        let ids = vec![
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Platform, "2"),
            create_mock_identifier(ArtifactType::Product, "a"),
            create_mock_identifier(ArtifactType::Board, "x"),
        ];
        let result = VersionedArtifactSet::new_from_mos_ids(ids);
        assert!(result.is_err());
    }

    /// Tests that `new_from_mos_ids` returns an error when there are duplicate product artifacts.
    #[test]
    fn test_new_from_mos_ids_duplicate_product() {
        let ids = vec![
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Product, "a"),
            create_mock_identifier(ArtifactType::Product, "b"),
            create_mock_identifier(ArtifactType::Board, "x"),
        ];
        let result = VersionedArtifactSet::new_from_mos_ids(ids);
        assert!(result.is_err());
    }

    /// Tests that `new_from_mos_ids` returns an error when there are duplicate board artifacts.
    #[test]
    fn test_new_from_mos_ids_duplicate_board() {
        let ids = vec![
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Product, "a"),
            create_mock_identifier(ArtifactType::Board, "x"),
            create_mock_identifier(ArtifactType::Board, "y"),
        ];
        let result = VersionedArtifactSet::new_from_mos_ids(ids);
        assert!(result.is_err());
    }

    /// Tests the `PartialEq` implementation for `VersionedArtifactSet`.
    #[test]
    fn test_partial_eq() {
        let ids1 = vec![
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Product, "a"),
            create_mock_identifier(ArtifactType::Board, "x"),
        ];
        let vas1 = VersionedArtifactSet::new_from_mos_ids(ids1).unwrap();

        let ids2 = vec![
            create_mock_identifier(ArtifactType::Platform, "1"),
            create_mock_identifier(ArtifactType::Product, "a"),
            create_mock_identifier(ArtifactType::Board, "x"),
        ];
        let vas2 = VersionedArtifactSet::new_from_mos_ids(ids2).unwrap();

        assert_eq!(vas1, vas2);

        let ids3 = vec![
            create_mock_identifier(ArtifactType::Platform, "2"),
            create_mock_identifier(ArtifactType::Product, "a"),
            create_mock_identifier(ArtifactType::Board, "x"),
        ];
        let vas3 = VersionedArtifactSet::new_from_mos_ids(ids3).unwrap();

        assert_ne!(vas1, vas3);
    }
}
