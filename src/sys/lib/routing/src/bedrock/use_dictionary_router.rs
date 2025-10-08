// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::capability_source::CapabilitySource;
use crate::error::RoutingError;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use router_error::RouterError;
use sandbox::{Dict, Request, Routable, Router, RouterResponse};

/// Given an original dictionary and a handful of additional dictionary routers, produces a router
/// that when invoked will route all the additional routers, merge everything together into a
/// single dictionary, and return that.
pub struct UseDictionaryRouter {
    original_dictionary: Dict,
    dictionary_routers: Vec<Router<Dict>>,
    capability_source: CapabilitySource,
}

impl UseDictionaryRouter {
    pub fn new(
        original_dictionary: Dict,
        dictionary_routers: Vec<Router<Dict>>,
        capability_source: CapabilitySource,
    ) -> Router<Dict> {
        Router::new(Self { original_dictionary, dictionary_routers, capability_source })
    }
}

#[async_trait]
impl Routable<Dict> for UseDictionaryRouter {
    async fn route(
        &self,
        request: Option<Request>,
        debug: bool,
    ) -> Result<RouterResponse<Dict>, RouterError> {
        if debug {
            return Ok(RouterResponse::Debug(
                self.capability_source
                    .clone()
                    .try_into()
                    .expect("failed to serialize capability source"),
            ));
        }
        let mut futures_unordered = FuturesUnordered::new();
        for dictionary_router in self.dictionary_routers.iter() {
            let request = request.as_ref().and_then(|r| r.try_clone().ok());
            futures_unordered.push(dictionary_router.route(request, false));
        }
        let resulting_dictionary = self.original_dictionary.shallow_copy().unwrap();
        while let Some(route_result) = futures_unordered.next().await {
            match route_result {
                Ok(RouterResponse::Capability(other_dictionary)) => {
                    let maybe_conflicting_name =
                        resulting_dictionary.follow_updates_from(other_dictionary).await;
                    if let Some(conflicting_name) = maybe_conflicting_name {
                        return Err(RoutingError::ConflictingDictionaryEntries {
                            moniker: self.capability_source.source_moniker(),
                            conflicting_name,
                        }
                        .into());
                    }
                }
                Ok(RouterResponse::Unavailable) => (),
                Ok(RouterResponse::Debug(_)) => {
                    panic!("got debug response when we didn't request one")
                }
                Err(_e) => {
                    // Errors are already logged by this point by the WithPorcelain router.
                    // Specifically, the routers in `dictionary_routers` are assembled by
                    // `crate::bedrock::sandbox_construction::extend_dict_with_use`, which does
                    // this wrapping.
                }
            }
        }
        Ok(resulting_dictionary.into())
    }
}
