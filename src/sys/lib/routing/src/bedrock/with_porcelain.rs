// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::WeakInstanceTokenExt;
use crate::component_instance::{ComponentInstanceInterface, WeakExtendedInstanceInterface};
use crate::error::{ErrorReporter, RouteRequestErrorInfo, RoutingError};
use crate::rights::Rights;
use crate::subdir::SubDir;
use async_trait::async_trait;
use capability_source::CapabilitySource;
use cm_rust::{CapabilityTypeName, EventScope, FidlIntoNative, NativeIntoFidl};
use cm_types::Availability;
use fidl_fuchsia_component_runtime::RouteRequest;
use fidl_fuchsia_component_sandbox as fsandbox;
use fidl_fuchsia_io as fio;
use moniker::{ExtendedMoniker, Moniker};
use router_error::RouterError;
use runtime_capabilities::{
    Capability, CapabilityBound, Dictionary, Routable, Router, WeakInstanceToken,
};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use strum::IntoEnumIterator;

#[cfg(target_os = "fuchsia")]
use fuchsia_trace as trace;

struct PorcelainRouter<T: CapabilityBound, R, C: ComponentInstanceInterface, const D: bool> {
    router: Arc<Router<T>>,
    porcelain_type: CapabilityTypeName,
    availability: Availability,
    rights: Option<Rights>,
    subdir: Option<SubDir>,
    inherit_rights: Option<bool>,
    event_stream_scope: Option<(Moniker, Box<[EventScope]>)>,
    target: WeakExtendedInstanceInterface<C>,
    route_request: RouteRequestErrorInfo,
    error_reporter: R,
    should_log: bool,
    #[allow(dead_code)]
    tracing: bool,
}

