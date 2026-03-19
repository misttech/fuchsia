// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bedrock::request_metadata::{
    InheritRights, IsolatedStoragePath, Metadata, StorageSourceMoniker, StorageSubdir,
};
use crate::bedrock::structured_dict::ComponentInput;
use crate::bedrock::with_policy_check::WithPolicyCheck;
use crate::capability_source::{CapabilitySource, ComponentCapability, ComponentSource};
use crate::component_instance::{
    ComponentInstanceInterface, ExtendedInstanceInterface, WeakComponentInstanceInterface,
    WeakExtendedInstanceInterface,
};
use crate::error::RoutingError;
use crate::rights::Rights;
use crate::{DictExt, LazyGet, WeakInstanceTokenExt};
use async_trait::async_trait;
use cm_rust::{CapabilityTypeName, NativeIntoFidl};
use cm_types::{Path, RelativePath};
use component_id_index::InstanceId;
use log::warn;
use moniker::{ChildName, ExtendedMoniker, Moniker};
use router_error::RouterError;
use sandbox::{
    Connector, Data, Dict, DirConnector, Request, Routable, Router, RouterResponse,
    WeakInstanceToken,
};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use {fidl_fuchsia_component_decl as fdecl, fidl_fuchsia_io as fio};

pub trait ProgramOutputGenerator<C: ComponentInstanceInterface + 'static> {
    /// Get a router for [Dict] that forwards the request to a [Router] served at `path`
    /// in the program's outgoing directory.
    fn new_program_dictionary_router(
        &self,
        component: WeakComponentInstanceInterface<C>,
        path: Path,
        capability: ComponentCapability,
    ) -> Router<Dict>;

    /// Get an outgoing directory router for `capability` that returns [Connector]. `capability`
    /// should be a type that maps to [Connector].
    fn new_outgoing_dir_connector_router(
        &self,
        component: &Arc<C>,
        decl: &cm_rust::ComponentDecl,
        capability: &cm_rust::CapabilityDecl,
    ) -> Router<Connector>;

    /// Get an outgoing directory router for `capability` that returns [DirConnector]. `capability`
    /// should be a type that maps to [DirConnector].
    fn new_outgoing_dir_dir_connector_router(
        &self,
        component: &Arc<C>,
        decl: &cm_rust::ComponentDecl,
        capability: &cm_rust::CapabilityDecl,
    ) -> Router<DirConnector>;
}

pub fn build_program_output_dictionary<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    decl: &cm_rust::ComponentDecl,
    component_input: &ComponentInput,
    child_outgoing_dictionary_routers: &HashMap<ChildName, Router<Dict>>,
    router_gen: &impl ProgramOutputGenerator<C>,
) -> (Dict, Dict) {
    let program_output_dict = Dict::new();
    let declared_dictionaries = Dict::new();
    for capability in &decl.capabilities {
        extend_dict_with_capability(
            component,
            decl,
            capability,
            &program_output_dict,
            &declared_dictionaries,
            component_input,
            child_outgoing_dictionary_routers,
            router_gen,
        );
    }
    (program_output_dict, declared_dictionaries)
}

