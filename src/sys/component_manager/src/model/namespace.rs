// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::constants::PKG_PATH;
use crate::model::component::{ComponentInstance, Package, WeakComponentInstance};
use ::routing::component_instance::ComponentInstanceInterface;
use ::routing::mapper::NoopRouteMapper;
use ::routing::{route_to_storage_decl, verify_instance_in_component_id_index};
use cm_rust::ComponentDecl;
use cm_types::{NamespacePath, Path};
use errors::CreateNamespaceError;
use fidl::prelude::*;
use fidl_fuchsia_io as fio;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use sandbox::{Capability, Dict};
use serve_processargs::{BuildNamespaceError, NamespaceBuilder};
use std::collections::HashSet;
use std::sync::Arc;
use vfs::execution_scope::ExecutionScope;

/// Creates a component's namespace.
///
/// TODO(b/298106231): eventually this should only build a delivery map as
/// the program dict will be fetched from the resolved component state.
pub async fn create_namespace(
    package: Option<&Package>,
    component: &Arc<ComponentInstance>,
    decl: &ComponentDecl,
    program_input_dict: &Dict,
    scope: ExecutionScope,
) -> Result<NamespaceBuilder, CreateNamespaceError> {
    let not_found_sender = not_found_logging(component);
    let mut namespace = NamespaceBuilder::new(scope.clone(), not_found_sender);
    if let Some(package) = package {
        let pkg_dir = fuchsia_fs::directory::clone(&package.package_dir).map_err(|e| {
            CreateNamespaceError::ClonePkgDirFailed { moniker: component.moniker.clone(), err: e }
        })?;
        add_pkg_directory(&mut namespace, pkg_dir).map_err(|e| {
            CreateNamespaceError::BuildNamespaceError { moniker: component.moniker.clone(), err: e }
        })?;
    }

    let mut dont_flatten_past = HashSet::new();
    for use_ in &decl.uses {
        if let cm_rust::UseDecl::Storage(decl) = use_ {
            if let Ok(source) =
                route_to_storage_decl(decl.clone(), component, &mut NoopRouteMapper).await
            {
                verify_instance_in_component_id_index(&source, component)
                    .await
                    .map_err(CreateNamespaceError::InstanceNotInInstanceIdIndex)?;
            }
        }
        if let cm_rust::UseDecl::Service(decl) = use_ {
            // Services should behave like protocols, and exist within a component manager hosted
            // directory instead of being directly placed in the namespace.
            //
            // Without this, using a service and a protocol both in /svc will cause a namespace
            // path conflict because the protocol would cause a directory to go at /svc and the
            // service would cause a directory to go at /svc/{service_name}.
            dont_flatten_past.insert(decl.target_path.parent());
        }
    }

    program_input_dict_to_namespace("", &mut namespace, program_input_dict, dont_flatten_past)
        .map_err(|e| CreateNamespaceError::BuildNamespaceError {
            moniker: component.moniker.clone(),
            err: e,
        })?;
    Ok(namespace)
}

/// Adds the package directory to the namespace under the path "/pkg".
fn add_pkg_directory(
    namespace: &mut NamespaceBuilder,
    pkg_dir: fio::DirectoryProxy,
) -> Result<(), BuildNamespaceError> {
    let client_end = pkg_dir.into_client_end().unwrap();
    let directory: sandbox::Directory = client_end.into();
    let path = cm_types::NamespacePath::new(PKG_PATH.to_str().unwrap()).unwrap();
    namespace.add_entry(Capability::Directory(directory), &path)?;
    Ok(())
}

/// Adds namespace entries for a component's program input dictionary.
fn program_input_dict_to_namespace(
    prefix: &str,
    namespace: &mut NamespaceBuilder,
    program_input_dict: &Dict,
    dont_flatten_past: HashSet<NamespacePath>,
) -> Result<(), serve_processargs::BuildNamespaceError> {
    // Convert (the transformed) program_input_dict to namespace.
    //
    // The namespace is flattened as much as is possible, up until any paths listed in
    // `dont_flatten_past` (past which no flattening happens).
    //
    // For example, a dictionary that contains a dictionary at "data" that contains one directory
    // at "foo" should add a directory to the namespace at "/data/foo", not a directory at "/data".
    //
    // Alternatively if a dictionary contains a dictionary at "svc" that contains a directory
    // connector at "foo.bar" and `dont_flatten_past` contains "svc", then a dictionary gets added
    // to the namespace at "/svc", not a directory connector at "/svc/foo.bar".
    for (key, value) in program_input_dict.enumerate() {
        let new_prefix = NamespacePath::new(format!("{prefix}/{key}")).unwrap();
        match value {
            Ok(Capability::Dictionary(d)) => {
                if dont_flatten_past.contains(&new_prefix) {
                    namespace.add_entry(Capability::Dictionary(d), &new_prefix)?;
                } else {
                    program_input_dict_to_namespace(
                        &format!("{prefix}/{key}"),
                        namespace,
                        &d,
                        dont_flatten_past.clone(),
                    )?;
                }
            }
            Ok(cap @ Capability::Directory(_)) => {
                namespace.add_entry(cap, &new_prefix)?;
            }
            Ok(cap @ Capability::DirConnector(_)) => {
                namespace.add_entry(cap, &new_prefix)?;
            }
            Ok(cap @ Capability::DirConnectorRouter(_)) => {
                namespace.add_entry(cap, &new_prefix)?;
            }
            Ok(cap) => {
                namespace.add_object(cap, &Path::new(format!("{prefix}/{key}")).unwrap())?;
            }
            Err(_) => {}
        }
    }

    Ok(())
}

fn not_found_logging(component: &Arc<ComponentInstance>) -> UnboundedSender<String> {
    let (sender, mut receiver) = unbounded();
    let component_for_logger: WeakComponentInstance = component.as_weak();

    component.nonblocking_task_group().spawn(async move {
        while let Some(path) = receiver.next().await {
            match component_for_logger.upgrade() {
                Ok(target) => {
                    target
                        .log(
                            log::Level::Warn,
                            format!(
                                "No capability available at path {} for component {}, \
                             verify the component has the proper `use` declaration.",
                                path, target.moniker
                            ),
                            &[],
                        )
                        .await;
                }
                Err(_) => {}
            }
        }
    });

    sender
}
