// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base::{Dependency, Entity, SettingType};
use crate::handler::base::{Error, Response};
use crate::ingress::registration::{Registrant, Registrar};
use crate::job::source::Seeder;
use fidl_fuchsia_settings::Error as SettingsError;
use fuchsia_component::server::{ServiceFsDir, ServiceObjLocal};
use serde::Deserialize;

use super::Scoped;

impl From<Error> for zx::Status {
    fn from(error: Error) -> zx::Status {
        match error {
            Error::UnhandledType(_) => zx::Status::UNAVAILABLE,
            _ => zx::Status::INTERNAL,
        }
    }
}
/// [Interface] defines the FIDL interfaces supported by the settings service.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Interface {
    Accessibility,
    Audio,
    Display(display::InterfaceFlags),
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

/// [InterfaceSpec] is the serializable type that defines the configuration for FIDL interfaces
/// supported by the settings service. It's read in from configuration files to modify what
/// interfaces the settings service provides.
#[derive(Clone, Deserialize, PartialEq, Eq, Hash, Debug)]
pub enum InterfaceSpec {
    Accessibility,
    Audio,
    // Should ideally be a HashSet, but HashSet does not impl Hash.
    Display(Vec<display::InterfaceSpec>),
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

impl From<InterfaceSpec> for Interface {
    fn from(spec: InterfaceSpec) -> Self {
        match spec {
            InterfaceSpec::Audio => Interface::Audio,
            InterfaceSpec::Accessibility => Interface::Accessibility,
            InterfaceSpec::Display(variants) => Interface::Display(variants.into()),
            InterfaceSpec::DoNotDisturb => Interface::DoNotDisturb,
            InterfaceSpec::FactoryReset => Interface::FactoryReset,
            InterfaceSpec::Input => Interface::Input,
            InterfaceSpec::Intl => Interface::Intl,
            InterfaceSpec::Keyboard => Interface::Keyboard,
            InterfaceSpec::Light => Interface::Light,
            InterfaceSpec::NightMode => Interface::NightMode,
            InterfaceSpec::Privacy => Interface::Privacy,
            InterfaceSpec::Setup => Interface::Setup,
        }
    }
}

pub mod display {
    use bitflags::bitflags;
    use serde::Deserialize;

    bitflags! {
        /// The Display interface covers a number of feature spaces, each handled by a different
        /// entity dependency. The flags below allow the scope of these features to be specified by
        /// the interface.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct InterfaceFlags: u64 {
            const BASE = 1 << 0;
        }
    }

    #[derive(Copy, Clone, Deserialize, PartialEq, Eq, Hash, Debug)]
    pub enum InterfaceSpec {
        Base,
    }

    impl From<Vec<InterfaceSpec>> for InterfaceFlags {
        fn from(variants: Vec<InterfaceSpec>) -> Self {
            variants.into_iter().fold(InterfaceFlags::empty(), |flags, variant| {
                flags
                    | match variant {
                        InterfaceSpec::Base => InterfaceFlags::BASE,
                    }
            })
        }
    }
}

impl From<Response> for Scoped<Result<(), SettingsError>> {
    fn from(response: Response) -> Self {
        Scoped(response.map_or(Err(SettingsError::Failed), |_| Ok(())))
    }
}

/// [Register] defines the closure implemented for interfaces to bring up support. Each interface
/// handler is given access to the MessageHub [Delegate] for communication within the service and
/// [ServiceFsDir] to register as the designated handler for the interface.
pub(crate) type Register =
    Box<dyn for<'a> FnOnce(&Seeder, &mut ServiceFsDir<'_, ServiceObjLocal<'a, ()>>)>;

impl Interface {
    /// Returns the list of [Dependencies](Dependency) that are necessary to provide this Interface.
    fn dependencies(self) -> Vec<Dependency> {
        match self {
            Interface::Accessibility => {
                vec![Dependency::Entity(Entity::Handler(SettingType::Accessibility))]
            }
            Interface::Audio => {
                vec![Dependency::Entity(Entity::Handler(SettingType::Audio))]
            }
            Interface::Display(interfaces) => {
                let mut dependencies = Vec::new();

                if interfaces.contains(display::InterfaceFlags::BASE) {
                    dependencies.push(Dependency::Entity(Entity::Handler(SettingType::Display)));
                }

                if dependencies.is_empty() {
                    panic!("A valid interface flag must be specified with Interface::Display");
                }

                dependencies
            }
            Interface::DoNotDisturb => {
                vec![Dependency::Entity(Entity::Handler(SettingType::DoNotDisturb))]
            }
            Interface::FactoryReset => {
                vec![Dependency::Entity(Entity::Handler(SettingType::FactoryReset))]
            }
            Interface::Input => {
                vec![Dependency::Entity(Entity::Handler(SettingType::Input))]
            }
            Interface::Intl => {
                vec![Dependency::Entity(Entity::Handler(SettingType::Intl))]
            }
            Interface::Keyboard => {
                vec![Dependency::Entity(Entity::Handler(SettingType::Keyboard))]
            }
            Interface::Light => {
                vec![Dependency::Entity(Entity::Handler(SettingType::Light))]
            }
            Interface::NightMode => {
                vec![Dependency::Entity(Entity::Handler(SettingType::NightMode))]
            }
            Interface::Privacy => {
                vec![Dependency::Entity(Entity::Handler(SettingType::Privacy))]
            }
            Interface::Setup => {
                vec![Dependency::Entity(Entity::Handler(SettingType::Setup))]
            }
        }
    }

    /// Converts an [Interface] into the closure to bring up the interface in the service environment
    /// as defined by [Register].
    fn registration_fn(self) -> Register {
        Box::new(move |_: &Seeder, _: &mut ServiceFsDir<'_, ServiceObjLocal<'_, ()>>| {
            match self {
                Interface::Accessibility
                | Interface::Audio
                | Interface::Display(_)
                | Interface::DoNotDisturb
                | Interface::FactoryReset
                | Interface::Input
                | Interface::Intl
                | Interface::Keyboard
                | Interface::Light
                | Interface::NightMode
                | Interface::Privacy
                | Interface::Setup => {} // Handled in lib.rs
            }
        })
    }

    /// Derives a [Registrant] from this [Interface]. This is used convert a list of Interfaces
    /// specified in a configuration into actionable Registrants that can be used in the setting
    /// service.
    pub(crate) fn registrant(self) -> Registrant {
        Registrant::new(
            format!("{self:?}"),
            Registrar::Fidl(self.registration_fn()),
            self.dependencies().into_iter().collect(),
        )
    }
}
