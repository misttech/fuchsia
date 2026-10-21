// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_driver_framework::NodePropertyValue;

/// A newtype wrapper that can be used to construct [`NodePropertyValue`]-compatible values
/// for [`crate::NodeBuilder::add_property`], and to read the values of the properties reported
/// by [`crate::DriverContext::node_properties`].
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyValue(pub(crate) NodePropertyValue);

/// A property of one of the driver's parent nodes, as reported by
/// [`crate::DriverContext::node_properties`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParentProperty<'a> {
    /// The name of the parent node the property belongs to. A driver bound to a single node sees
    /// its properties under the name `default`; a composite driver sees one group per parent,
    /// plus a `default` group that repeats the primary parent's properties.
    pub parent: &'a str,
    /// The property key.
    pub key: &'a str,
    /// The property value.
    pub value: PropertyValue,
}

impl From<String> for PropertyValue {
    fn from(value: String) -> Self {
        Self(NodePropertyValue::StringValue(value))
    }
}

impl From<&str> for PropertyValue {
    fn from(value: &str) -> Self {
        Self(NodePropertyValue::StringValue(value.to_owned()))
    }
}

impl From<u32> for PropertyValue {
    fn from(value: u32) -> Self {
        Self(NodePropertyValue::IntValue(value))
    }
}

impl From<bool> for PropertyValue {
    fn from(value: bool) -> Self {
        Self(NodePropertyValue::BoolValue(value))
    }
}

impl From<&fidl_next_fuchsia_driver_framework::NodePropertyValue> for PropertyValue {
    fn from(value: &fidl_next_fuchsia_driver_framework::NodePropertyValue) -> Self {
        use fidl_next_fuchsia_driver_framework::NodePropertyValue as Next;
        Self(match value {
            Next::IntValue(v) => NodePropertyValue::IntValue(*v),
            Next::StringValue(v) => NodePropertyValue::StringValue(v.clone()),
            Next::BoolValue(v) => NodePropertyValue::BoolValue(*v),
            Next::EnumValue(v) => NodePropertyValue::EnumValue(v.clone()),
            Next::UnknownOrdinal_(unknown_ordinal) => {
                NodePropertyValue::__SourceBreaking { unknown_ordinal: *unknown_ordinal }
            }
        })
    }
}

impl PropertyValue {
    /// Creates a value from something that is string-like
    pub fn from_string(value: impl Into<String>) -> Self {
        String::into(value.into())
    }

    /// Creates a value from something that is integer-like
    pub fn from_int(value: impl Into<u32>) -> Self {
        u32::into(value.into())
    }

    /// Creates a value from something that is bool-like
    pub fn from_bool(value: impl Into<bool>) -> Self {
        bool::into(value.into())
    }

    /// Returns the value if it is an integer property.
    pub fn as_int(&self) -> Option<u32> {
        match self.0 {
            NodePropertyValue::IntValue(value) => Some(value),
            _ => None,
        }
    }

    /// Returns the value if it is a string property.
    pub fn as_str(&self) -> Option<&str> {
        match &self.0 {
            NodePropertyValue::StringValue(value) => Some(value),
            _ => None,
        }
    }

    /// Returns the value if it is a boolean property.
    pub fn as_bool(&self) -> Option<bool> {
        match self.0 {
            NodePropertyValue::BoolValue(value) => Some(value),
            _ => None,
        }
    }

    /// Returns the value if it is an enum property.
    pub fn as_enum(&self) -> Option<&str> {
        match &self.0 {
            NodePropertyValue::EnumValue(value) => Some(value),
            _ => None,
        }
    }
}
