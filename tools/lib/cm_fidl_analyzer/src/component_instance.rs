// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::component_model::{BuildAnalyzerModelError, Child, DynamicDictionaryConfig};
use crate::component_sandbox::{
    ProgramOutputGenerator, build_capability_sourced_capabilities_dictionary,
    build_framework_router, build_root_component_input, new_aggregate_router,
    new_event_stream_multiplexing_router, static_children_component_output_dictionary_routers,
};
use async_trait::async_trait;
use capability_source::{BuiltinCapabilities, NamespaceCapabilities};
use cm_config::RuntimeConfig;
use cm_rust::ComponentDecl;
use cm_types::Url;
use config_encoder::ConfigFields;
use fuchsia_sync::{Mutex, RwLock};
use moniker::{BorrowedChildName, ChildName, Moniker};
use router_error::RouterError;
use routing::bedrock::program_output_dict::build_program_output_dictionary;
use routing::bedrock::sandbox_construction::{ComponentSandbox, build_component_sandbox};
use routing::bedrock::structured_dict::ComponentInput;
use routing::component_instance::{
    ComponentInstanceInterface, ExtendedInstanceInterface, TopInstanceInterface,
    WeakExtendedInstanceInterface,
};
use routing::error::{ComponentInstanceError, ErrorReporter, RouteRequestErrorInfo};
use routing::policy::GlobalPolicyChecker;
use routing::resolving::{ComponentAddress, ComponentResolutionContext, ResolverError};
use std::collections::HashMap;
use std::sync::Arc;

/// A representation of a v2 component instance.
#[derive(Debug)]
pub struct ComponentInstanceForAnalyzer {
    moniker: Moniker,
    pub(crate) decl: ComponentDecl,
    config: Option<ConfigFields>,
    url: Url,
    parent: WeakExtendedInstanceInterface<Self>,
    pub(crate) children: RwLock<HashMap<ChildName, Arc<Self>>>,
    policy_checker: GlobalPolicyChecker,
    component_id_index: Arc<component_id_index::Index>,
    // A mutex is used here so that we can initialize a `ComponentSandbox::default()` value and
    // then overwrite that value once `build_component_sandbox` has been called.
    pub(crate) sandbox: Mutex<ComponentSandbox>,
}

impl ComponentInstanceForAnalyzer {
    /// Exposes the component's ComponentDecl. This is referenced directly in tests.
    pub fn decl_for_testing(&self) -> &ComponentDecl {
        &self.decl
    }

    /// Creates a new root component instance.
    pub(crate) fn new_root(
        decl: ComponentDecl,
        config: Option<ConfigFields>,
        url: Url,
        top_instance: Arc<TopInstanceForAnalyzer>,
        runtime_config: Arc<RuntimeConfig>,
        policy: GlobalPolicyChecker,
        id_index: Arc<component_id_index::Index>,
        dynamic_dictionaries: Arc<DynamicDictionaryConfig>,
    ) -> Arc<Self> {
        let moniker = Moniker::root();
        let root_component_input = build_root_component_input(&runtime_config, &policy);
        let parent = WeakExtendedInstanceInterface::from(&ExtendedInstanceInterface::AboveRoot(
            top_instance,
        ));
        Self::new_internal(
            moniker,
            decl,
            config,
            url,
            parent,
            policy,
            id_index,
            root_component_input,
            dynamic_dictionaries,
        )
    }

