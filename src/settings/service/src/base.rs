// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Service-wide definitions.
//!
//! # Summary
//!
//! The base mod houses the core definitions for communicating information
//! across the service. Note that there are currently references to types in
//! other nested base mods. It is the long-term intention that the common
//! general (non-domain specific or overarching) definitions are migrated here,
//! while particular types, such as setting-specific definitions, are moved to
//! a common base mod underneath the parent setting mod.

use crate::ingress::fidl;
#[cfg(test)]
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;

/// The setting types supported by the service.
#[derive(PartialEq, Debug, Eq, Hash, Clone, Copy, Serialize)]
pub enum SettingType {
    /// This value is reserved for testing purposes.
    #[cfg(test)]
    Unknown,
    Accessibility,
    Audio,
    Display,
    DoNotDisturb,
    FactoryReset,
    Input,
    Intl,
    Keyboard,
    Light,
    NightMode,
    Privacy,
    Setup,
}

/// [Entity] defines the types of components that exist within the setting service. Entities can be
/// any part of the system that can be interacted with. Others can reference [Entities](Entity) to
/// declare associations, such as dependencies.
#[derive(PartialEq, Debug, Eq, Hash, Clone, Copy)]
pub enum Entity {
    /// A component that handles requests for the specified [SettingType].
    Handler(SettingType),
}

/// A [Dependency] declares a reliance of a particular configuration/feature/component/etc. within
/// the setting service. [Dependencies](Dependency) are used to generate the necessary component map
/// to support a particular service configuration. It can used to determine if the platform/product
/// configuration can support the requested service configuration.
#[derive(PartialEq, Debug, Eq, Hash, Clone, Copy)]
pub(crate) enum Dependency {
    /// An [Entity] is a component within the setting service.
    Entity(Entity),
}

impl Dependency {
    /// Returns whether the [Dependency] can be handled by the provided environment. Currently, this
    /// only involves [SettingType] handlers.
    pub(crate) fn is_fulfilled(&self, entities: &HashSet<Entity>) -> bool {
        match self {
            Dependency::Entity(entity) => entities.contains(entity),
        }
    }
}

/// This macro takes an enum, which has variants associated with exactly one data, and
/// generates the same enum and implements a for_inspect method.
/// The for_inspect method returns variants' names and formated data contents.
#[macro_export]
macro_rules! generate_inspect_with_info {
    ($(#[$metas:meta])* pub enum $name:ident {
        $(
            $(#[doc = $str:expr])*
            $(#[cfg($test:meta)])?
            $variant:ident ( $data:ty )
        ),* $(,)?
    }
    ) => {
        $(#[$metas])*
        pub enum $name {
            $(
                $(#[doc = $str])*
                $(#[cfg($test)])?
                $variant($data),
            )*
        }
    };
}

generate_inspect_with_info! {
    /// Enumeration over the possible info types available in the service.
    #[derive(PartialEq, Debug, Clone)]
    pub enum SettingInfo {
        /// This value is reserved for testing purposes.
        #[cfg(test)]
        Unknown(UnknownInfo),
    }
}

#[allow(dead_code)]
pub(crate) trait HasSettingType {
    const SETTING_TYPE: SettingType;
}

macro_rules! conversion_impls {
    ($($(#[cfg($test:meta)])? $variant:ident($info_ty:ty) => $ty_variant:ident ),+ $(,)?) => {
        $(
            $(#[cfg($test)])?
            impl HasSettingType for $info_ty {
                const SETTING_TYPE: SettingType = SettingType::$ty_variant;
            }

            $(#[cfg($test)])?
            impl TryFrom<SettingInfo> for $info_ty {
                type Error = ();

                fn try_from(setting_info: SettingInfo) -> Result<Self, ()> {
                    #[allow(unreachable_patterns)]
                    match setting_info {
                        SettingInfo::$variant(info) => Ok(info),
                    }
                }
            }
        )+
    }
}

conversion_impls! {
    #[cfg(test)] Unknown(UnknownInfo) => Unknown,
}

impl From<&SettingInfo> for SettingType {
    fn from(info: &SettingInfo) -> SettingType {
        #[allow(unreachable_patterns)]
        match info {
            #[cfg(test)]
            SettingInfo::Unknown(_) => SettingType::Unknown,
            _ => unreachable!(),
        }
    }
}

/// This struct is reserved for testing purposes. Some tests need to verify data changes, bool value
/// can be used for this purpose.
#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
#[cfg(test)]
#[derive(Default)]
pub struct UnknownInfo(pub bool);

/// Returns the default interfaces supported by any product if none are supplied.
pub fn get_default_interfaces() -> HashSet<fidl::InterfaceSpec> {
    [
        fidl::InterfaceSpec::Accessibility,
        fidl::InterfaceSpec::Intl,
        fidl::InterfaceSpec::Privacy,
        fidl::InterfaceSpec::Setup,
    ]
    .into()
}

/// Returns all known setting types. New additions to SettingType should also
/// be inserted here.
#[cfg(test)]
pub(crate) fn get_all_setting_types() -> HashSet<SettingType> {
    [
        SettingType::Accessibility,
        SettingType::Audio,
        SettingType::Display,
        SettingType::DoNotDisturb,
        SettingType::FactoryReset,
        SettingType::Input,
        SettingType::Intl,
        SettingType::Keyboard,
        SettingType::Light,
        SettingType::NightMode,
        SettingType::Privacy,
        SettingType::Setup,
    ]
    .into()
}

#[cfg(test)]
mod testing {
    use settings_storage::device_storage::DeviceStorageCompatible;
    use settings_storage::storage_factory::NoneT;

    use super::{SettingInfo, UnknownInfo};

    impl DeviceStorageCompatible for UnknownInfo {
        type Loader = NoneT;
        const KEY: &'static str = "unknown_info";
    }

    impl From<UnknownInfo> for SettingInfo {
        fn from(info: UnknownInfo) -> SettingInfo {
            SettingInfo::Unknown(info)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::bool_assert_comparison)]
    #[fuchsia::test]
    fn test_dependency_fulfillment() {
        let target_entity = Entity::Handler(SettingType::Unknown);
        let dependency = Dependency::Entity(target_entity);
        let mut available_entities = HashSet::new();

        // Verify that an empty entity set does not fulfill dependency.
        assert_eq!(dependency.is_fulfilled(&available_entities), false);

        // Verify an entity set without the target entity does not fulfill dependency.
        let _ = available_entities.insert(Entity::Handler(SettingType::FactoryReset));
        assert_eq!(dependency.is_fulfilled(&available_entities), false);

        // Verify an entity set with target entity does fulfill dependency.
        let _ = available_entities.insert(target_entity);
        assert!(dependency.is_fulfilled(&available_entities));
    }
}
