// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Trait definition for matchers.

use alloc::format;
use alloc::string::String;
use core::fmt::Debug;
use core::num::NonZeroU64;
use core::ops::RangeInclusive;

use derivative::Derivative;
use net_types::ip::{IpAddress, Subnet};

use crate::{InspectableValue, Inspector, Mark, MarkDomain, MarkStorage, Marks};

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

/// A matcher to the socket mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkMatcher {
    /// Matches a packet if it is unmarked.
    Unmarked,
    /// The packet carries a mark that is in the range after masking.
    Marked {
        /// The mask to apply.
        mask: u32,
        /// Start of the range, inclusive.
        start: u32,
        /// End of the range, inclusive.
        end: u32,
        /// Inverts the meaning of the match.
        invert: bool,
    },
}

impl Matcher<Mark> for MarkMatcher {
    fn matches(&self, Mark(actual): &Mark) -> bool {
        match self {
            MarkMatcher::Unmarked => actual.is_none(),
            MarkMatcher::Marked { mask, start, end, invert } => {
                let val = actual.is_some_and(|actual| (*start..=*end).contains(&(actual & *mask)));

                if *invert { !val } else { val }
            }
        }
    }
}

/// The 2 mark matchers a rule can specify. All non-none markers must match.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkMatchers(MarkStorage<Option<MarkMatcher>>);

impl MarkMatchers {
    /// Creates [`MarkMatcher`]s from an iterator of `(MarkDomain, MarkMatcher)`.
    ///
    /// An unspecified domain will not have a matcher.
    ///
    /// # Panics
    ///
    /// Panics if the same domain is specified more than once.
    pub fn new(matchers: impl IntoIterator<Item = (MarkDomain, MarkMatcher)>) -> Self {
        MarkMatchers(MarkStorage::new(matchers))
    }

    /// Returns an iterator over the mark matchers of all domains.
    pub fn iter(&self) -> impl Iterator<Item = (MarkDomain, &Option<MarkMatcher>)> {
        let Self(storage) = self;
        storage.iter()
    }
}

impl Matcher<Marks> for MarkMatchers {
    fn matches(&self, actual: &Marks) -> bool {
        let Self(matchers) = self;
        matchers.zip_with(actual).all(|(_domain, matcher, actual)| matcher.matches(actual))
    }
}

/// A matcher for transport-layer port numbers.
#[derive(Clone, Debug)]
pub struct PortMatcher {
    /// The range of port numbers in which the tested port number must fall.
    pub range: RangeInclusive<u16>,
    /// Whether to check for an "inverse" or "negative" match (in which case,
    /// if the matcher criteria do *not* apply, it *is* considered a match, and
    /// vice versa).
    pub invert: bool,
}

impl Matcher<u16> for PortMatcher {
    fn matches(&self, actual: &u16) -> bool {
        let Self { range, invert } = self;
        range.contains(actual) ^ *invert
    }
}

/// A matcher for IP addresses.
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub enum AddressMatcherType<A: IpAddress> {
    /// A subnet that must contain the address.
    #[derivative(Debug = "transparent")]
    Subnet(SubnetMatcher<A>),
    /// An inclusive range of IP addresses that must contain the address.
    Range(RangeInclusive<A>),
}

impl<A: IpAddress> Matcher<A> for AddressMatcherType<A> {
    fn matches(&self, actual: &A) -> bool {
        match self {
            Self::Subnet(subnet_matcher) => subnet_matcher.matches(actual),
            Self::Range(range) => range.contains(actual),
        }
    }
}

/// A matcher for IP addresses.
#[derive(Clone, Debug)]
pub struct AddressMatcher<A: IpAddress> {
    /// The type of the address matcher.
    pub matcher: AddressMatcherType<A>,
    /// Whether to check for an "inverse" or "negative" match (in which case,
    /// if the matcher criteria do *not* apply, it *is* considered a match, and
    /// vice versa).
    pub invert: bool,
}

impl<A: IpAddress> InspectableValue for AddressMatcher<A> {
    fn record<I: Inspector>(&self, name: &str, inspector: &mut I) {
        inspector.record_debug(name, self);
    }
}

