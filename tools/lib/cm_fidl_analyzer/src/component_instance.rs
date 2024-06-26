// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::component_model::{BuildAnalyzerModelError, Child};
use crate::environment::EnvironmentForAnalyzer;
use async_trait::async_trait;
use cm_config::RuntimeConfig;
use cm_rust::{CapabilityDecl, CollectionDecl, ComponentDecl, ExposeDecl, OfferDecl, UseDecl};
use cm_types::{Name, Url};
use config_encoder::ConfigFields;
use moniker::{ChildName, Moniker};
use routing::capability_source::{BuiltinCapabilities, NamespaceCapabilities};
use routing::component_instance::{
    ComponentInstanceInterface, ExtendedInstanceInterface, ResolvedInstanceInterface,
    TopInstanceInterface, WeakExtendedInstanceInterface,
};
use routing::environment::RunnerRegistry;
use routing::error::ComponentInstanceError;
use routing::policy::GlobalPolicyChecker;
use routing::resolving::{ComponentAddress, ComponentResolutionContext};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A representation of a v2 component instance.
#[derive(Debug)]
pub struct ComponentInstanceForAnalyzer {
    moniker: Moniker,
    pub(crate) decl: ComponentDecl,
    config: Option<ConfigFields>,
    url: Url,
    parent: WeakExtendedInstanceInterface<Self>,
    children: RwLock<HashMap<ChildName, Arc<Self>>>,
    pub(crate) environment: Arc<EnvironmentForAnalyzer>,
    policy_checker: GlobalPolicyChecker,
    component_id_index: Arc<component_id_index::Index>,
}

impl ComponentInstanceForAnalyzer {
    /// Exposes the component's ComponentDecl. This is referenced directly in tests.
    pub fn decl_for_testing(&self) -> &ComponentDecl {
        &self.decl
    }

    // Creates a new root component instance.
    pub(crate) fn new_root(
        decl: ComponentDecl,
        config: Option<ConfigFields>,
        url: Url,
        top_instance: Arc<TopInstanceForAnalyzer>,
        runtime_config: Arc<RuntimeConfig>,
        policy_checker: GlobalPolicyChecker,
        component_id_index: Arc<component_id_index::Index>,
        runner_registry: RunnerRegistry,
    ) -> Arc<Self> {
        let environment =
            EnvironmentForAnalyzer::new_root(runner_registry, &runtime_config, &top_instance);
        let moniker = Moniker::root();
        Arc::new(Self {
            moniker,
            decl,
            config,
            url,
            parent: WeakExtendedInstanceInterface::from(&ExtendedInstanceInterface::AboveRoot(
                top_instance,
            )),
            children: RwLock::new(HashMap::new()),
            environment,
            policy_checker,
            component_id_index,
        })
    }

    // Creates a new non-root component instance as a child of `parent`.
    pub(crate) fn new_for_child(
        child: &Child,
        child_component_decl: ComponentDecl,
        config: Option<ConfigFields>,
        parent: Arc<Self>,
        policy_checker: GlobalPolicyChecker,
        component_id_index: Arc<component_id_index::Index>,
    ) -> Result<Arc<Self>, BuildAnalyzerModelError> {
        let environment = EnvironmentForAnalyzer::new_for_child(&parent, child)?;
        let moniker = parent.moniker.child(
            ChildName::try_new(
                child.child_moniker.name().as_str(),
                child.child_moniker.collection().map(|c| c.as_str()),
            )
            .expect("child moniker is guaranteed to be valid"),
        );
        Ok(Arc::new(Self {
            moniker,
            decl: child_component_decl,
            config,
            url: child.url.clone(),
            parent: WeakExtendedInstanceInterface::from(&ExtendedInstanceInterface::Component(
                parent,
            )),
            children: RwLock::new(HashMap::new()),
            environment,
            policy_checker,
            component_id_index,
        }))
    }

    // Returns all children of the component instance.
    pub(crate) fn get_children(&self) -> Vec<Arc<Self>> {
        self.children
            .read()
            .expect("failed to acquire read lock")
            .values()
            .map(|c| Arc::clone(c))
            .collect()
    }

    // Adds a new child to this component instance.
    pub(crate) fn add_child(&self, child_moniker: ChildName, child: Arc<Self>) {
        self.children.write().expect("failed to acquire write lock").insert(child_moniker, child);
    }

    // A (nearly) no-op sync function used to implement the async trait method `lock_resolved_instance`
    // for `ComponentInstanceInterface`.
    pub(crate) fn resolve<'a>(
        self: &'a Arc<Self>,
    ) -> Result<Box<dyn ResolvedInstanceInterface<Component = Self> + 'a>, ComponentInstanceError>
    {
        Ok(Box::new(&**self))
    }

    pub fn environment(&self) -> &Arc<EnvironmentForAnalyzer> {
        &self.environment
    }

    pub fn config_fields(&self) -> Option<&ConfigFields> {
        self.config.as_ref()
    }
}

