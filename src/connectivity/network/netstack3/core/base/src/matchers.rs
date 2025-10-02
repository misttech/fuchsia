// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Trait definition for matchers.

use alloc::format;
use alloc::string::String;
use core::fmt::Debug;
use core::num::NonZeroU64;

use derivative::Derivative;
use net_types::ip::{IpAddress, Subnet};

use crate::{InspectableValue, Inspector};

/// Trait defining required types for matchers provided by bindings.
///
/// Allows rules that match on device class to be installed, storing the
/// [`MatcherBindingsTypes::DeviceClass`] type at rest, while allowing Netstack3
/// Core to have Bindings provide the type since it is platform-specific.
pub trait MatcherBindingsTypes {
    /// The device class type for devices installed in the netstack.
    type DeviceClass: Clone + Debug;
}

/// Common pattern to define a matcher for a metadata input `T`.
///
/// Used in matching engines like filtering and routing rules.
pub trait Matcher<T> {
    /// Returns whether the provided value matches.
    fn matches(&self, actual: &T) -> bool;

    /// Returns whether the provided value is set and matches.
    fn required_matches(&self, actual: Option<&T>) -> bool {
        actual.map_or(false, |actual| self.matches(actual))
    }
}

/// Implement `Matcher` for optional matchers, so that if a matcher is left
/// unspecified, it matches all inputs by default.
impl<T, O> Matcher<T> for Option<O>
where
    O: Matcher<T>,
{
    fn matches(&self, actual: &T) -> bool {
        self.as_ref().map_or(true, |expected| expected.matches(actual))
    }

    fn required_matches(&self, actual: Option<&T>) -> bool {
        self.as_ref().map_or(true, |expected| expected.required_matches(actual))
    }
}

/// Matcher that matches IP addresses in a subnet.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SubnetMatcher<A: IpAddress>(pub Subnet<A>);

impl<A: IpAddress> Matcher<A> for SubnetMatcher<A> {
    fn matches(&self, actual: &A) -> bool {
        let Self(matcher) = self;
        matcher.contains(actual)
    }
}

/// A matcher for network interfaces.
#[derive(Clone, Derivative, PartialEq, Eq)]
#[derivative(Debug)]
pub enum InterfaceMatcher<DeviceClass> {
    /// The ID of the interface as assigned by the netstack.
    Id(NonZeroU64),
    /// Match based on name.
    Name(String),
    /// The device class of the interface.
    DeviceClass(DeviceClass),
}

impl<DeviceClass: Debug> InspectableValue for InterfaceMatcher<DeviceClass> {
    fn record<I: Inspector>(&self, name: &str, inspector: &mut I) {
        match self {
            InterfaceMatcher::Id(id) => inspector.record_string(name, format!("Id({})", id.get())),
            InterfaceMatcher::Name(iface_name) => {
                inspector.record_string(name, format!("Name({iface_name})"))
            }
            InterfaceMatcher::DeviceClass(class) => {
                inspector.record_debug(name, format!("Class({class:?})"))
            }
        };
    }
}

/// Allows code to match on properties of an interface (ID, name, and device
/// class) without Netstack3 Core (or Bindings, in the case of the device class)
/// having to specifically expose that state.
pub trait InterfaceProperties<DeviceClass> {
    /// Returns whether the provided ID matches the interface.
    fn id_matches(&self, id: &NonZeroU64) -> bool;

    /// Returns whether the provided name matches the interface.
    fn name_matches(&self, name: &str) -> bool;

    /// Returns whether the provided device class matches the interface.
    fn device_class_matches(&self, device_class: &DeviceClass) -> bool;
}

impl<DeviceClass, I: InterfaceProperties<DeviceClass>> Matcher<I>
    for InterfaceMatcher<DeviceClass>
{
    fn matches(&self, actual: &I) -> bool {
        match self {
            InterfaceMatcher::Id(id) => actual.id_matches(id),
            InterfaceMatcher::Name(name) => actual.name_matches(name),
            InterfaceMatcher::DeviceClass(device_class) => {
                actual.device_class_matches(device_class)
            }
        }
    }
}

/// Matcher for the bound device of locally generated traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundInterfaceMatcher<DeviceClass> {
    /// The packet is bound to a device which is matched by the matcher.
    Bound(InterfaceMatcher<DeviceClass>),
    /// There is no bound device.
    Unbound,
}

impl<'a, DeviceClass, D: InterfaceProperties<DeviceClass>> Matcher<Option<&'a D>>
    for BoundInterfaceMatcher<DeviceClass>
{
    fn matches(&self, actual: &Option<&'a D>) -> bool {
        match self {
            BoundInterfaceMatcher::Bound(matcher) => matcher.required_matches(actual.as_deref()),
            BoundInterfaceMatcher::Unbound => actual.is_none(),
        }
    }
}