#[async_trait]
impl<T: CapabilityBound, R: ErrorReporter, C: ComponentInstanceInterface + 'static, const D: bool>
    Routable<T> for PorcelainRouter<T, R, C, D>
{
    async fn route(
        &self,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<Option<Arc<T>>, RouterError> {
        #[allow(unused)]
        let moniker: Option<ExtendedMoniker> = self
            .tracing
            .then(|| <Arc<WeakInstanceToken> as WeakInstanceTokenExt<C>>::moniker(&target));

        #[cfg(target_os = "fuchsia")]
        if self.tracing {
            trace::duration_begin!("component_manager", "route_capability",
                                   "target" => moniker.as_ref().unwrap().as_str(),
                                   "type" => self.route_request.type_name().as_ref(),
                                   "capability" => self.route_request.name().as_ref());
        }

        let result = match self.route_inner(request, D, target).await {
            Err(err) if self.should_log => {
                self.error_reporter
                    .report(&self.route_request, &err, self.target.clone().into())
                    .await;
                Err(err)
            }
            other_result => other_result,
        };

        #[cfg(target_os = "fuchsia")]
        if self.tracing {
            trace::duration_end!("component_manager", "route_capability",
                                 "target" => moniker.as_ref().unwrap().as_str(),
                                 "type" => self.route_request.type_name().as_ref(),
                                 "capability" => self.route_request.name().as_ref());
        }

        result
    }

    async fn route_debug(
        &self,
        request: RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<CapabilitySource, RouterError> {
        #[allow(unused)]
        let moniker: Option<ExtendedMoniker> = self
            .tracing
            .then(|| <Arc<WeakInstanceToken> as WeakInstanceTokenExt<C>>::moniker(&target));

        #[cfg(target_os = "fuchsia")]
        if self.tracing {
            trace::duration_begin!("component_manager", "route_capability_debug",
                                   "target" => moniker.as_ref().unwrap().as_str(),
                                   "type" => self.route_request.type_name().as_ref(),
                                   "capability" => self.route_request.name().as_ref());
        }

        let result = match self.route_debug_inner(request, D, target).await {
            Err(err) if self.should_log => {
                self.error_reporter
                    .report(&self.route_request, &err, self.target.clone().into())
                    .await;
                Err(err)
            }
            other_result => other_result,
        };

        #[cfg(target_os = "fuchsia")]
        if self.tracing {
            trace::duration_end!("component_manager", "route_capability_debug",
                                 "target" => moniker.as_ref().unwrap().as_str(),
                                 "type" => self.route_request.type_name().as_ref(),
                                 "capability" => self.route_request.name().as_ref());
        }

        result
    }
}

impl<T: CapabilityBound, R: ErrorReporter, C: ComponentInstanceInterface + 'static, const D: bool>
    PorcelainRouter<T, R, C, D>
{
    async fn route_inner(
        &self,
        request: RouteRequest,
        supply_default: bool,
        target: Arc<WeakInstanceToken>,
    ) -> Result<Option<Arc<T>>, RouterError> {
        let request = self.check_and_compute_request(request, supply_default)?;
        self.router.route(request, target).await
    }

    async fn route_debug_inner(
        &self,
        request: RouteRequest,
        supply_default: bool,
        target: Arc<WeakInstanceToken>,
    ) -> Result<CapabilitySource, RouterError> {
        let request = self.check_and_compute_request(request, supply_default)?;
        self.router.route_debug(request, target).await
    }

    fn check_and_compute_request(
        &self,
        request: RouteRequest,
        supply_default: bool,
    ) -> Result<RouteRequest, RouterError> {
        let PorcelainRouter {
            router: _,
            porcelain_type,
            availability,
            rights,
            subdir,
            inherit_rights,
            event_stream_scope,
            target,
            route_request: _,
            error_reporter: _,
            should_log: _,
            tracing: _,
        } = self;
        let mut request = if request != RouteRequest::default() {
            request
        } else {
            if !supply_default {
                Err(RouterError::InvalidArgs)?;
            }
            let mut request = RouteRequest::default();
            request.build_type_name = Some(porcelain_type.to_string());
            request.availability = Some(availability.native_into_fidl());
            if let Some(rights) = rights {
                request.directory_rights = Some(fio::Flags::from(*rights));
            }
            if let Some(inherit_rights) = inherit_rights {
                request.inherit_rights = Some(*inherit_rights);
            }
            if let Some((scope_moniker, scope)) = event_stream_scope.as_ref() {
                request.event_stream_scope_moniker = Some(scope_moniker.to_string());
                request.event_stream_scope = Some(scope.clone().native_into_fidl());
            }
            request
        };

        let moniker: ExtendedMoniker = match target {
            WeakExtendedInstanceInterface::Component(t) => t.moniker.clone().into(),
            WeakExtendedInstanceInterface::AboveRoot(_) => ExtendedMoniker::ComponentManager,
        };
        check_porcelain_type(&moniker, &request, *porcelain_type)?;
        let updated_availability = check_availability(&moniker, &request, *availability)?;

        check_and_compute_rights(&moniker, &mut request, &rights)?;
        if let Some(new_subdir) = check_and_compute_subdir(&moniker, &request, &subdir)? {
            request.sub_directory_path = Some(new_subdir.as_ref().clone().native_into_fidl());
        }
        if let Some((new_scope_moniker, new_scope)) = event_stream_scope.as_ref() {
            // If the scope is already set then it's a smaller scope (because we can't expose
            // these), so only set our scope if the request doesn't have one yet.
            if request.event_stream_scope_moniker.is_none() {
                request.event_stream_scope_moniker =
                    Some(new_scope_moniker.clone().native_into_fidl());
                request.event_stream_scope = Some(new_scope.clone().native_into_fidl());
            }
        }

        // Everything checks out, forward the request.
        request.availability = Some(updated_availability.native_into_fidl());
        Ok(request)
    }
}

fn check_porcelain_type(
    moniker: &ExtendedMoniker,
    request: &RouteRequest,
    expected_type: CapabilityTypeName,
) -> Result<(), RouterError> {
    let capability_type: CapabilityTypeName = request
        .build_type_name
        .as_ref()
        .ok_or_else(|| RoutingError::BedrockMissingCapabilityType {
            type_name: expected_type.to_string(),
            moniker: moniker.clone(),
        })?
        .parse()
        .map_err(|_| RouterError::InvalidArgs)?;
    if capability_type != expected_type {
        Err(RoutingError::BedrockWrongCapabilityType {
            moniker: moniker.clone(),
            actual: capability_type.to_string(),
            expected: expected_type.to_string(),
        })?;
    }
    Ok(())
}

fn check_availability(
    moniker: &ExtendedMoniker,
    request: &RouteRequest,
    availability: Availability,
) -> Result<Availability, RouterError> {
    // The availability of the request must be compatible with the
    // availability of this step of the route.
    let request_availability =
        request.availability.ok_or(fsandbox::RouterError::InvalidArgs).inspect_err(|e| {
            log::error!("request {:?} did not have availability metadata: {e:?}", request)
        })?;
    crate::availability::advance(&moniker, request_availability.fidl_into_native(), availability)
        .map_err(|e| RoutingError::from(e).into())
}

fn check_and_compute_rights(
    moniker: &ExtendedMoniker,
    request: &mut RouteRequest,
    rights: &Option<Rights>,
) -> Result<(), RouterError> {
    let Some(rights) = rights else {
        return Ok(());
    };
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
    request_rights.validate_next(&rights, moniker.clone().into()).map_err(RoutingError::from)?;
    Ok(())
}

fn check_and_compute_subdir(
    moniker: &ExtendedMoniker,
    request: &RouteRequest,
    subdir: &Option<SubDir>,
) -> Result<Option<SubDir>, RouterError> {
    let Some(mut subdir_from_decl) = subdir.clone() else {
        return Ok(None);
    };

    let request_subdir: Option<SubDir> =
        request.sub_directory_path.as_ref().map(|s| SubDir::new(s).expect("invalid sub directory"));

    if let Some(request_subdir) = request_subdir {
        let success = subdir_from_decl.as_mut().extend(request_subdir.clone().into());
        if !success {
            return Err(RoutingError::PathTooLong {
                moniker: moniker.clone(),
                path: subdir_from_decl.to_string(),
                keyword: request_subdir.to_string(),
            }
            .into());
        }
    }
    Ok(Some(subdir_from_decl))
}

pub type DefaultMetadataFn = Arc<dyn Fn(Availability) -> Dictionary + Send + Sync + 'static>;

/// Builds a router that ensures the capability request has an availability strength that is at
/// least the provided `availability`. A default `RouteRequest` is populated with `metadata_fn` if
/// the client passes an empty `RouteRequest`.
pub struct PorcelainBuilder<
    T: CapabilityBound,
    R: ErrorReporter,
    C: ComponentInstanceInterface + 'static,
    const D: bool,
> {
    router: Arc<Router<T>>,
    porcelain_type: CapabilityTypeName,
    availability: Option<Availability>,
    rights: Option<Rights>,
    subdir: Option<SubDir>,
    inherit_rights: Option<bool>,
    event_stream_scope: Option<(Moniker, Box<[EventScope]>)>,
    target: Option<WeakExtendedInstanceInterface<C>>,
    error_info: Option<RouteRequestErrorInfo>,
    error_reporter: Option<R>,
    should_log: bool,
    #[allow(dead_code)]
    tracing: bool,
}

impl<T: CapabilityBound, R: ErrorReporter, C: ComponentInstanceInterface + 'static, const D: bool>
    PorcelainBuilder<T, R, C, D>
{
    fn new(router: Arc<Router<T>>, porcelain_type: CapabilityTypeName) -> Self {
        Self {
            router,
            porcelain_type,
            availability: None,
            rights: None,
            subdir: None,
            inherit_rights: None,
            event_stream_scope: None,
            target: None,
            error_info: None,
            error_reporter: None,
            should_log: false,
            tracing: false,
        }
    }

    pub fn log_errors(mut self) -> Self {
        self.should_log = true;
        self
    }

    pub fn with_tracing(mut self) -> Self {
        self.tracing = true;
        self
    }

    /// The [Availability] attribute for this route.
    /// REQUIRED.
    pub fn availability(mut self, a: Availability) -> Self {
        self.availability = Some(a);
        self
    }

    pub fn rights(mut self, rights: Option<Rights>) -> Self {
        self.rights = rights;
        self
    }

    pub fn subdir(mut self, subdir: SubDir) -> Self {
        self.subdir = Some(subdir);
        self
    }

    pub fn inherit_rights(mut self, inherit_rights: bool) -> Self {
        self.inherit_rights = Some(inherit_rights);
        self
    }

    pub fn event_stream_scope(mut self, scope: (Moniker, Box<[EventScope]>)) -> Self {
        self.event_stream_scope = Some(scope);
        self
    }

    /// The identity of the component on behalf of whom this routing request is performed, if the
    /// caller passes a `None` request.
    /// Either this or `target_above_root` is REQUIRED.
    pub fn target(mut self, t: &Arc<C>) -> Self {
        self.target = Some(WeakExtendedInstanceInterface::Component(t.as_weak()));
        self
    }

    /// The identity of the "above root" instance that is component manager itself.
    /// Either this or `target` is REQUIRED.
    pub fn target_above_root(mut self, t: &Arc<C::TopInstance>) -> Self {
        self.target = Some(WeakExtendedInstanceInterface::AboveRoot(Arc::downgrade(t)));
        self
    }

    /// Object used to generate diagnostic information about the route that is logged if the route
    /// fails. This is usually a [cm_rust] type that is castable to [RouteRequestErrorInfo]
    /// REQUIRED.
    pub fn error_info<S>(mut self, r: S) -> Self
    where
        RouteRequestErrorInfo: From<S>,
    {
        self.error_info = Some(RouteRequestErrorInfo::from(r));
        self
    }

    /// The [ErrorReporter] used to log errors if routing fails.
    /// REQUIRED.
    pub fn error_reporter(mut self, r: R) -> Self {
        self.error_reporter = Some(r);
        self
    }

    /// Build the [PorcelainRouter] with attributes configured by this builder.
    pub fn build(self) -> Arc<Router<T>> {
        Router::new(PorcelainRouter::<T, R, C, D> {
            router: self.router,
            porcelain_type: self.porcelain_type,
            availability: self.availability.expect("must set availability"),
            rights: self.rights,
            subdir: self.subdir,
            inherit_rights: self.inherit_rights,
            event_stream_scope: self.event_stream_scope,
            target: self.target.expect("must set target"),
            route_request: self.error_info.expect("must set route_request"),
            error_reporter: self.error_reporter.expect("must set error_reporter"),
            should_log: self.should_log,
            tracing: self.tracing,
        })
    }
}

impl<R: ErrorReporter, T: CapabilityBound, C: ComponentInstanceInterface + 'static, const D: bool>
    From<PorcelainBuilder<T, R, C, D>> for Capability
where
    Arc<Router<T>>: Into<Capability>,
{
    fn from(b: PorcelainBuilder<T, R, C, D>) -> Self {
        b.build().into()
    }
}

/// See [WithPorcelain::with_porcelain] for documentation.
pub trait WithPorcelain<
    T: CapabilityBound,
    R: ErrorReporter,
    C: ComponentInstanceInterface + 'static,
>
{
    /// Returns a [PorcelainBuilder] you use to construct a new router with porcelain properties
    /// that augments the `self`. See [PorcelainBuilder] for documentation of the supported
    /// properties.
    ///
    /// If a `None` request is passed into the built router, the router will supply a default
    /// request based on the values passed to the builder.
    fn with_porcelain_with_default(
        self,
        type_: CapabilityTypeName,
    ) -> PorcelainBuilder<T, R, C, true>;

    /// Returns a [PorcelainBuilder] you use to construct a new router with porcelain properties
    /// that augments the `self`. See [PorcelainBuilder] for documentation of the supported
    /// properties.
    ///
    /// If a `None` request is passed into the built router, the router will throw an `InvalidArgs`
    /// error.
    fn with_porcelain_no_default(
        self,
        type_: CapabilityTypeName,
    ) -> PorcelainBuilder<T, R, C, false>;
}

impl<T: CapabilityBound, R: ErrorReporter, C: ComponentInstanceInterface + 'static>
    WithPorcelain<T, R, C> for Arc<Router<T>>
{
    fn with_porcelain_with_default(
        self,
        type_: CapabilityTypeName,
    ) -> PorcelainBuilder<T, R, C, true> {
        PorcelainBuilder::<T, R, C, true>::new(self, type_)
    }

    fn with_porcelain_no_default(
        self,
        type_: CapabilityTypeName,
    ) -> PorcelainBuilder<T, R, C, false> {
        PorcelainBuilder::<T, R, C, false>::new(self, type_)
    }
}

pub fn metadata_for_porcelain_type(
    typename: CapabilityTypeName,
) -> Arc<dyn Fn(Availability) -> RouteRequest + Send + Sync + 'static> {
    type MetadataMap = HashMap<
        CapabilityTypeName,
        Arc<dyn Fn(Availability) -> RouteRequest + Send + Sync + 'static>,
    >;
    static CLOSURES: LazyLock<MetadataMap> = LazyLock::new(|| {
        fn entry_for_typename(
            typename: CapabilityTypeName,
        ) -> (CapabilityTypeName, Arc<dyn Fn(Availability) -> RouteRequest + Send + Sync + 'static>)
        {
            let v = Arc::new(move |availability: Availability| RouteRequest {
                build_type_name: Some(typename.to_string()),
                availability: Some(availability.native_into_fidl()),
                ..Default::default()
            });
            (typename, v)
        }
        CapabilityTypeName::iter().map(entry_for_typename).collect()
    });
    CLOSURES.get(&typename).unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolvedInstanceInterface;
    use crate::bedrock::sandbox_construction::ComponentSandbox;
    use crate::component_instance::{ExtendedInstanceInterface, TopInstanceInterface};
    use crate::error::ComponentInstanceError;
    use crate::policy::GlobalPolicyChecker;
    use assert_matches::assert_matches;
    use capability_source::{BuiltinCapabilities, NamespaceCapabilities};
    use cm_rust_testing::UseBuilder;
    use cm_types::Url;
    use fuchsia_sync::Mutex;
    use moniker::Moniker;
    use router_error::RouterError;
    use runtime_capabilities::Data;
    use std::sync::Arc;

    #[derive(Debug)]
    struct FakeComponent {
        moniker: Moniker,
    }

    #[derive(Debug)]
    struct FakeTopInstance {
        ns: NamespaceCapabilities,
        builtin: BuiltinCapabilities,
    }

    impl TopInstanceInterface for FakeTopInstance {
        fn namespace_capabilities(&self) -> &NamespaceCapabilities {
            &self.ns
        }
        fn builtin_capabilities(&self) -> &BuiltinCapabilities {
            &self.builtin
        }
    }

    #[async_trait]
    impl ComponentInstanceInterface for FakeComponent {
        type TopInstance = FakeTopInstance;

        fn moniker(&self) -> &Moniker {
            &self.moniker
        }

        fn url(&self) -> &Url {
            panic!()
        }

        fn config_parent_overrides(&self) -> Option<&[cm_rust::ConfigOverride]> {
            panic!()
        }

        fn policy_checker(&self) -> &GlobalPolicyChecker {
            panic!()
        }

        fn component_id_index(&self) -> &component_id_index::Index {
            panic!()
        }

        fn try_get_parent(
            &self,
        ) -> Result<ExtendedInstanceInterface<Self>, ComponentInstanceError> {
            panic!()
        }

        async fn lock_resolved_state<'a>(
            self: &'a Arc<Self>,
        ) -> Result<Box<dyn ResolvedInstanceInterface<Component = Self> + 'a>, ComponentInstanceError>
        {
            panic!()
        }

        async fn component_sandbox(
            self: &Arc<Self>,
        ) -> Result<ComponentSandbox, ComponentInstanceError> {
            panic!()
        }
    }

    #[derive(Clone)]
    struct TestErrorReporter {
        reported: Arc<Mutex<bool>>,
    }

    impl TestErrorReporter {
        fn new() -> Self {
            Self { reported: Arc::new(Mutex::new(false)) }
        }
    }

    #[async_trait]
    impl ErrorReporter for TestErrorReporter {
        async fn report(
            &self,
            _request: &RouteRequestErrorInfo,
            _err: &RouterError,
            _route_target: Arc<WeakInstanceToken>,
        ) {
            let mut reported = self.reported.lock();
            if *reported {
                panic!("report() was called twice");
            }
            *reported = true;
        }
    }

    fn fake_component() -> Arc<FakeComponent> {
        Arc::new(FakeComponent { moniker: Moniker::root() })
    }

    fn error_info() -> cm_rust::UseDecl {
        UseBuilder::protocol().name("name").build()
    }

    #[fuchsia::test]
    async fn success() {
        let source = Arc::new(Data::String("hello".into()));
        let base = Router::<Data>::new_ok(source);
        let component = fake_component();
        let proxy = base
            .with_porcelain_with_default(CapabilityTypeName::Protocol)
            .availability(Availability::Optional)
            .target(&component)
            .error_info(&error_info())
            .error_reporter(TestErrorReporter::new())
            .build();
        let request = RouteRequest {
            build_type_name: Some(CapabilityTypeName::Protocol.to_string()),
            availability: Some(Availability::Optional.native_into_fidl()),
            ..Default::default()
        };

        let capability = proxy.route(request, component.as_weak().into()).await.unwrap();
        let capability = match capability {
            Some(d) => d,
            _ => panic!(),
        };
        assert_eq!(&*capability, &Data::String("hello".into()));
    }

    #[fuchsia::test]
    async fn type_missing() {
        let reporter = TestErrorReporter::new();
        let reported = reporter.reported.clone();
        let source = Data::String("hello".into());
        let base = Router::<Data>::new_ok(source);
        let component = fake_component();
        let proxy = base
            .with_porcelain_with_default(CapabilityTypeName::Protocol)
            .availability(Availability::Optional)
            .target(&component)
            .error_info(&error_info())
            .error_reporter(reporter)
            .log_errors()
            .build();
        let request = RouteRequest {
            availability: Some(Availability::Optional.native_into_fidl()),
            ..Default::default()
        };

        let error = proxy.route(request, component.as_weak().into()).await.unwrap_err();
        assert_matches!(
            error,
            RouterError::NotFound(err)
            if matches!(
                err.as_any().downcast_ref::<RoutingError>(),
                Some(RoutingError::BedrockMissingCapabilityType {
                    moniker,
                    type_name,
                }) if moniker == &Moniker::root().into() && type_name == "protocol"
            )
        );
        assert!(*reported.lock());
    }

    #[fuchsia::test]
    async fn type_mismatch() {
        let reporter = TestErrorReporter::new();
        let reported = reporter.reported.clone();
        let source = Data::String("hello".into());
        let base = Router::<Data>::new_ok(source);
        let component = fake_component();
        let proxy = base
            .with_porcelain_with_default(CapabilityTypeName::Protocol)
            .availability(Availability::Optional)
            .target(&component)
            .error_info(&error_info())
            .error_reporter(reporter)
            .log_errors()
            .build();
        let request = RouteRequest {
            build_type_name: Some(CapabilityTypeName::Service.to_string()),
            availability: Some(Availability::Optional.native_into_fidl()),
            ..Default::default()
        };

        let error = proxy.route(request, component.as_weak().into()).await.unwrap_err();
        assert_matches!(
            error,
            RouterError::NotFound(err)
            if matches!(
                err.as_any().downcast_ref::<RoutingError>(),
                Some(RoutingError::BedrockWrongCapabilityType {
                    moniker,
                    expected,
                    actual
                }) if moniker == &Moniker::root().into()
                    && expected == "protocol" && actual == "service"
            )
        );
        assert!(*reported.lock());
    }

    #[fuchsia::test]
    async fn availability_mismatch() {
        let reporter = TestErrorReporter::new();
        let reported = reporter.reported.clone();
        let source = Data::String("hello".into());
        let base = Router::<Data>::new_ok(source);
        let component = fake_component();
        let proxy = base
            .with_porcelain_with_default(CapabilityTypeName::Protocol)
            .availability(Availability::Optional)
            .target(&component)
            .error_info(&error_info())
            .error_reporter(reporter)
            .log_errors()
            .build();
        let request = RouteRequest {
            build_type_name: Some(CapabilityTypeName::Protocol.to_string()),
            availability: Some(Availability::Required.native_into_fidl()),
            ..Default::default()
        };

        let error = proxy.route(request, component.as_weak().into()).await.unwrap_err();
        assert_matches!(
            error,
            RouterError::NotFound(err)
            if matches!(
                err.as_any().downcast_ref::<RoutingError>(),
                Some(RoutingError::AvailabilityRoutingError(
                        crate::error::AvailabilityRoutingError::TargetHasStrongerAvailability {
                        moniker
                    }
                )) if moniker == &Moniker::root().into()
            )
        );
        assert!(*reported.lock());
    }
}