#[async_trait]
impl ComponentInstanceInterface for ComponentInstanceForAnalyzer {
    type TopInstance = TopInstanceForAnalyzer;

    fn moniker(&self) -> &Moniker {
        &self.moniker
    }

    fn child_moniker(&self) -> Option<&ChildName> {
        self.moniker.leaf()
    }

    fn url(&self) -> &Url {
        &self.url
    }

    fn environment(&self) -> &routing::environment::Environment<Self> {
        &self.environment.env()
    }

    fn try_get_parent(&self) -> Result<ExtendedInstanceInterface<Self>, ComponentInstanceError> {
        Ok(self.parent.upgrade()?)
    }

    fn policy_checker(&self) -> &GlobalPolicyChecker {
        &self.policy_checker
    }

    fn config_parent_overrides(&self) -> Option<&Vec<cm_rust::ConfigOverride>> {
        // TODO(https://fxbug.dev/42077231) ensure static parent overrides are captured
        None
    }

    fn component_id_index(&self) -> &component_id_index::Index {
        &self.component_id_index
    }

    // The trait definition requires this function to be async, but `ComponentInstanceForAnalyzer`'s
    // implementation must not await. This method is called by `route_capability`, which must
    // return immediately for `ComponentInstanceForAnalyzer` (see
    // `ComponentModelForAnalyzer::route_capability_sync()`).
    //
    // TODO(https://fxbug.dev/42168300): Remove this comment when Scrutiny's `DataController` can make async
    // function calls.
    async fn lock_resolved_state<'a>(
        self: &'a Arc<Self>,
    ) -> Result<
        Box<dyn ResolvedInstanceInterface<Component = ComponentInstanceForAnalyzer> + 'a>,
        ComponentInstanceError,
    > {
        self.resolve()
    }
}

impl ResolvedInstanceInterface for ComponentInstanceForAnalyzer {
    type Component = Self;

    fn uses(&self) -> Vec<UseDecl> {
        self.decl.uses.clone()
    }

    fn exposes(&self) -> Vec<ExposeDecl> {
        self.decl.exposes.clone()
    }

    fn offers(&self) -> Vec<OfferDecl> {
        self.decl.offers.clone()
    }

    fn capabilities(&self) -> Vec<CapabilityDecl> {
        self.decl.capabilities.clone()
    }

    fn collections(&self) -> Vec<CollectionDecl> {
        self.decl.collections.clone()
    }

    fn get_child(&self, moniker: &ChildName) -> Option<Arc<Self>> {
        self.children.read().expect("failed to acquire read lock").get(moniker).map(Arc::clone)
    }

    // This is a static model with no notion of a collection.
    fn children_in_collection(&self, _collection: &Name) -> Vec<(ChildName, Arc<Self>)> {
        vec![]
    }

    fn address(&self) -> ComponentAddress {
        ComponentAddress::from_absolute_url(&"none://not_used".parse().unwrap()).unwrap()
    }

    fn context_to_resolve_children(&self) -> Option<ComponentResolutionContext> {
        None
    }
}

/// A representation of `ComponentManager`'s instance, providing a set of capabilities to
/// the root component instance.
#[derive(Debug, Default)]
pub struct TopInstanceForAnalyzer {
    namespace_capabilities: NamespaceCapabilities,
    builtin_capabilities: BuiltinCapabilities,
}

impl TopInstanceForAnalyzer {
    pub fn new(
        namespace_capabilities: NamespaceCapabilities,
        builtin_capabilities: BuiltinCapabilities,
    ) -> Arc<Self> {
        Arc::new(Self { namespace_capabilities, builtin_capabilities })
    }
}

impl TopInstanceInterface for TopInstanceForAnalyzer {
    fn namespace_capabilities(&self) -> &NamespaceCapabilities {
        &self.namespace_capabilities
    }

    fn builtin_capabilities(&self) -> &BuiltinCapabilities {
        &self.builtin_capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cm_rust_testing::ComponentDeclBuilder;
    use futures::FutureExt;

    // Spot-checks that `ComponentInstanceForAnalyzer`'s implementation of the `ComponentInstanceInterface`
    // trait method `lock_resolved_state()` returns immediately. In addition, updates to that method should
    // be reviewed to make sure that this property holds; otherwise, `ComponentModelForAnalyzer`'s sync
    // methods may panic.
    #[test]
    fn lock_resolved_state_is_sync() {
        let decl = ComponentDeclBuilder::new().build();
        let url = "base://some_url";

        let instance = ComponentInstanceForAnalyzer::new_root(
            decl,
            None,
            url.parse().unwrap(),
            TopInstanceForAnalyzer::new(vec![], vec![]),
            Arc::new(RuntimeConfig::default()),
            GlobalPolicyChecker::default(),
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );

        assert!(instance.lock_resolved_state().now_or_never().is_some())
    }
}
