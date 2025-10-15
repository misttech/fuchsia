// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides a client and parsing logic for communicating with MOS.

use crate::artifact::{Artifact, ArtifactType, CIPDPackage, MOSIdentifier};
use anyhow::{Context, Result, anyhow, bail};
use gcs::client::Client as GcsClient;
use hyper::{Body, Method, Request};
use serde::Deserialize;
use serde_json;
use serde_json::json;

/// Parse strings like "mos://fuchsia/boards/x64@1.2.3.4" into a MOS identifier.
pub fn parse_mos_artifact(s: impl AsRef<str>) -> Result<Option<Artifact>> {
    let s = s.as_ref();
    let Some(resource) = s.strip_prefix("mos://") else {
        return Ok(None);
    };

    let parts: Vec<&str> = resource.split('/').collect();
    let [repo, artifact_type_str, name_and_version] = parts.as_slice() else {
        bail!(
            "Invalid format: expected 3 parts separated by '/' (e.g., repo/type/name@version), but got {}. Full string: '{}'",
            parts.len(),
            s
        );
    };

    let Some((name, version)) = name_and_version.split_once('@') else {
        bail!(
            "Invalid format: missing '@' to separate name and version in '{}'. Full string: '{}'",
            name_and_version,
            s
        );
    };

    if repo.is_empty() || artifact_type_str.is_empty() || name.is_empty() || version.is_empty() {
        bail!(
            "Invalid format: repository, artifact type, name, or version cannot be empty. Full string: '{}'",
            s
        );
    }

    let mos_id = MOSIdentifier {
        name: name.to_string(),
        version: version.to_string(),
        repository: repo.to_string(),
        artifact_type: artifact_type_str.parse::<ArtifactType>().map_err(|()| {
            anyhow::anyhow!("Failed to parse artifact type '{}'", artifact_type_str)
        })?,
        cipd: None,
    };
    Ok(Some(Artifact::MOS(mos_id)))
}

