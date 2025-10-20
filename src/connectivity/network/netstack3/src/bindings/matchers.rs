// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::convert::Infallible as Never;

use crate::bindings::util::{IntoCore, TryFromFidl};
use crate::bindings::{BindingsCtx, MatcherBindingsTypes};
use fidl_fuchsia_net_matchers_ext as fnet_matchers_ext;

impl TryFromFidl<fnet_matchers_ext::Interface>
    for netstack3_core::device::InterfaceMatcher<<BindingsCtx as MatcherBindingsTypes>::DeviceClass>
{
    type Error = Never;

    fn try_from_fidl(fidl: fnet_matchers_ext::Interface) -> Result<Self, Self::Error> {
        Ok(match fidl {
            fnet_matchers_ext::Interface::Id(id) => Self::Id(id),
            fnet_matchers_ext::Interface::Name(name) => Self::Name(name),
            fnet_matchers_ext::Interface::PortClass(class) => Self::DeviceClass(class.into()),
        })
    }
}

impl TryFromFidl<fnet_matchers_ext::BoundInterface>
    for netstack3_core::device::BoundInterfaceMatcher<
        <BindingsCtx as MatcherBindingsTypes>::DeviceClass,
    >
{
    type Error = Never;

    fn try_from_fidl(fidl: fnet_matchers_ext::BoundInterface) -> Result<Self, Self::Error> {
        match fidl {
            fnet_matchers_ext::BoundInterface::Bound(matcher) => {
                Ok(netstack3_core::device::BoundInterfaceMatcher::Bound(matcher.into_core()))
            }
            fnet_matchers_ext::BoundInterface::Unbound => {
                Ok(netstack3_core::device::BoundInterfaceMatcher::Unbound)
            }
        }
    }
}

impl TryFromFidl<fnet_matchers_ext::Mark> for netstack3_core::ip::MarkMatcher {
    type Error = Never;

    fn try_from_fidl(fidl: fnet_matchers_ext::Mark) -> Result<Self, Self::Error> {
        Ok(match fidl {
            fnet_matchers_ext::Mark::Unmarked => netstack3_core::ip::MarkMatcher::Unmarked,
            fnet_matchers_ext::Mark::Marked { mask, between, invert } => {
                netstack3_core::ip::MarkMatcher::Marked {
                    mask,
                    start: *between.start(),
                    end: *between.end(),
                    invert,
                }
            }
        })
    }
}

impl TryFromFidl<fnet_matchers_ext::Port> for netstack3_core::ip::PortMatcher {
    type Error = Never;

    fn try_from_fidl(fidl: fnet_matchers_ext::Port) -> Result<Self, Self::Error> {
        Ok(netstack3_core::ip::PortMatcher { range: fidl.range().clone(), invert: fidl.invert })
    }
}