/// Adds `capability` to the program output dict given the resolved `decl`. The program output dict
/// is a dict of routers, keyed by capability name.
fn extend_dict_with_capability<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    decl: &cm_rust::ComponentDecl,
    capability: &cm_rust::CapabilityDecl,
    program_output_dict: &Dict,
    declared_dictionaries: &Dict,
    component_input: &ComponentInput,
    child_outgoing_dictionary_routers: &HashMap<ChildName, Router<Dict>>,
    router_gen: &impl ProgramOutputGenerator<C>,
) {
    match capability {
        cm_rust::CapabilityDecl::Service(_) => {
            let router =
                router_gen.new_outgoing_dir_dir_connector_router(component, decl, capability);
            let router = router.with_policy_check::<C>(
                CapabilitySource::Component(ComponentSource {
                    capability: ComponentCapability::from(capability.clone()),
                    moniker: component.moniker().clone(),
                }),
                component.policy_checker().clone(),
            );
            match program_output_dict.insert_capability(capability.name(), router.into()) {
                Ok(()) => (),
                Err(e) => {
                    warn!("failed to add {} to program output dict: {e:?}", capability.name())
                }
            }
        }
        cm_rust::CapabilityDecl::Directory(_) => {
            let router =
                router_gen.new_outgoing_dir_dir_connector_router(component, decl, capability);
            let router = router.with_policy_check::<C>(
                CapabilitySource::Component(ComponentSource {
                    capability: ComponentCapability::from(capability.clone()),
                    moniker: component.moniker().clone(),
                }),
                component.policy_checker().clone(),
            );
            match program_output_dict.insert_capability(capability.name(), router.into()) {
                Ok(()) => (),
                Err(e) => {
                    warn!("failed to add {} to program output dict: {e:?}", capability.name())
                }
            }
        }
        cm_rust::CapabilityDecl::Storage(cm_rust::StorageDecl {
            name,
            source,
            backing_dir,
            subdir,
            storage_id,
        }) => {
            let router: Router<DirConnector> = match source {
                cm_rust::StorageDirectorySource::Parent => {
                    component_input.capabilities().get_router_or_not_found(
                        backing_dir,
                        RoutingError::storage_from_parent_not_found(
                            component.moniker(),
                            backing_dir.clone(),
                        ),
                    )
                }
                cm_rust::StorageDirectorySource::Self_ => program_output_dict
                    .get_router_or_not_found(
                        backing_dir,
                        RoutingError::BedrockNotPresentInDictionary {
                            name: backing_dir.to_string(),
                            moniker: ExtendedMoniker::ComponentInstance(
                                component.moniker().clone(),
                            ),
                        },
                    ),
                cm_rust::StorageDirectorySource::Child(child_name) => {
                    let child_name = ChildName::parse(child_name).expect("invalid child name");
                    let Some(child_component_output) =
                        child_outgoing_dictionary_routers.get(&child_name)
                    else {
                        panic!(
                            "use declaration in manifest for component {} has a source of a nonexistent child {}, this should be prevented by manifest validation",
                            component.moniker(),
                            child_name
                        );
                    };
                    child_component_output.clone().lazy_get(
                        backing_dir.to_owned(),
                        RoutingError::storage_from_child_expose_not_found(
                            &child_name,
                            &component.moniker(),
                            backing_dir.clone(),
                        ),
                    )
                }
            };

            #[derive(Debug)]
            struct StorageBackingDirRouter<C: ComponentInstanceInterface + 'static> {
                subdir: RelativePath,
                storage_id: fdecl::StorageId,
                backing_dir_router: Router<DirConnector>,
                storage_source_moniker: Moniker,
                backing_dir_target: WeakInstanceToken,
                _component_type: PhantomData<C>,
            }

            #[async_trait]
            impl<C: ComponentInstanceInterface + 'static> Routable<DirConnector>
                for StorageBackingDirRouter<C>
            {
                async fn route(
                    &self,
                    request: Option<Request>,
                    debug: bool,
                    target: WeakInstanceToken,
                ) -> Result<RouterResponse<DirConnector>, RouterError> {
                    fn generate_moniker_based_storage_path(
                        subdir: Option<String>,
                        moniker: &Moniker,
                        instance_id: Option<&InstanceId>,
                    ) -> PathBuf {
                        let mut dir_path = vec![];
                        if let Some(subdir) = subdir {
                            dir_path.push(subdir);
                        }

                        if let Some(id) = instance_id {
                            dir_path.push(id.to_string());
                            return dir_path.into_iter().collect();
                        }

                        let path = moniker.path();
                        let mut path = path.iter();
                        if let Some(p) = path.next() {
                            dir_path.push(format!("{p}:0"));
                        }
                        while let Some(p) = path.next() {
                            dir_path.push("children".to_string());
                            dir_path.push(format!("{p}:0"));
                        }

                        // Storage capabilities used to have a hardcoded set of types, which would be appended
                        // here. To maintain compatibility with the old paths (and thus not lose data when this was
                        // migrated) we append "data" here. This works because this is the only type of storage
                        // that was actually used in the wild.
                        //
                        // This is only temporary, until the storage instance id migration changes this layout.
                        dir_path.push("data".to_string());
                        dir_path.into_iter().collect()
                    }
                    let request = request.ok_or(RouterError::InvalidArgs)?;
                    let StorageBackingDirRouter {
                        subdir,
                        storage_id,
                        backing_dir_router,
                        storage_source_moniker,
                        backing_dir_target,
                        _component_type: _,
                    } = self;
                    let instance: ExtendedInstanceInterface<C> = target.upgrade().unwrap();
                    let instance = match instance {
                        ExtendedInstanceInterface::Component(c) => c,
                        ExtendedInstanceInterface::AboveRoot(_) => {
                            panic!("unexpected component manager instance")
                        }
                    };
                    let index = instance.component_id_index();
                    let instance_id = index.id_for_moniker(instance.moniker());
                    match storage_id {
                        fdecl::StorageId::StaticInstanceId if instance_id.is_none() => {
                            return Err(RouterError::from(RoutingError::ComponentNotInIdIndex {
                                source_moniker: storage_source_moniker.clone(),
                                target_name: instance.moniker().leaf().map(Into::into),
                            }));
                        }
                        _ => (),
                    }
                    let moniker = match WeakInstanceTokenExt::<C>::moniker(&target) {
                        ExtendedMoniker::ComponentInstance(m) => m,
                        ExtendedMoniker::ComponentManager => {
                            panic!("component manager is the target of a storage capability")
                        }
                    };
                    let moniker = match moniker.strip_prefix(&storage_source_moniker) {
                        Ok(v) => v,
                        Err(_) => moniker,
                    };
                    let subdir_opt = if subdir.is_dot() { None } else { Some(subdir.to_string()) };
                    let isolated_storage_path =
                        generate_moniker_based_storage_path(subdir_opt, &moniker, instance_id);
                    request.metadata.set_metadata(IsolatedStoragePath(isolated_storage_path));
                    request.metadata.set_metadata(CapabilityTypeName::Directory);
                    request.metadata.set_metadata(Rights::from(fio::RW_STAR_DIR));
                    request.metadata.set_metadata(InheritRights(false));
                    request.metadata.set_metadata(StorageSubdir(subdir.clone()));
                    request
                        .metadata
                        .set_metadata(StorageSourceMoniker(storage_source_moniker.clone()));
                    backing_dir_router.route(Some(request), debug, backing_dir_target.clone()).await
                }
            }

            let router = router.with_policy_check::<C>(
                CapabilitySource::Component(ComponentSource {
                    capability: ComponentCapability::from(capability.clone()),
                    moniker: component.moniker().clone(),
                }),
                component.policy_checker().clone(),
            );
            let router = Router::new(StorageBackingDirRouter::<C> {
                subdir: subdir.clone(),
                storage_id: storage_id.clone(),
                backing_dir_router: router,
                storage_source_moniker: component.moniker().clone(),
                backing_dir_target: WeakInstanceToken {
                    inner: Arc::new(WeakExtendedInstanceInterface::Component(component.as_weak())),
                },
                _component_type: Default::default(),
            });
            match program_output_dict.insert_capability(name, router.into()) {
                Ok(()) => (),
                Err(e) => {
                    warn!("failed to add {} to program output dict: {e:?}", name)
                }
            }
        }
        cm_rust::CapabilityDecl::Protocol(_)
        | cm_rust::CapabilityDecl::Runner(_)
        | cm_rust::CapabilityDecl::Resolver(_) => {
            let router = router_gen.new_outgoing_dir_connector_router(component, decl, capability);
            let router = router.with_policy_check::<C>(
                CapabilitySource::Component(ComponentSource {
                    capability: ComponentCapability::from(capability.clone()),
                    moniker: component.moniker().clone(),
                }),
                component.policy_checker().clone(),
            );
            match program_output_dict.insert_capability(capability.name(), router.into()) {
                Ok(()) => (),
                Err(e) => {
                    warn!("failed to add {} to program output dict: {e:?}", capability.name())
                }
            }
        }
        cm_rust::CapabilityDecl::Dictionary(d) => {
            extend_dict_with_dictionary(
                component,
                d,
                program_output_dict,
                declared_dictionaries,
                router_gen,
            );
        }
        cm_rust::CapabilityDecl::Config(c) => {
            let data = sandbox::Data::Bytes(
                fidl::persist(&c.value.clone().native_into_fidl()).unwrap().into(),
            );
            struct ConfigRouter {
                data: Data,
                source: CapabilitySource,
            }
            #[async_trait]
            impl Routable<Data> for ConfigRouter {
                async fn route(
                    &self,
                    _request: Option<Request>,
                    debug: bool,
                    _target: WeakInstanceToken,
                ) -> Result<RouterResponse<Data>, RouterError> {
                    if debug {
                        Ok(RouterResponse::Debug(
                            self.source
                                .clone()
                                .try_into()
                                .expect("failed to convert capability source to dictionary"),
                        ))
                    } else {
                        Ok(RouterResponse::Capability(self.data.clone()))
                    }
                }
            }
            let source = CapabilitySource::Component(ComponentSource {
                capability: ComponentCapability::from(capability.clone()),
                moniker: component.moniker().clone(),
            });
            let router = Router::new(ConfigRouter { data, source: source.clone() });
            let router = router.with_policy_check::<C>(source, component.policy_checker().clone());
            match program_output_dict.insert_capability(capability.name(), router.into()) {
                Ok(()) => (),
                Err(e) => {
                    warn!("failed to add {} to program output dict: {e:?}", capability.name())
                }
            }
        }
        cm_rust::CapabilityDecl::EventStream(_) => {
            // Capabilities not supported in bedrock program output dict yet.
            return;
        }
    }
}