    // Creates a new non-root component instance as a child of `parent`.
    pub(crate) fn new_for_child(
        child: &Child,
        decl: ComponentDecl,
        config: Option<ConfigFields>,
        parent: Arc<Self>,
        policy: GlobalPolicyChecker,
        id_index: Arc<component_id_index::Index>,
        dynamic_dictionaries: Arc<DynamicDictionaryConfig>,
    ) -> Result<Arc<Self>, BuildAnalyzerModelError> {
        let child_name = ChildName::try_new(
            child.child_moniker.name().as_str(),
            child.child_moniker.collection().map(|c| c.as_str()),
        )
        .expect("child moniker is guaranteed to be valid");
        let url = child.url.clone();
        let input = if let Some(collection_name) = child.child_moniker.collection() {
            let input = parent
                .sandbox
                .lock()
                .collection_inputs
                .get(collection_name)
                .expect("missing collection input");
            input
        } else {
            parent
                .sandbox
                .lock()
                .child_inputs
                .get(
                    &cm_types::Name::new(child.child_moniker.name().as_str())
                        .expect("child is static so name is not long"),
                )
                .expect("missing child input")
        };
        let moniker = parent.moniker.child(child_name);
        let parent =
            WeakExtendedInstanceInterface::from(&ExtendedInstanceInterface::Component(parent));
        Ok(Self::new_internal(
            moniker,
            decl,
            config,
            url,
            parent,
            policy,
            id_index,
            input,
            dynamic_dictionaries,
        ))
    }

    fn new_internal(
        moniker: Moniker,
        decl: ComponentDecl,
        config: Option<ConfigFields>,
        url: Url,
        parent: WeakExtendedInstanceInterface<ComponentInstanceForAnalyzer>,
        policy_checker: GlobalPolicyChecker,
        component_id_index: Arc<component_id_index::Index>,
        input: ComponentInput,
        dynamic_dictionaries: Arc<DynamicDictionaryConfig>,
    ) -> Arc<Self> {
        let self_ = Arc::new(Self {
            moniker: moniker,
            decl: decl.clone(),
            config,
            url,
            parent,
            children: RwLock::new(HashMap::new()),
            policy_checker,
            component_id_index,
            sandbox: Mutex::new(Default::default()),
        });
        let children_component_output_dictionary_routers =
            static_children_component_output_dictionary_routers(&self_, &decl);
        let (program_output_dict, declared_dictionaries) = build_program_output_dictionary(
            &self_,
            &decl,
            &input,
            &children_component_output_dictionary_routers,
            &ProgramOutputGenerator {
                dynamic_dictionaries,
                executable: decl.get_runner().is_some(),
            },
        );
        #[derive(Clone)]
        struct NullErrorReporter {}
        #[async_trait]
        impl ErrorReporter for NullErrorReporter {
            async fn report(
                &self,
                _: &RouteRequestErrorInfo,
                _: &RouterError,
                _: Arc<runtime_capabilities::WeakInstanceToken>,
            ) {
            }
        }
        let sandbox = build_component_sandbox(
            &self_,
            children_component_output_dictionary_routers,
            &decl,
            input,
            program_output_dict,
            build_framework_router(&self_),
            build_capability_sourced_capabilities_dictionary(&self_, &decl),
            declared_dictionaries,
            NullErrorReporter {},
            &new_aggregate_router,
            &new_event_stream_multiplexing_router,
        );
        *self_.sandbox.lock() = sandbox;
        self_
    }

    // Returns all children of the component instance.
    pub(crate) fn get_children(&self) -> Vec<Arc<Self>> {
        self.children.read().values().map(|c| Arc::clone(c)).collect()
    }

    // Adds a new child to this component instance.
    pub(crate) fn add_child(&self, child_moniker: ChildName, child: Arc<Self>) {
        self.children.write().insert(child_moniker, child);
    }

    pub(crate) fn moniker(&self) -> &Moniker {
        &self.moniker
    }

    pub fn config_fields(&self) -> Option<&ConfigFields> {
        self.config.as_ref()
    }
}

impl PartialEq for ComponentInstanceForAnalyzer {
    fn eq(&self, other: &Self) -> bool {
        self.moniker == other.moniker
    }
}

#[async_trait]
impl ComponentInstanceInterface for ComponentInstanceForAnalyzer {
    type TopInstance = TopInstanceForAnalyzer;

    fn moniker(&self) -> &Moniker {
        &self.moniker
    }

