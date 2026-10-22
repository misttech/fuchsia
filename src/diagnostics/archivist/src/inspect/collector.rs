// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::inspect::container::InspectHandle;
use diagnostics_data::InspectHandleName;
use fidl_fuchsia_inspect::TreeProxy;
use std::sync::{Arc, Weak};

/// Pairs a diagnostics data-object's name to the underlying encoding of that data.
pub type InspectHandleDeque = std::collections::VecDeque<(Option<InspectHandleName>, InspectData)>;

/// Data associated with a component.
/// This data is stored by data collectors and passed by the collectors to processors.
#[derive(Debug)]
pub enum InspectData {
    /// A VMO containing data associated with the event.
    Vmo { data: Arc<zx::Vmo>, escrowed: bool },

    /// A connection to a Tree service.
    Tree(TreeProxy),
}

pub fn populate_data_map(inspect_handles: &[Weak<InspectHandle>]) -> InspectHandleDeque {
    let mut data_map = InspectHandleDeque::new();
    for inspect_handle in inspect_handles {
        let Some(handle) = inspect_handle.upgrade() else {
            continue;
        };
        match handle.as_ref() {
            InspectHandle::Tree { proxy, name } => {
                data_map.push_back((
                    name.as_ref().map(|name| InspectHandleName::name(name.clone())),
                    InspectData::Tree(proxy.clone()),
                ));
            }
            InspectHandle::Escrow { vmo, name, .. } => {
                data_map.push_back((
                    name.as_ref().map(|name| InspectHandleName::name(name.clone())),
                    InspectData::Vmo { data: Arc::clone(vmo), escrowed: true },
                ));
            }
        }
    }

    data_map
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fidl::endpoints::create_request_stream;
    use fidl_fuchsia_inspect::TreeMarker;
    use fuchsia_async as fasync;
    use fuchsia_inspect::{Inspector, reader};
    use inspect_runtime::TreeServerSendPreference;
    use inspect_runtime::service::spawn_tree_server_with_stream;

    #[fuchsia::test]
    async fn populate_data_map_with_trees() {
        let insp1 = Inspector::default();
        let insp2 = Inspector::default();
        let insp3 = Inspector::default();

        insp1.root().record_int("one", 1);
        insp2.root().record_int("two", 2);
        insp3.root().record_int("three", 3);

        let scope = fasync::Scope::new();

        let (tree1, request_stream) = create_request_stream::<TreeMarker>();
        spawn_tree_server_with_stream(
            insp1,
            TreeServerSendPreference::default(),
            request_stream,
            &scope,
        );
        let (tree2, request_stream) = create_request_stream::<TreeMarker>();
        spawn_tree_server_with_stream(
            insp2,
            TreeServerSendPreference::default(),
            request_stream,
            &scope,
        );
        let (tree3, request_stream) = create_request_stream::<TreeMarker>();
        spawn_tree_server_with_stream(
            insp3,
            TreeServerSendPreference::default(),
            request_stream,
            &scope,
        );

        let name1 = Some(InspectHandleName::name("tree1"));
        let name2 = Some(InspectHandleName::name("tree2"));
        let name3 = None;

        let handles = [
            Arc::new(InspectHandle::Tree { proxy: tree1.into_proxy(), name: Some("tree1".into()) }),
            Arc::new(InspectHandle::Tree { proxy: tree2.into_proxy(), name: Some("tree2".into()) }),
            Arc::new(InspectHandle::Tree { proxy: tree3.into_proxy(), name: None }),
        ];
        let data = populate_data_map(&handles.iter().map(Arc::downgrade).collect::<Vec<_>>());
        assert_eq!(data.len(), 3);

        let (_, tree1) = data.iter().find(|(n, _)| *n == name1).unwrap();

        assert_matches!(tree1, InspectData::Tree(t) => {
            let h = reader::read(t).await.unwrap();
            assert_data_tree!(h, root: {
                one: 1i64,
            });
        });

        let (_, tree2) = data.iter().find(|(n, _)| *n == name2).unwrap();

        assert_matches!(tree2, InspectData::Tree(t) => {
            let h = reader::read(t).await.unwrap();
            assert_data_tree!(h, root: {
                two: 2i64,
            });
        });

        let (_, tree3) = data.iter().find(|(n, _)| *n == name3).unwrap();

        assert_matches!(tree3, InspectData::Tree(t) => {
            let h = reader::read(t).await.unwrap();
            assert_data_tree!(h, root: {
                three: 3i64,
            });
        });
    }
}
