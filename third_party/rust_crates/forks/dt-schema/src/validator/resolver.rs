// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

// TODO implement and wire up.

use std::{collections::HashMap, sync::Arc};

//use jsonschema::SchemaResolver;

use super::schema::Schema;

#[derive(Clone)]
pub struct LocalOnlyResolver {
    inner: Arc<Inner>,
}

struct Inner {
    schemas: HashMap<String, Arc<serde_json::Value>>,
}

impl LocalOnlyResolver {
    pub fn new(schemas: &mut dyn Iterator<Item = &Schema>) -> Self {
        LocalOnlyResolver {
            inner: Arc::new(Inner {
                schemas: schemas
                    .map(|i| {
                        (
                            i.spec_id()
                                .as_ref()
                                .unwrap()
                                .trim_end_matches('#')
                                .to_owned(),
                            Arc::new(i.raw_schema()),
                        )
                    })
                    .collect(),
            }),
        }
    }
}

/*
impl SchemaResolver for LocalOnlyResolver {
    fn resolve(
        &self,
        _root_schema: &serde_json::Value,
        url: &url::Url,
        original_reference: &str,
    ) -> Result<std::sync::Arc<serde_json::Value>, jsonschema::SchemaResolverError> {
        tracing::trace!("resolve {} ref={}", url, original_reference);
        if let Some(value) = self.inner.schemas.get(url.as_str()) {
            return Ok(value.clone());
        }
        return Err(anyhow::anyhow!("not supported"));
    }
}
*/