    fn url(&self) -> &Url {
        &self.url
    }

    fn try_get_parent(&self) -> Result<ExtendedInstanceInterface<Self>, ComponentInstanceError> {
        Ok(self.parent.upgrade()?)
    }

    fn policy_checker(&self) -> &GlobalPolicyChecker {
        &self.policy_checker
    }

    fn config_parent_overrides(&self) -> Option<&[cm_rust::ConfigOverride]> {
        // TODO(https://fxbug.dev/42077231) ensure static parent overrides are captured
        None
    }

    fn component_id_index(&self) -> &component_id_index::Index {
        &self.component_id_index
    }

    async fn component_sandbox(
        self: &Arc<Self>,
    ) -> Result<ComponentSandbox, ComponentInstanceError> {
        Ok(self.sandbox.lock().clone())
    }

    async fn get_child_maybe_resolve(
        self: &Arc<Self>,
        moniker: &BorrowedChildName,
    ) -> Result<Option<Arc<Self>>, ComponentInstanceError> {
        Ok(self.children.read().get(moniker).map(Arc::clone))
    }

    async fn address_maybe_resolve(self: &Arc<Self>) -> Result<ComponentAddress, ResolverError> {
        Ok(ComponentAddress::from_absolute_url(&"none://not_used".parse().unwrap()).unwrap())
    }

    async fn context_to_resolve_children(
        self: &Arc<Self>,
    ) -> Result<Option<ComponentResolutionContext>, ComponentInstanceError> {
        Ok(None)
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

    // Spot-checks that `ComponentInstanceForAnalyzer`'s implementation of `ComponentInstanceInterface`
    // trait methods return immediately. In addition, updates to those methods should
    // be reviewed to make sure that this property holds; otherwise, `ComponentModelForAnalyzer`'s sync
    // methods may panic.
    #[test]
    fn component_instance_interface_methods_are_sync() {
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
            Arc::new(DynamicDictionaryConfig::default()),
        );

        assert!(instance.component_sandbox().now_or_never().is_some());
        assert!(instance.address_maybe_resolve().now_or_never().is_some());
        assert!(instance.context_to_resolve_children().now_or_never().is_some());
    }

    // ---------------------------------------------------------------------------------------
    // Sandbox construction memory budget guardrail & microbenchmark.
    //
    // `ComponentInstanceForAnalyzer::new_root` is synchronous and runs the same
    // `build_program_output_dictionary` + `build_component_sandbox` code that component_manager
    // runs inside `Actions::Resolve::new_resolved_state`, isolating sandbox construction from
    // resolver IPC, async scheduling, and device noise.
    //
    // A thread-local counting allocator gives an exact, deterministic heap allocation and
    // retained-bytes count per construction, even when cargo/fx test runs tests in parallel.
    // ---------------------------------------------------------------------------------------

    use cm_rust_testing::{ExposeBuilder, OfferBuilder, UseBuilder};
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct CountingAllocator;

