// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use tracing::{subscriber::Interest, Subscriber};
use tracing_subscriber::Layer;

pub struct LoggingMetadata {
    prefixes: Vec<String>,
}

impl LoggingMetadata {
    pub fn new(prefixes: Vec<String>) -> Self {
        LoggingMetadata { prefixes }
    }

    fn is_interesting(&self, value: &str) -> bool {
        for prefix in self.prefixes.iter() {
            if prefix.ends_with('$') && &prefix[..prefix.len() - 1] == value {
                return true;
            }
            if value.starts_with(prefix) {
                return true;
            }
        }
        false
    }
}

impl<S> Layer<S> for LoggingMetadata
where
    S: Subscriber,
{
    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if self.prefixes.is_empty() {
            return Interest::always();
        }
        if let Some(module) = metadata.module_path() {
            if self.is_interesting(module) {
                Interest::always()
            } else {
                Interest::never()
            }
        } else {
            Interest::never()
        }
    }
}
