// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::RoutingError;
use crate::rights::Rights;
use async_trait::async_trait;
use capability_source::CapabilitySource;
use fidl_fuchsia_component_runtime::RouteRequest;
use fidl_fuchsia_io as fio;
use moniker::ExtendedMoniker;
use router_error::RouterError;
use runtime_capabilities::{CapabilityBound, Routable, Router, WeakInstanceToken};
use std::sync::Arc;

struct RightsRouter<T: CapabilityBound> {
    router: Arc<Router<T>>,
    rights: Rights,
    moniker: ExtendedMoniker,
}

impl<T: CapabilityBound> RightsRouter<T> {
    fn check_and_compute_rights(
        &self,
        mut request: RouteRequest,
    ) -> Result<RouteRequest, RouterError> {
        if request == RouteRequest::default() {
            return Err(RouterError::InvalidArgs);
        }
        let RightsRouter { router: _, rights, moniker } = self;
        let inherit = request.inherit_rights.ok_or(RouterError::InvalidArgs)?;
        let request_rights: Rights = match request.directory_rights {
            Some(request_rights) => request_rights.into(),
            None => {
                if inherit {
                    request.directory_rights = Some(fio::Flags::from(*rights));
                    *rights
                } else {
                    Err(RouterError::InvalidArgs)?
                }
            }
        };
        // The rights of the previous step (if any) of the route must be
        // compatible with this step of the route.
        if let Some(intermediate_rights) = request.directory_intermediate_rights {
            Rights::from(intermediate_rights)
                .validate_next(&rights, moniker.clone().into())
                .map_err(|e| router_error::RouterError::from(RoutingError::from(e)))?;
        };
        request.directory_intermediate_rights = Some(fio::Flags::from(*rights));
        // The rights of the request must be compatible with the
        // rights of this step of the route.
        match request_rights.validate_next(&rights, moniker.clone().into()) {
            Ok(()) => Ok(request),
            Err(e) => Err(RoutingError::from(e).into()),
        }
    }
}

#[async_trait]
impl<T: CapabilityBound> Routable<T> for RightsRouter<T> {
    async fn route(
        &self,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<Option<Arc<T>>, RouterError> {
        let request = self.check_and_compute_rights(request)?;
        self.router.route(request, target).await
    }

    async fn route_debug(
        &self,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<CapabilitySource, RouterError> {
        let request = self.check_and_compute_rights(request)?;
        self.router.route_debug(request, target).await
    }
}

pub trait WithRights {
    /// Returns a router that ensures the capability request does not request
    /// greater rights than provided at this stage of the route.
    fn with_rights(self, moniker: impl Into<ExtendedMoniker>, rights: Rights) -> Self;
}

impl<T: CapabilityBound> WithRights for Arc<Router<T>> {
    fn with_rights(self, moniker: impl Into<ExtendedMoniker>, rights: Rights) -> Self {
        Router::<T>::new(RightsRouter { rights, router: self, moniker: moniker.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl_fuchsia_io as fio;
    use router_error::RouterError;
    use runtime_capabilities::{Data, WeakInstanceToken};
    use std::sync::Arc;

    #[derive(Debug)]
    struct FakeComponentToken {}

    impl FakeComponentToken {
        fn new() -> Arc<WeakInstanceToken> {
            Arc::new(WeakInstanceToken { inner: Box::new(FakeComponentToken {}) })
        }
    }

    impl runtime_capabilities::WeakInstanceTokenAny for FakeComponentToken {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[fuchsia::test]
    async fn rights_good() {
        let source = Arc::new(Data::String("hello".into()));
        let base = Router::<Data>::new_ok(source);
        let proxy = base.with_rights(ExtendedMoniker::ComponentManager, fio::RW_STAR_DIR.into());
        let request = RouteRequest {
            directory_rights: Some(fio::PERM_READABLE),
            inherit_rights: Some(false),
            ..Default::default()
        };
        let capability = proxy.route(request, FakeComponentToken::new()).await.unwrap();
        let capability = match capability {
            Some(d) => d,
            c => panic!("Bad enum {:#?}", c),
        };
        assert_eq!(&*capability, &Data::String("hello".into()));
    }

    #[fuchsia::test]
    async fn rights_bad() {
        let source = Arc::new(Data::String("hello".into()));
        let base = Router::<Data>::new_ok(source);
        let proxy = base.with_rights(ExtendedMoniker::ComponentManager, fio::R_STAR_DIR.into());
        let request = RouteRequest {
            directory_rights: Some(fio::PERM_READABLE | fio::PERM_WRITABLE),
            inherit_rights: Some(false),
            ..Default::default()
        };
        let error = proxy.route(request, FakeComponentToken::new()).await.unwrap_err();
        assert_matches!(
            error,
            RouterError::NotFound(err)
            if matches!(
                err.as_any().downcast_ref::<RoutingError>(),
                Some(RoutingError::RightsRoutingError(
                    crate::error::RightsRoutingError::Invalid { moniker: ExtendedMoniker::ComponentManager, requested, provided }
                )) if *requested == <fio::Operations as Into<Rights>>::into(fio::RW_STAR_DIR) && *provided == <fio::Operations as Into<Rights>>::into(fio::R_STAR_DIR)
            )
        );
    }

    #[fuchsia::test]
    async fn invalid_intermediate_rights() {
        let source = Data::String("hello".into());
        let base = Router::<Data>::new_ok(source)
            .with_rights(ExtendedMoniker::ComponentManager, fio::R_STAR_DIR.into());
        let intermediate =
            base.with_rights(ExtendedMoniker::ComponentManager, fio::RW_STAR_DIR.into());
        let request = RouteRequest {
            directory_rights: Some(fio::PERM_READABLE),
            inherit_rights: Some(false),
            ..Default::default()
        };
        let error = intermediate.route(request, FakeComponentToken::new()).await.unwrap_err();
        assert_matches!(
            error,
            RouterError::NotFound(err)
            if matches!(
                err.as_any().downcast_ref::<RoutingError>(),
                Some(RoutingError::RightsRoutingError(
                    crate::error::RightsRoutingError::Invalid { moniker: ExtendedMoniker::ComponentManager, requested, provided }
                )) if *requested == <fio::Operations as Into<Rights>>::into(fio::RW_STAR_DIR) && *provided == <fio::Operations as Into<Rights>>::into(fio::R_STAR_DIR)
            )
        );
    }
}