    thread_local! {
        static ALLOCS: Cell<u64> = const { Cell::new(0) };
        static ALLOC_BYTES: Cell<u64> = const { Cell::new(0) };
        static DEALLOCS: Cell<u64> = const { Cell::new(0) };
        static DEALLOC_BYTES: Cell<u64> = const { Cell::new(0) };
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
            let _ = ALLOC_BYTES.try_with(|c| c.set(c.get() + layout.size() as u64));
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            let _ = DEALLOCS.try_with(|c| c.set(c.get() + 1));
            let _ = DEALLOC_BYTES.try_with(|c| c.set(c.get() + layout.size() as u64));
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
            let _ = ALLOC_BYTES.try_with(|c| c.set(c.get() + new_size as u64));
            let _ = DEALLOCS.try_with(|c| c.set(c.get() + 1));
            let _ = DEALLOC_BYTES.try_with(|c| c.set(c.get() + layout.size() as u64));
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;

    fn snapshot_counters() -> (u64, u64, u64, u64) {
        (
            ALLOCS.with(|c| c.get()),
            ALLOC_BYTES.with(|c| c.get()),
            DEALLOCS.with(|c| c.get()),
            DEALLOC_BYTES.with(|c| c.get()),
        )
    }

    fn synthetic_decl(
        num_children: usize,
        parent_protocols_per_child: usize,
        self_protocols_per_child: usize,
    ) -> ComponentDecl {
        let mut b = ComponentDeclBuilder::new();
        for i in 0..parent_protocols_per_child {
            b = b.use_(UseBuilder::protocol().name(&format!("fuchsia.bench.Parent{i}")));
        }
        for c in 0..num_children {
            let child = format!("child_{c}");
            b = b.child_default(&child);
            for i in 0..parent_protocols_per_child {
                b = b.offer(
                    OfferBuilder::protocol()
                        .name(&format!("fuchsia.bench.Parent{i}"))
                        .source(cm_rust::offer::OfferSource::Parent)
                        .target_static_child(&child),
                );
            }
            for i in 0..self_protocols_per_child {
                let name = format!("fuchsia.bench.Self{c}_{i}");
                b = b.protocol_default(&name);
                b = b.offer(
                    OfferBuilder::protocol()
                        .name(&name)
                        .source(cm_rust::offer::OfferSource::Self_)
                        .target_static_child(&child),
                );
            }
            b = b.expose(
                ExposeBuilder::protocol()
                    .name(&format!("fuchsia.bench.Exposed{c}"))
                    .source_static_child(&child),
            );
            if c > 0 {
                b = b.offer(
                    OfferBuilder::protocol()
                        .name(&format!("fuchsia.bench.Exposed{}", c - 1))
                        .source_static_child(&format!("child_{}", c - 1))
                        .target_static_child(&child),
                );
            }
        }
        b.build()
    }

    /// CQ guardrail test: ensures sandbox construction for a 50-child realm (549 offers,
    /// 50 exposes, 8 uses, 100 capabilities) stays within its retained heap object and byte
    /// budget (~1,991 live heap objects, ~175 KB retained heap).
    #[test]
    fn sandbox_construction_memory_budget() {
        let decl = synthetic_decl(50, 8, 2);
        let url: Url = "base://bench".parse().unwrap();
        let top_instance = TopInstanceForAnalyzer::new(vec![], vec![]);
        let runtime_config = Arc::new(RuntimeConfig::default());
        let policy = GlobalPolicyChecker::default();
        let id_index = Arc::new(component_id_index::Index::default());
        let dynamic_dictionaries = Arc::new(DynamicDictionaryConfig::default());

        let construct = |d: ComponentDecl| {
            ComponentInstanceForAnalyzer::new_root(
                d,
                None,
                url.clone(),
                Arc::clone(&top_instance),
                Arc::clone(&runtime_config),
                policy.clone(),
                Arc::clone(&id_index),
                Arc::clone(&dynamic_dictionaries),
            )
        };

        // Warm up once to initialize any lazy globals / FlyStr interning tables.
        drop(std::hint::black_box(construct(decl.clone())));

        let input = decl.clone();
        let (a0, b0, d0, db0) = snapshot_counters();
        let instance = construct(input);
        let (a1, b1, d1, db1) = snapshot_counters();
        let live_objs = (a1 - a0).saturating_sub(d1 - d0);
        let live_bytes = (b1 - b0).saturating_sub(db1 - db0);
        drop(std::hint::black_box(instance));

        assert!(
            live_objs <= 2_050,
            "sandbox construction retained heap objects regressed: {live_objs} > 2050 \
             (did a new per-route Arc/Box allocation get introduced in build_component_sandbox?)"
        );
        assert!(
            live_bytes <= 180_000,
            "sandbox construction retained heap bytes regressed: {live_bytes} > 180000 \
             (did IntermediateRouter, RouteRequest, or Capability struct layout grow?)"
        );
    }
}
