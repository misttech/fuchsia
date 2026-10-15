// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_update_installer_ext::Options;
#[cfg(test)]
use fidl_fuchsia_update_installer_ext::{Initiator, options::Range};
use std::time::{Instant, SystemTime};

/// Configuration for an update attempt.
#[derive(PartialEq, Clone)]
pub struct Config {
    pub update_url: http::Uri,
    pub options: Options,
    pub(super) start_time: SystemTime,
    pub(super) start_time_mono: Instant,
}

impl Config {
    /// Constructs update configuration from url and options.
    pub fn new(update_url: http::Uri, options: Options) -> Self {
        let start_time = SystemTime::now();
        let start_time_mono = Instant::now();

        Self { update_url, options, start_time, start_time_mono }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("initiator", &self.options.initiator)
            .field("update_url", &self.update_url.to_string())
            .field("should_write_recovery", &self.options.should_write_recovery)
            .field("should_commit", &self.options.should_commit)
            .field("start_time", &chrono::DateTime::<chrono::Utc>::from(self.start_time))
            .field("start_time_mono", &self.start_time_mono)
            .field(
                "allow_attach_to_existing_attempt",
                &self.options.allow_attach_to_existing_attempt,
            )
            .field("manifest_range", &self.options.manifest_range)
            // Only print the names of the headers, to avoid logging potentially sensitive data.
            .field(
                "manifest_header_names",
                &self
                    .options
                    .manifest_headers
                    .iter()
                    .map(|h| String::from_utf8_lossy(&h.name))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
pub struct ConfigBuilder<'a> {
    update_url: &'a str,
    should_write_recovery: bool,
    should_commit: bool,
    allow_attach_to_existing_attempt: bool,
    manifest_range: Option<Range>,
}

#[cfg(test)]
impl<'a> ConfigBuilder<'a> {
    pub fn new() -> Self {
        Self {
            update_url: "fuchsia-pkg://fuchsia.test/update",
            should_write_recovery: true,
            should_commit: true,
            allow_attach_to_existing_attempt: false,
            manifest_range: None,
        }
    }
    pub fn update_url(mut self, update_url: &'a str) -> Self {
        self.update_url = update_url;
        self
    }
    pub fn allow_attach_to_existing_attempt(
        mut self,
        allow_attach_to_existing_attempt: bool,
    ) -> Self {
        self.allow_attach_to_existing_attempt = allow_attach_to_existing_attempt;
        self
    }
    pub fn should_write_recovery(mut self, should_write_recovery: bool) -> Self {
        self.should_write_recovery = should_write_recovery;
        self
    }
    pub fn should_commit(mut self, should_commit: bool) -> Self {
        self.should_commit = should_commit;
        self
    }
    pub fn manifest_range(mut self, manifest_range: Option<Range>) -> Self {
        self.manifest_range = manifest_range;
        self
    }
    pub fn build(self) -> Result<Config, anyhow::Error> {
        let Self {
            update_url,
            should_write_recovery,
            should_commit,
            allow_attach_to_existing_attempt,
            manifest_range,
        } = self;
        Ok(Config::new(
            update_url.parse()?,
            Options {
                allow_attach_to_existing_attempt,
                should_write_recovery,
                should_commit,
                initiator: Initiator::User,
                manifest_range,
                manifest_headers: vec![],
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_new() {
        let options = Options {
            initiator: Initiator::User,
            allow_attach_to_existing_attempt: true,
            should_write_recovery: true,
            manifest_range: None,
            manifest_headers: vec![],
            should_commit: true,
        };
        let update_url: http::Uri = "fuchsia-pkg://fuchsia.test/foo".parse().unwrap();

        let config = Config::new(update_url.clone(), options.clone());

        assert_matches::assert_matches!(
            config,
            Config {
                update_url: url,
                options: opts,
                ..
            } if url == update_url && opts == options
        );
    }
}
