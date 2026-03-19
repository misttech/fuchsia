// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::path::PathBuf;
use std::str::FromStr;

use crate::rights::Rights;
use crate::subdir::SubDir;
use cm_rust::{Availability, CapabilityTypeName};
use cm_types::RelativePath;
use fidl::{persist, unpersist};
use moniker::Moniker;
use sandbox::{Capability, Data, Dict, DictKey};
use {
    fidl_fuchsia_component_internal as finternal, fidl_fuchsia_component_sandbox as fsandbox,
    fidl_fuchsia_io as fio,
};

/// A type which has accessors for route request metadata of type T.
pub trait Metadata<T> {
    /// A key string used for setting and getting the metadata.
    const KEY: &'static str;

    /// Infallibly assigns `value` to `self`.
    fn set_metadata(&self, value: T);

    /// Retrieves the subdir metadata from `self`, if present.
    fn get_metadata(&self) -> Option<T>;
}

impl Metadata<CapabilityTypeName> for Dict {
    const KEY: &'static str = "type";

    fn set_metadata(&self, value: CapabilityTypeName) {
        let key = DictKey::new(<Self as Metadata<CapabilityTypeName>>::KEY)
            .expect("dict key creation failed unexpectedly");
        match self.insert(key, Capability::Data(Data::String(value.to_string().into()))) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<CapabilityTypeName> {
        let key = DictKey::new(<Self as Metadata<CapabilityTypeName>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        match capability {
            Capability::Data(Data::String(capability_type_name)) => {
                CapabilityTypeName::from_str(&capability_type_name).ok()
            }
            _ => None,
        }
    }
}

impl Metadata<Availability> for Dict {
    const KEY: &'static str = "availability";

    fn set_metadata(&self, value: Availability) {
        let key = DictKey::new(<Self as Metadata<Availability>>::KEY)
            .expect("dict key creation failed unexpectedly");
        match self.insert(key, Capability::Data(Data::String(value.to_string().into()))) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<Availability> {
        let key = DictKey::new(<Self as Metadata<Availability>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        match capability {
            Capability::Data(Data::String(availability)) => match &*availability {
                "Optional" => Some(Availability::Optional),
                "Required" => Some(Availability::Required),
                "SameAsTarget" => Some(Availability::SameAsTarget),
                "Transitional" => Some(Availability::Transitional),
                _ => None,
            },
            _ => None,
        }
    }
}

impl Metadata<Rights> for Dict {
    const KEY: &'static str = "rights";

    fn set_metadata(&self, value: Rights) {
        let key = DictKey::new(<Self as Metadata<Rights>>::KEY)
            .expect("dict key creation failed unexpectedly");
        match self.insert(key, Capability::Data(Data::Uint64(value.into()))) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<Rights> {
        let key = DictKey::new(<Self as Metadata<Rights>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        let rights = match capability {
            Capability::Data(Data::Uint64(rights)) => fio::Operations::from_bits(rights)?,
            _ => None?,
        };
        Some(Rights::from(rights))
    }
}
impl Metadata<finternal::EventStreamRouteMetadata> for Dict {
    const KEY: &'static str = "event_stream_route_metadata";

    fn set_metadata(&self, esrm: finternal::EventStreamRouteMetadata) {
        let key = DictKey::new(<Self as Metadata<Rights>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let value = persist(&esrm).expect("failed to persist event stream route metadata");
        match self.insert(key, Data::Bytes(value.into()).into()) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<finternal::EventStreamRouteMetadata> {
        let key = DictKey::new(<Self as Metadata<Rights>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        match capability {
            Capability::Data(Data::Bytes(bytes)) => Some(unpersist(&bytes).ok()?),
            _ => None,
        }
    }
}

/// The directory rights associated with the previous declaration in a multi-step route.
pub struct IntermediateRights(pub Rights);

impl Metadata<IntermediateRights> for Dict {
    const KEY: &'static str = "intermediate_rights";

    fn set_metadata(&self, value: IntermediateRights) {
        let key = DictKey::new(<Self as Metadata<IntermediateRights>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let IntermediateRights(value) = value;
        match self.insert(key, Capability::Data(Data::Uint64(value.into()))) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<IntermediateRights> {
        let key = DictKey::new(<Self as Metadata<IntermediateRights>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        let rights = match capability {
            Capability::Data(Data::Uint64(rights)) => fio::Operations::from_bits(rights)?,
            _ => None?,
        };
        Some(IntermediateRights(Rights::from(rights)))
    }
}

/// A flag indicating that directory rights should be inherited from the capability declaration
/// if they were not present in an expose or offer declaration.
pub struct InheritRights(pub bool);

impl Metadata<InheritRights> for Dict {
    const KEY: &'static str = "inherit_rights";

    fn set_metadata(&self, value: InheritRights) {
        let key = DictKey::new(<Self as Metadata<InheritRights>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let InheritRights(value) = value;
        match self.insert(key, Capability::Data(Data::Uint64(value.into()))) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<InheritRights> {
        let key = DictKey::new(<Self as Metadata<InheritRights>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        let inherit = match capability {
            Capability::Data(Data::Uint64(inherit)) => inherit != 0,
            _ => None?,
        };
        Some(InheritRights(inherit))
    }
}

impl Metadata<SubDir> for Dict {
    const KEY: &'static str = "subdir";

    fn set_metadata(&self, value: SubDir) {
        let key = DictKey::new(<Self as Metadata<SubDir>>::KEY)
            .expect("dict key creation failed unexpectedly");
        match self.insert(key, Capability::Data(Data::String(value.to_string().into()))) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<SubDir> {
        let key = DictKey::new(<Self as Metadata<SubDir>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        match capability {
            Capability::Data(Data::String(subdir)) => SubDir::new(subdir).ok(),
            _ => None,
        }
    }
}

/// The isolated storage path in the backing directory of the providing
/// component of a storage capability.
pub struct IsolatedStoragePath(pub PathBuf);

impl Metadata<IsolatedStoragePath> for Dict {
    const KEY: &'static str = "isolated_storage_path";

    fn set_metadata(&self, value: IsolatedStoragePath) {
        let key = DictKey::new(<Self as Metadata<IsolatedStoragePath>>::KEY)
            .expect("dict key creation failed unexpectedly");
        match self.insert(
            key,
            Capability::Data(Data::String(value.0.to_string_lossy().into_owned().into())),
        ) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<IsolatedStoragePath> {
        let key = DictKey::new(<Self as Metadata<IsolatedStoragePath>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        match capability {
            Capability::Data(Data::String(isolated_storage_path)) => {
                Some(IsolatedStoragePath(PathBuf::from(isolated_storage_path.to_string())))
            }
            _ => None,
        }
    }
}

/// The subdirectory inside of the storage backing directory's subdirectory to
/// use, if any. The difference between this and SubDir is that a) the SubDir
/// generically refers to the subdirectory of a directory capability, and b) the
/// SubDir is appended to the IsolatedStoragePath first (which is a path into a
/// backing directory), and component_manager will create the StorageSubdir if
/// it doesn't exist but won't create SubDir. Accordingly, the complete path to
/// a storage capability within the backing directory is
/// {IsolatedStoragePath}/{SubDir}/{StorageSubdir}.
pub struct StorageSubdir(pub RelativePath);

impl Metadata<StorageSubdir> for Dict {
    const KEY: &'static str = "storage_subdir";

    fn set_metadata(&self, value: StorageSubdir) {
        let key = DictKey::new(<Self as Metadata<StorageSubdir>>::KEY)
            .expect("dict key creation failed unexpectedly");
        match self.insert(key, Capability::Data(Data::String(value.0.to_string().into()))) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<StorageSubdir> {
        let key = DictKey::new(<Self as Metadata<StorageSubdir>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        match capability {
            Capability::Data(Data::String(subdir)) => {
                Some(StorageSubdir(RelativePath::new(subdir).unwrap()))
            }
            _ => None,
        }
    }
}

/// The moniker of the component that provides a Storage porcelain capability.
pub struct StorageSourceMoniker(pub Moniker);

impl Metadata<StorageSourceMoniker> for Dict {
    const KEY: &'static str = "storage_source_moniker";

    fn set_metadata(&self, value: StorageSourceMoniker) {
        let key = DictKey::new(<Self as Metadata<StorageSourceMoniker>>::KEY)
            .expect("dict key creation failed unexpectedly");
        match self.insert(key, Capability::Data(Data::String(value.0.to_string().into()))) {
            // When an entry already exists for a key in a Dict, insert() will
            // still replace that entry with the new value, even though it
            // returns an ItemAlreadyExists error. As a result, we can treat
            // ItemAlreadyExists as a success case.
            Ok(()) | Err(fsandbox::CapabilityStoreError::ItemAlreadyExists) => (),
            // Dict::insert() only returns `CapabilityStoreError::ItemAlreadyExists` variant
            Err(e) => panic!("unexpected error variant returned from Dict::insert(): {e:?}"),
        }
    }

    fn get_metadata(&self) -> Option<StorageSourceMoniker> {
        let key = DictKey::new(<Self as Metadata<StorageSourceMoniker>>::KEY)
            .expect("dict key creation failed unexpectedly");
        let capability = self.get(&key).ok()??;
        match capability {
            Capability::Data(Data::String(moniker)) => {
                Some(StorageSourceMoniker(Moniker::parse_str(&moniker).unwrap()))
            }
            _ => None,
        }
    }
}

/// Returns a `Dict` containing Router Request metadata specifying a Protocol porcelain type.
pub fn protocol_metadata(availability: cm_types::Availability) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::Protocol);
    metadata.set_metadata(availability);
    metadata
}

/// Returns a `Dict` containing Router Request metadata specifying a Dictionary porcelain type.
pub fn dictionary_metadata(availability: cm_types::Availability) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::Dictionary);
    metadata.set_metadata(availability);
    metadata
}

/// Returns a `Dict` containing Router Request metadata specifying a Directory porcelain type.
pub fn directory_metadata(
    availability: cm_types::Availability,
    rights: Option<Rights>,
    subdir: Option<SubDir>,
) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::Directory);
    if let Some(subdir) = subdir {
        metadata.set_metadata(subdir);
    }
    metadata.set_metadata(availability);
    match rights {
        Some(rights) => {
            metadata.set_metadata(rights);
            metadata.set_metadata(InheritRights(false));
        }
        None => {
            metadata.set_metadata(InheritRights(true));
        }
    }
    metadata
}

/// Returns a `Dict` containing Router Request metadata specifying a Config porcelain type.
pub fn config_metadata(availability: cm_types::Availability) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::Config);
    metadata.set_metadata(availability);
    metadata
}

/// Returns a `Dict` containing Router Request metadata specifying a Runner porcelain type.
pub fn runner_metadata(availability: cm_types::Availability) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::Runner);
    metadata.set_metadata(availability);
    metadata
}

/// Returns a `Dict` Containing Router Request metadata specifying a Resolver porcelain type.
pub fn resolver_metadata(availability: cm_types::Availability) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::Resolver);
    metadata.set_metadata(availability);
    metadata
}

/// Returns a `Dict` Containing Router Request metadata specifying a Service porcelain type.
pub fn service_metadata(availability: cm_types::Availability) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::Service);
    metadata.set_metadata(availability);
    // Service capabilities are implemented as DirConnectors. When the Router<DirConnector> that
    // connects to a component's outgoing directory wants to assemble a DirConnector, it pulls the
    // set of rights that are allowed for that DirConnector from the route metadata. This gives us
    // two choices: maintain a different Router<DirConnector> exclusively for connecting service
    // capabilities to an outgoing directory that hard-codes R_STAR_DIR, or set R_STAR_DIR in the
    // routing metadata and let the existing Router<DirConnector> use that information.
    //
    // It's less code duplication to do the latter, so we set the necessary bits to carry rights
    // information in the routing metadata for service capability routing.
    metadata.set_metadata(Rights::from(fio::R_STAR_DIR));
    metadata.set_metadata(InheritRights(true));
    metadata
}

pub fn event_stream_metadata(
    availability: cm_types::Availability,
    route_metadata: finternal::EventStreamRouteMetadata,
) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::EventStream);
    metadata.set_metadata(availability);
    metadata.set_metadata(route_metadata);
    metadata
}

/// Returns a `Dict` containing Router Request metadata specifying a Storage porcelain type.
pub fn storage_metadata(availability: cm_types::Availability) -> sandbox::Dict {
    let metadata = sandbox::Dict::new();
    metadata.set_metadata(CapabilityTypeName::Storage);
    metadata.set_metadata(availability);
    metadata.set_metadata(Rights::from(fio::RW_STAR_DIR));
    metadata.set_metadata(InheritRights(false));
    metadata
}