impl<DeviceClass: Debug> InspectableValue for BoundInterfaceMatcher<DeviceClass> {
    fn record<I: Inspector>(&self, name: &str, inspector: &mut I) {
        match self {
            BoundInterfaceMatcher::Unbound => inspector.record_str(name, "Unbound"),
            BoundInterfaceMatcher::Bound(interface) => {
                inspector.record_inspectable_value(name, interface)
            }
        }
    }
}

#[cfg(any(test, feature = "testutils"))]
pub(crate) mod testutil {
    use alloc::string::String;
    use core::num::NonZeroU64;

    use crate::matchers::InterfaceProperties;
    use crate::testutil::{FakeDeviceClass, FakeStrongDeviceId, FakeWeakDeviceId};
    use crate::{DeviceIdentifier, StrongDeviceIdentifier};

    /// A fake device ID for testing matchers.
    #[derive(Clone, Debug, PartialOrd, Ord, PartialEq, Eq, Hash)]
    #[allow(missing_docs)]
    pub struct FakeMatcherDeviceId {
        pub id: NonZeroU64,
        pub name: String,
        pub class: FakeDeviceClass,
    }

    #[allow(missing_docs)]
    impl FakeMatcherDeviceId {
        pub fn wlan_interface() -> FakeMatcherDeviceId {
            FakeMatcherDeviceId {
                id: NonZeroU64::new(1).unwrap(),
                name: String::from("wlan"),
                class: FakeDeviceClass::Wlan,
            }
        }

        pub fn ethernet_interface() -> FakeMatcherDeviceId {
            FakeMatcherDeviceId {
                id: NonZeroU64::new(2).unwrap(),
                name: String::from("eth"),
                class: FakeDeviceClass::Ethernet,
            }
        }
    }

    impl StrongDeviceIdentifier for FakeMatcherDeviceId {
        type Weak = FakeWeakDeviceId<Self>;

        fn downgrade(&self) -> Self::Weak {
            FakeWeakDeviceId(self.clone())
        }
    }

    impl DeviceIdentifier for FakeMatcherDeviceId {
        fn is_loopback(&self) -> bool {
            false
        }
    }

    impl FakeStrongDeviceId for FakeMatcherDeviceId {
        fn is_alive(&self) -> bool {
            true
        }
    }

    impl PartialEq<FakeWeakDeviceId<FakeMatcherDeviceId>> for FakeMatcherDeviceId {
        fn eq(&self, FakeWeakDeviceId(other): &FakeWeakDeviceId<FakeMatcherDeviceId>) -> bool {
            self == other
        }
    }

    impl InterfaceProperties<FakeDeviceClass> for FakeMatcherDeviceId {
        fn id_matches(&self, id: &NonZeroU64) -> bool {
            &self.id == id
        }

        fn name_matches(&self, name: &str) -> bool {
            &self.name == name
        }

        fn device_class_matches(&self, class: &FakeDeviceClass) -> bool {
            &self.class == class
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use ip_test_macro::ip_test;
    use net_types::ip::Ip;

    use super::*;
    use crate::testutil::{FakeDeviceId, TestIpExt};

    /// Only matches `true`.
    #[derive(Debug)]
    struct TrueMatcher;

    impl Matcher<bool> for TrueMatcher {
        fn matches(&self, actual: &bool) -> bool {
            *actual
        }
    }

    #[test]
    fn test_optional_matcher_optional_value() {
        assert!(TrueMatcher.matches(&true));
        assert!(!TrueMatcher.matches(&false));

        assert!(TrueMatcher.required_matches(Some(&true)));
        assert!(!TrueMatcher.required_matches(Some(&false)));
        assert!(!TrueMatcher.required_matches(None));

        assert!(Some(TrueMatcher).matches(&true));
        assert!(!Some(TrueMatcher).matches(&false));
        assert!(None::<TrueMatcher>.matches(&true));
        assert!(None::<TrueMatcher>.matches(&false));

        assert!(Some(TrueMatcher).required_matches(Some(&true)));
        assert!(!Some(TrueMatcher).required_matches(Some(&false)));
        assert!(!Some(TrueMatcher).required_matches(None));
        assert!(None::<TrueMatcher>.required_matches(Some(&true)));
        assert!(None::<TrueMatcher>.required_matches(Some(&false)));
        assert!(None::<TrueMatcher>.required_matches(None));
    }

    #[test]
    fn device_name_matcher() {
        let device = FakeDeviceId;
        let positive_matcher = InterfaceMatcher::Name(FakeDeviceId::FAKE_NAME.into());
        let negative_matcher =
            InterfaceMatcher::Name(format!("DONTMATCH-{}", FakeDeviceId::FAKE_NAME));
        assert!(positive_matcher.matches(&device));
        assert!(!negative_matcher.matches(&device));
    }

    #[ip_test(I)]
    fn subnet_matcher<I: Ip + TestIpExt>() {
        let matcher = SubnetMatcher(I::TEST_ADDRS.subnet);
        assert!(matcher.matches(&I::TEST_ADDRS.local_ip));
        assert!(!matcher.matches(&I::get_other_remote_ip_address(1)));
    }
}