/// Retrieve the CIPD information for a MOS identifier.
pub async fn get_cipd_package_from_mos_artifact(
    id: &MOSIdentifier,
    gcs_client: &GcsClient,
) -> Result<CIPDPackage> {
    if let Some(pkg) = &id.cipd {
        // If the CIPD field is already populated, return it.
        Ok(pkg.clone())
    } else {
        // If not, contact MOS to acquire the information.
        let client = MOSClient::new(gcs_client.clone());
        let id = client
            .get_artifact_release_info(id)
            .await
            .context(format!("Failed to get artifact release info for {:?}", id))?;
        Ok(id.cipd.unwrap())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetArtifactResponse {
    name: String,
    cipd_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductBundleArtifactsResponse {
    product_bundle_artifacts: Option<Vec<String>>,
}

/// Create a new MOSIdentifier instance from a path string from MOS.
fn new_identifier_from_path(path: &str) -> Result<MOSIdentifier> {
    // Split the path into its components based on the '/' delimiter.
    // "artifactRepositories/{repo}/versions/{version}/productBundleArtifacts/{type}_{name}"
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 6 {
        return Err(anyhow!("Path '{}' is malformed and has too few components", path));
    }

    let repository =
        parts.get(1).ok_or_else(|| anyhow!("Could not extract repository from path: {}", path))?;
    let version =
        parts.get(3).ok_or_else(|| anyhow!("Could not extract version from path: {}", path))?;
    let last_part = parts
        .last()
        .ok_or_else(|| anyhow!("Could not extract final component from path: {}", path))?;

    let type_and_name: Vec<&str> = last_part.splitn(2, '_').collect();
    if type_and_name.len() < 2 {
        return Err(anyhow!(
            "Final component '{}' does not contain type and name separated by '_'",
            last_part
        ));
    }
    let artifact_type = type_and_name.get(0).ok_or_else(|| {
        anyhow!("Could not extract artifact type from final component: {}", last_part)
    })?;
    let name = type_and_name
        .get(1)
        .ok_or_else(|| anyhow!("Could not extract name from final component: {}", last_part))?;

    Ok(MOSIdentifier {
        repository: repository.to_string(),
        version: version.to_string(),
        name: name.to_string(),
        artifact_type: artifact_type
            .parse()
            .map_err(|_| anyhow!("Failed to parse artifact type"))?,
        cipd: None,
    })
}

/// Create a new MOSIdentifier instance from an http response from MOS.
fn new_identifier_from_http_response(response: &str) -> Result<MOSIdentifier> {
    let response: GetArtifactResponse = serde_json::from_str(response)?;
    let mut mos_id = new_identifier_from_path(&response.name)?;

    let cipd = if let Some(cipd_uri) = response.cipd_uri {
        let cipd_url = format!("cipd://{}", cipd_uri);
        if let Some(Artifact::CIPD(pkg)) = crate::cipd::parse_cipd_artifact(&cipd_url)? {
            Some(pkg)
        } else {
            None
        }
    } else {
        None
    };
    mos_id.cipd = cipd;
    Ok(mos_id)
}

/// Create a new vector of MOSIdentifier instances from an http response from MOS.
/// This is used for product bundles and the interpolation API.
fn new_identifier_vec_from_http_response(response: String) -> Result<Vec<MOSIdentifier>> {
    let parsed: ProductBundleArtifactsResponse = serde_json::from_str(&response)?;
    parsed
        .product_bundle_artifacts
        .unwrap_or_default()
        .iter()
        .map(|path| new_identifier_from_path(path))
        .collect()
}

/// `MOSClient` provides functions to call the MOS artifactRepository API.
pub struct MOSClient {
    base_url: String,
    gcs_client: GcsClient,
}

impl MOSClient {
    /// `newClient` returns a `Client` given an endpoint `base_url` and `token`.
    pub fn new(gcs_client: GcsClient) -> Self {
        let base_url = "https://managedos.googleapis.com/v1alpha".to_string();
        MOSClient { base_url, gcs_client }
    }

    async fn post(&self, uri: String, data: String) -> Result<String> {
        let url = format!("{}/{}", self.base_url, uri);
        let req = Request::builder().method(Method::POST).uri(url.as_str());
        let req = req.body(Body::from(data))?;

        let res = self.gcs_client.send_request(req).await?;
        let status = res.status();
        let body_bytes = hyper::body::to_bytes(res.into_body()).await?;
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();

        if !status.is_success() {
            bail!("https post request failed, status {}: {}", status, &body_str);
        }
        Ok(body_str)
    }

    async fn get(&self, uri: String) -> Result<String> {
        let url = format!("{}/{}", self.base_url, uri);
        let req = Request::builder().method(Method::GET).uri(url.as_str());
        let req = req.body(Body::empty())?;

        let res = self.gcs_client.send_request(req).await?;
        let status = res.status();
        let body_bytes = hyper::body::to_bytes(res.into_body()).await?;
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();

        if !status.is_success() {
            bail!("https get request failed, status {}: {}", status, &body_str);
        }
        Ok(body_str)
    }

    /// Return information about a given assembly artifact
    pub async fn get_artifact_release_info(&self, id: &MOSIdentifier) -> Result<MOSIdentifier> {
        let name = format!(
            "artifactRepositories/{}/versions/{}/productBundleArtifacts/{}_{}",
            id.repository, id.version, id.artifact_type, id.name
        );
        let response = self.get(name).await.context("Failed to call getArtifact MOS API")?;
        let mos_id = new_identifier_from_http_response(&response)?;
        if mos_id.cipd.is_none() {
            bail!("MOS response for artifact {:?} did not contain CIPD information", id);
        }
        Ok(mos_id)
    }

    /// Return information about a given product bundle
    pub async fn get_pb_release_info(
        &self,
        name: String,
        version: String,
    ) -> Result<Vec<MOSIdentifier>> {
        let uri = "productBundles:search".to_string();
        let data = json!({ "criteria": {"product_name": name.clone(), "version": version.clone()}})
            .to_string();
        let response = self.post(uri.clone(), data).await?;
        let identifiers = new_identifier_vec_from_http_response(response)?;
        if identifiers.is_empty() {
            bail!("MOS returned no artifact information for product bundle {}.{}", name, version);
        }
        Ok(identifiers)
    }

    /// Interpolate between two versions of an assembly artifact
    pub async fn interpolate(
        &self,
        from_success: &MOSIdentifier,
        to_failure: &MOSIdentifier,
    ) -> Result<Vec<MOSIdentifier>> {
        if from_success.version == to_failure.version {
            return Ok(vec![from_success.clone()]);
        }

        let uri = format!(
            "artifactRepositories/{}/versions/{}/productBundleArtifacts/{}_{}:interpolate",
            from_success.repository,
            from_success.version,
            from_success.artifact_type,
            from_success.name
        );
        let data = format!(
            "{{\"target_artifact\": \"artifactRepositories/{}/versions/{}/productBundleArtifacts/{}_{}\"}}",
            to_failure.repository, to_failure.version, to_failure.artifact_type, to_failure.name
        );
        let response =
            self.post(uri.clone(), data).await.context("Failed to call interpolate API")?;
        let identifiers = new_identifier_vec_from_http_response(response)?;
        if identifiers.is_empty() {
            bail!(
                "MOS returned no results for the interpolation from {} to {}.",
                from_success.id(),
                to_failure.id()
            );
        }
        Ok(identifiers)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::artifact::Artifact;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_parse_mos_artifact_valid() {
        let artifact =
            parse_mos_artifact("mos://fuchsia/products/workstation@1.2.3").unwrap().unwrap();
        assert_eq!(
            artifact,
            Artifact::MOS(MOSIdentifier {
                repository: "fuchsia".into(),
                artifact_type: ArtifactType::Product,
                name: "workstation".into(),
                version: "1.2.3".into(),
                cipd: None,
            })
        );
    }

    #[test]
    fn test_parse_mos_artifact_invalid_prefix() {
        let artifact = parse_mos_artifact("foo://fuchsia/product/workstation@1.2.3").unwrap();
        assert_eq!(artifact, None);
    }

    #[test]
    fn test_parse_mos_artifact_missing_parts() {
        let result = parse_mos_artifact("mos://fuchsia/workstation@1.2.3");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mos_artifact_empty_repo() {
        let result = parse_mos_artifact("mos:///product/workstation@1.2.3");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mos_artifact_empty_type() {
        let result = parse_mos_artifact("mos://fuchsia//workstation@1.2.3");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mos_artifact_empty_name() {
        let result = parse_mos_artifact("mos://fuchsia/products/@1.2.3");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mos_artifact_empty_version() {
        let result = parse_mos_artifact("mos://fuchsia/products/workstation@");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mos_artifact_no_version() {
        let result = parse_mos_artifact("mos://fuchsia/products/workstation");
        assert!(result.is_err());
    }
}