fn extend_dict_with_dictionary<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    decl: &cm_rust::DictionaryDecl,
    program_output_dict: &Dict,
    declared_dictionaries: &Dict,
    router_gen: &impl ProgramOutputGenerator<C>,
) {
    let router;
    let declared_dict;
    if let Some(source_path) = decl.source_path.as_ref() {
        // Dictionary backed by program's outgoing directory.
        router = router_gen.new_program_dictionary_router(
            component.as_weak(),
            source_path.clone(),
            ComponentCapability::Dictionary(decl.clone()),
        );
        declared_dict = None;
    } else {
        let dict = Dict::new();
        router = make_simple_dict_router(dict.clone(), component, decl);
        declared_dict = Some(dict);
    }
    if let Some(dict) = declared_dict {
        match declared_dictionaries.insert_capability(&decl.name, dict.into()) {
            Ok(()) => (),
            Err(e) => warn!("failed to add {} to declared dicts: {e:?}", decl.name),
        };
    }
    match program_output_dict.insert_capability(&decl.name, router.into()) {
        Ok(()) => (),
        Err(e) => warn!("failed to add {} to program output dict: {e:?}", decl.name),
    }
}

/// Makes a router that always returns the given dictionary.
fn make_simple_dict_router<C: ComponentInstanceInterface + 'static>(
    dict: Dict,
    component: &Arc<C>,
    decl: &cm_rust::DictionaryDecl,
) -> Router<Dict> {
    struct DictRouter {
        dict: Dict,
        source: CapabilitySource,
    }
    #[async_trait]
    impl Routable<Dict> for DictRouter {
        async fn route(
            &self,
            _request: Option<Request>,
            debug: bool,
            _target: WeakInstanceToken,
        ) -> Result<RouterResponse<Dict>, RouterError> {
            if debug {
                Ok(RouterResponse::Debug(
                    self.source
                        .clone()
                        .try_into()
                        .expect("failed to convert capability source to dictionary"),
                ))
            } else {
                Ok(RouterResponse::Capability(self.dict.clone().into()))
            }
        }
    }
    let source = CapabilitySource::Component(ComponentSource {
        capability: ComponentCapability::Dictionary(decl.clone()),
        moniker: component.moniker().clone(),
    });
    Router::<Dict>::new(DictRouter { dict, source })
}