impl<A: IpAddress> Matcher<A> for AddressMatcher<A> {
    fn matches(&self, addr: &A) -> bool {
        let Self { matcher, invert } = self;
        matcher.matches(addr) ^ *invert
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
    use net_types::Witness;
    use net_types::ip::Ip;
    use test_case::test_case;

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

    #[test_case(MarkMatcher::Unmarked, Mark(None) => true; "unmarked matches none")]
    #[test_case(MarkMatcher::Unmarked, Mark(Some(0)) => false; "unmarked does not match some")]
    #[test_case(MarkMatcher::Marked {
        mask: 1,
        start: 0,
        end: 0,
        invert: false,
    }, Mark(None) => false; "marked does not match none")]
    #[test_case(MarkMatcher::Marked {
        mask: 1,
        start: 0,
        end: 0,
        invert: false,
    }, Mark(Some(0)) => true; "marked 0 mask 1 matches 0")]
    #[test_case(MarkMatcher::Marked {
        mask: 1,
        start: 0,
        end: 0,
        invert: false,
    }, Mark(Some(1)) => false; "marked 0 mask 1 does not match 1")]
    #[test_case(MarkMatcher::Marked {
        mask: 1,
        start: 0,
        end: 0,
        invert: false,
    }, Mark(Some(2)) => true; "marked 0 mask 1 matches 2")]
    #[test_case(MarkMatcher::Marked {
        mask: 1,
        start: 0,
        end: 0,
        invert: false,
    }, Mark(Some(3)) => false; "marked 0 mask 1 does not match 3")]
    #[test_case(MarkMatcher::Marked {
        mask: !0,
        start: 0,
        end: 10,
        invert: true,
    }, Mark(Some(5)) => false; "marked invert no match in range")]
    #[test_case(MarkMatcher::Marked {
        mask: !0,
        start: 0,
        end: 10,
        invert: true,
    }, Mark(Some(11)) => true; "marked invert matches out of range")]
    fn mark_matcher(matcher: MarkMatcher, mark: Mark) -> bool {
        matcher.matches(&mark)
    }

    #[test_case(
        MarkMatchers::new(
            [(MarkDomain::Mark1, MarkMatcher::Unmarked),
            (MarkDomain::Mark2, MarkMatcher::Unmarked)]
        ),
        Marks::new([]) => true;
        "all unmarked matches empty"
    )]
    #[test_case(
        MarkMatchers::new(
            [(MarkDomain::Mark1, MarkMatcher::Unmarked),
            (MarkDomain::Mark2, MarkMatcher::Unmarked)]
        ),
        Marks::new([(MarkDomain::Mark1, 1)]) => false;
        "all unmarked does not match mark1"
    )]
    #[test_case(
        MarkMatchers::new(
            [(MarkDomain::Mark1, MarkMatcher::Unmarked),
            (MarkDomain::Mark2, MarkMatcher::Unmarked)]
        ),
        Marks::new([(MarkDomain::Mark2, 1)]) => false;
        "all unmarked does not match mark2"
    )]
    #[test_case(
        MarkMatchers::new(
            [(MarkDomain::Mark1, MarkMatcher::Unmarked),
            (MarkDomain::Mark2, MarkMatcher::Unmarked)]
        ),
        Marks::new([
            (MarkDomain::Mark1, 1),
            (MarkDomain::Mark2, 1),
        ]) => false;
        "all unmarked does not match mark1 and mark2"
    )]
    #[test_case(
        MarkMatchers::new(
            [(MarkDomain::Mark1, MarkMatcher::Marked { mask: !0, start: 1, end: 1, invert: false }),
            (MarkDomain::Mark2, MarkMatcher::Unmarked)]
        ),
        Marks::new([(MarkDomain::Mark1, 1)]) => true;
        "mark1 marked matches"
    )]
    #[test_case(
        MarkMatchers::new(
            [(MarkDomain::Mark1, MarkMatcher::Marked { mask: !0, start: 1, end: 1, invert: false }),
            (MarkDomain::Mark2, MarkMatcher::Unmarked)]
        ),
        Marks::new([(MarkDomain::Mark1, 2)]) => false;
        "mark1 marked no match"
    )]
    #[test_case(
        MarkMatchers::new(
            [(MarkDomain::Mark1, MarkMatcher::Marked { mask: !0, start: 1, end: 1, invert: false }),
            (MarkDomain::Mark2, MarkMatcher::Marked { mask: !0, start: 2, end: 2, invert: false })]
        ),
        Marks::new([(MarkDomain::Mark1, 1), (MarkDomain::Mark2, 2)]) => true;
        "all marked matches"
    )]
    #[test_case(
        MarkMatchers::new(
            [(MarkDomain::Mark1, MarkMatcher::Marked { mask: !0, start: 1, end: 1, invert: false }),
            (MarkDomain::Mark2, MarkMatcher::Marked { mask: !0, start: 2, end: 2, invert: false })]
        ),
        Marks::new([(MarkDomain::Mark1, 1), (MarkDomain::Mark2, 3)]) => false;
        "all marked no match mark2"
    )]
    fn mark_matchers(matchers: MarkMatchers, marks: Marks) -> bool {
        matchers.matches(&marks)
    }

    #[test_case(PortMatcher { range: 10..=20, invert: false }, 9 => false)]
    #[test_case(PortMatcher { range: 10..=20, invert: false }, 10 => true)]
    #[test_case(PortMatcher { range: 10..=20, invert: false }, 15 => true)]
    #[test_case(PortMatcher { range: 10..=20, invert: false }, 20 => true)]
    #[test_case(PortMatcher { range: 10..=20, invert: false }, 21 => false)]
    #[test_case(PortMatcher { range: 10..=20, invert: true }, 9 => true)]
    #[test_case(PortMatcher { range: 10..=20, invert: true }, 10 => false)]
    #[test_case(PortMatcher { range: 10..=20, invert: true }, 15 => false)]
    #[test_case(PortMatcher { range: 10..=20, invert: true }, 20 => false)]
    #[test_case(PortMatcher { range: 10..=20, invert: true }, 21 => true)]
    fn port_matcher(matcher: PortMatcher, actual: u16) -> bool {
        matcher.matches(&actual)
    }

    #[ip_test(I)]
    fn address_matcher_type<I: TestIpExt>() {
        let local_ip = I::TEST_ADDRS.local_ip.get();
        let remote_ip = I::TEST_ADDRS.remote_ip.get();

        let matcher = AddressMatcherType::Subnet(SubnetMatcher(I::TEST_ADDRS.subnet));
        assert!(matcher.matches(&local_ip));
        assert!(!matcher.matches(&I::get_other_remote_ip_address(1)));

        let matcher = AddressMatcherType::Range(local_ip..=remote_ip);
        assert!(matcher.matches(&local_ip));
        assert!(matcher.matches(&remote_ip));
        assert!(!matcher.matches(&I::get_other_remote_ip_address(1)));
    }

    #[ip_test(I)]
    fn address_matcher<I: TestIpExt>() {
        let local_ip = I::TEST_ADDRS.local_ip.get();
        let remote_ip = I::TEST_ADDRS.remote_ip.get();

        let matcher = AddressMatcher {
            matcher: AddressMatcherType::Subnet(SubnetMatcher(I::TEST_ADDRS.subnet)),
            invert: false,
        };
        assert!(matcher.matches(&local_ip));
        assert!(matcher.matches(&remote_ip));
        assert!(!matcher.matches(&I::get_other_remote_ip_address(1)));

        let matcher = AddressMatcher {
            matcher: AddressMatcherType::Subnet(SubnetMatcher(I::TEST_ADDRS.subnet)),
            invert: true,
        };
        assert!(!matcher.matches(&local_ip));
        assert!(!matcher.matches(&remote_ip));
        assert!(matcher.matches(&I::get_other_remote_ip_address(1)));

        let matcher = AddressMatcher {
            matcher: AddressMatcherType::Range(local_ip..=remote_ip),
            invert: false,
        };
        assert!(matcher.matches(&local_ip));
        assert!(matcher.matches(&remote_ip));
        assert!(!matcher.matches(&I::get_other_remote_ip_address(1)));

        let matcher = AddressMatcher {
            matcher: AddressMatcherType::Range(local_ip..=remote_ip),
            invert: true,
        };
        assert!(!matcher.matches(&local_ip));
        assert!(!matcher.matches(&remote_ip));
        assert!(matcher.matches(&I::get_other_remote_ip_address(1)));
    }
}
