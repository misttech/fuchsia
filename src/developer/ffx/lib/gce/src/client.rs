// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::models::{Instance, InstanceList};
use anyhow::{Context, Result, bail};
use fuchsia_hyper::{HttpsClient, new_https_client};
use http_body_util::BodyExt;
use hyper::{Method, Request};
use serde::de::DeserializeOwned;
use url::Url;

const COMPUTE_BASE: &str = "https://compute.googleapis.com/compute/v1";

pub type Body = http_body_util::Full<hyper::body::Bytes>;

#[derive(Debug)]
pub struct GceClient {
    base_url: Url,
    access_token: String,
    https_client: HttpsClient,
}

impl GceClient {
    pub fn new(access_token: String) -> Self {
        Self {
            base_url: Url::parse(COMPUTE_BASE).expect("valid compute base URL"),
            access_token,
            https_client: new_https_client(),
        }
    }

    async fn send_request<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<T> {
        let mut builder = Request::builder().method(&method).uri(url.as_str());

        if !self.access_token.is_empty() {
            builder = builder.header("Authorization", format!("Bearer {}", self.access_token));
        }

        let body_bytes = body.unwrap_or_default();
        if method == Method::POST || !body_bytes.is_empty() {
            builder = builder
                .header("Content-Type", "application/json")
                .header("Content-Length", body_bytes.len().to_string());
        }

        let req = builder.body(Body::from(body_bytes)).context("Failed to build request")?;

        let res = self.https_client.request(req).await.context("HTTP request failed")?;
        let status = res.status();

        let collected = res.into_body().collect().await.context("Failed to read response body")?;
        let bytes = collected.to_bytes();

        if !status.is_success() {
            let error_text = String::from_utf8_lossy(&bytes);
            bail!("GCE API returned error status {}: {}", status, error_text);
        }

        let parsed: T = serde_json::from_slice(&bytes).context("Failed to parse JSON response")?;
        Ok(parsed)
    }

    pub async fn get_instance(
        &self,
        project: &str,
        zone: &str,
        instance_name: &str,
    ) -> Result<Instance> {
        let mut url = self.base_url.clone();
        url.path_segments_mut().map_err(|_| anyhow::anyhow!("Invalid base URL"))?.extend(&[
            "projects",
            project,
            "zones",
            zone,
            "instances",
            instance_name,
        ]);
        self.send_request(Method::GET, url, None).await
    }

    pub async fn list_instances(&self, project: &str, zone: &str) -> Result<Vec<Instance>> {
        let mut url = self.base_url.clone();
        url.path_segments_mut().map_err(|_| anyhow::anyhow!("Invalid base URL"))?.extend(&[
            "projects",
            project,
            "zones",
            zone,
            "instances",
        ]);
        let list: InstanceList = self.send_request(Method::GET, url, None).await?;
        Ok(list.items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_url_construction() {
        let client = GceClient::new("token123".to_string());
        let mut url = client.base_url.clone();
        url.path_segments_mut().unwrap().extend(&[
            "projects",
            "test-p",
            "zones",
            "test-z",
            "instances",
        ]);
        assert_eq!(
            url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances"
        );
    }

    #[test]
    fn test_get_instance_url_construction() {
        let client = GceClient::new("token123".to_string());
        let mut url = client.base_url.clone();
        url.path_segments_mut().unwrap().extend(&[
            "projects",
            "test-p",
            "zones",
            "test-z",
            "instances",
            "test-inst",
        ]);
        assert_eq!(
            url.as_str(),
            "https://compute.googleapis.com/compute/v1/projects/test-p/zones/test-z/instances/test-inst"
        );
    }
}
