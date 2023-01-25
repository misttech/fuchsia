// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::{
    collections::BTreeSet,
    ffi::{CStr, FromBytesWithNulError},
};

use byteorder::{BigEndian, ByteOrder};
use serde_json::json;

use crate::{path::JsonPath, validator::property_type::PropertyType};

use super::{fixups::DevicetreeFixupError, types::PropertyTypeLookup};

#[derive(thiserror::Error, Debug)]
pub enum DevicetreeJsonError {
    #[error("String was not valid: {1}")]
    InvalidString(#[source] FromBytesWithNulError, JsonPath),

    #[error("String has non-printable characters")]
    NonPrintableString(JsonPath),

    #[error("Error serialising JSON value")]
    JsonError(#[from] serde_json::Error),

    #[error("Invalid unicode")]
    Utf8Error(#[from] std::str::Utf8Error),

    #[error("Error performing fixups")]
    FixupError(#[from] DevicetreeFixupError),
}

#[derive(Debug, PartialEq, Clone)]
pub struct Property {
    pub(super) key: String,
    pub(super) value: Vec<u8>,
}

impl Property {
    #[tracing::instrument(level = "info", skip_all, fields(property=self.key))]
    pub fn value_json(
        &self,
        nodename: &str,
        type_lookup: &dyn PropertyTypeLookup,
        path: JsonPath,
    ) -> Result<serde_json::Value, DevicetreeJsonError> {
        if self.value.is_empty() {
            return Ok(json!(true));
        }

        if nodename == "__fixups__" || nodename == "aliases" {
            return self.as_json_string(path);
        }

        let mut types = type_lookup.get_property_type(&self.key);
        types.remove(&PropertyType::Node);
        if types.len() > 1 {
            // If we have more than one possible type, try and reduce that set.
            types = types
                .iter()
                .filter(|t| {
                    // Filter out types which |self.value| is too big for.
                    t.max_size()
                        .map(|max| self.value.len() <= max)
                        .unwrap_or(true)
                })
                .copied()
                .collect::<BTreeSet<_>>();
        }

        tracing::trace!("Possible types: {:?}", types);

        #[allow(clippy::comparison_chain)]
        let chosen_type = if types.len() > 1 {
            if types.contains(&PropertyType::String) || types.contains(&PropertyType::StringArray) {
                // Try and treat it as a string.
                if let Ok(str) = self.as_json_string(path.clone()) {
                    return Ok(str);
                }

                let string_types =
                    BTreeSet::from([PropertyType::String, PropertyType::StringArray]);
                // There should only be one other type.
                let difference = types.difference(&string_types).collect::<Vec<_>>();
                if difference.len() == 1 {
                    difference.into_iter().next().copied()
                } else {
                    tracing::warn!(
                        "Property should only have one type left, but have these types: {:?}",
                        difference
                    );
                    return self.as_json_bytes();
                }
            } else {
                None
            }
        } else if types.len() == 1 {
            types.pop_first()
        } else {
            None
        };

        let chosen_type = if let Some(ty) = chosen_type {
            ty
        } else {
            if let Ok(value) = self.as_json_string(path.clone()) {
                return Ok(value);
            }

            if self.value.len() % 4 == 0 {
                PropertyType::Uint32Array
            } else {
                return self.as_json_bytes();
            }
        };
        tracing::trace!("Chosen type: {:?}", chosen_type);

        if chosen_type.is_string() {
            return self.as_json_string(path);
        }

        if chosen_type == PropertyType::Flag {
            assert!(
                !self.value.is_empty(),
                "If length is zero we should always return true above"
            );
            tracing::warn!("Property is a flag but has data!");
            return self.as_json_bytes();
        }

        // This is slightly different than the upstream equivalent
        // (see https://github.com/devicetree-org/dt-schema/blob/547c943ab55f4d0b44fd88e3c36c7f6fa49c6ae2/dtschema/dtb.py#L137)
        // but I think it is functionally the same.
        let bytes_per_element = match chosen_type.bytes_per_element() {
            Some(bytes) => bytes,
            None => {
                tracing::warn!("Type {:?} has no bytes per element", chosen_type);
                return self.as_json_bytes();
            }
        };

        if self.value.len() % bytes_per_element != 0 {
            tracing::warn!(
                "Invalid size: {} (expected a multiple of {})",
                self.value.len(),
                bytes_per_element
            );
            return self.as_json_bytes();
        }

        let values_int = self
            .value
            .chunks_exact(bytes_per_element)
            .map(|chunk| BigEndian::read_uint(chunk, bytes_per_element))
            .collect::<Vec<_>>();

        let dim = if chosen_type.is_matrix() {
            type_lookup.get_property_dimensions(&self.key)
        } else {
            None
        };

        if let Some(dim) = dim {
            let stride = dim.stride(values_int.len());

            return Ok(serde_json::to_value(
                values_int
                    .chunks(stride)
                    .map(Vec::from)
                    .collect::<Vec<Vec<u64>>>(),
            )?);
        } else {
            Ok(serde_json::to_value([values_int])?)
        }
    }

    fn as_json_bytes(&self) -> Result<serde_json::Value, DevicetreeJsonError> {
        Ok(serde_json::to_value(self.value.clone())?)
    }

    fn as_json_string(&self, path: JsonPath) -> Result<serde_json::Value, DevicetreeJsonError> {
        let slices = self
            .value
            .split_inclusive(|&byte| byte == 0)
            .map(|v| {
                CStr::from_bytes_with_nul(v)
                    .map_err(|e| DevicetreeJsonError::InvalidString(e, path.clone()))
                    .and_then(|v| match v.to_str() {
                        Ok(str) => {
                            if str.chars().any(char::is_control) {
                                Err(DevicetreeJsonError::NonPrintableString(path.clone()))
                            } else {
                                Ok(str)
                            }
                        }
                        Err(e) => Err(e.into()),
                    })
            })
            .collect::<Result<Vec<&str>, DevicetreeJsonError>>()?;

        Ok(serde_json::to_value(slices)?)
    }

    pub fn key(&self) -> &String {
        &self.key
    }
}

#[cfg(test)]
mod tests {

    use crate::validator::dimension::Dimension;

    use super::*;

    fn make_prop(key: &str, value: &[u8]) -> Property {
        Property {
            key: key.to_owned(),
            value: Vec::from(value),
        }
    }

    struct FakeTypeLookup {
        types: BTreeSet<PropertyType>,
        dimensions: Option<Dimension>,
    }
    impl FakeTypeLookup {
        fn empty() -> Self {
            FakeTypeLookup {
                types: BTreeSet::new(),
                dimensions: None,
            }
        }

        fn new(types: &[PropertyType], dim: Option<Dimension>) -> Self {
            FakeTypeLookup {
                types: types.iter().copied().collect(),
                dimensions: dim,
            }
        }
    }

    impl PropertyTypeLookup for FakeTypeLookup {
        fn get_property_type(&self, _propname: &str) -> BTreeSet<PropertyType> {
            self.types.clone()
        }

        fn get_property_dimensions(
            &self,
            _propname: &str,
        ) -> Option<crate::validator::dimension::Dimension> {
            self.dimensions
        }
    }

    #[test]
    fn test_flag_property() {
        let prop = make_prop("test", &[]);
        assert_eq!(
            prop.value_json("test", &FakeTypeLookup::empty(), JsonPath::new())
                .expect("valid value"),
            json!(true)
        );
    }

    #[test]
    fn test_no_types() {
        let prop = make_prop("test", &[1, 2, 3]);
        assert_eq!(
            prop.value_json("test", &FakeTypeLookup::empty(), JsonPath::new())
                .expect("valid value"),
            json!([1, 2, 3])
        )
    }

    #[test]
    fn test_string_array() {
        let prop = make_prop("test_string_array", &[b'h', b'i', 0, b'h', b'i', 0]);
        let lookup = FakeTypeLookup::new(&[PropertyType::String], None);

        assert_eq!(
            prop.value_json("test", &lookup, JsonPath::new())
                .expect("valid value"),
            json!(["hi", "hi"])
        );
    }

    #[test]
    fn test_single_string() {
        let prop = make_prop("test_single_string", &[b'h', b'i', 0]);
        let lookup = FakeTypeLookup::new(&[PropertyType::String], None);

        assert_eq!(
            prop.value_json("test", &lookup, JsonPath::new())
                .expect("valid value"),
            json!(["hi"])
        );
    }

    #[test]
    fn test_string_ambiguous() {
        let prop = make_prop(
            "test_string_ambiguous",
            &[0x04, 0x07, 0x08, 0x02, 0x03, 0x09, 0x04, 0x06],
        );
        let lookup = FakeTypeLookup::new(&[PropertyType::String, PropertyType::Uint32Array], None);

        assert_eq!(
            prop.value_json("test", &lookup, JsonPath::new())
                .expect("valid value"),
            json!([[0x04070802, 0x03090406]])
        );
    }

    #[test]
    fn test_untyped_bytes() {
        let prop = make_prop("untyped_bytes", &[0x04, 0x05]);
        let lookup = FakeTypeLookup::new(&[], None);

        assert_eq!(
            prop.value_json("test", &lookup, JsonPath::new())
                .expect("valid value"),
            json!([0x04, 0x05])
        );
    }

    #[test]
    fn test_explicit_flag_type_with_data() {
        let prop = make_prop("explicit_flag_type", &[0x01, 0x02]);
        let lookup = FakeTypeLookup::new(&[PropertyType::Flag], None);

        assert_eq!(
            prop.value_json("test", &lookup, JsonPath::new())
                .expect("valid value"),
            json!([0x01, 0x02])
        );
    }

    #[test]
    fn test_u64_array() {
        let prop = make_prop(
            "u64_array",
            &[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, /* */
                0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
            ],
        );

        let lookup = FakeTypeLookup::new(&[PropertyType::Uint64Array], None);
        assert_eq!(
            prop.value_json("test", &lookup, JsonPath::new())
                .expect("valid value"),
            json!([[0xff, 0x1000000000000001_u64]])
        );
    }

    #[test]
    fn test_u32_matrix_simple() {
        let prop = make_prop(
            "u32_matrix",
            &[
                0x00, 0x00, 0x00, 0x01, /* */
                0x00, 0x00, 0x00, 0x01, /* */
                0x00, 0x00, 0x00, 0x04, /* */
                0x00, 0x00, 0x00, 0x04,
            ],
        );

        let lookup =
            FakeTypeLookup::new(&[PropertyType::Uint32Matrix], Some([[1, 1], [2, 2]].into()));

        assert_eq!(
            prop.value_json("test", &lookup, JsonPath::new())
                .expect("valid value"),
            json!([[1, 1], [4, 4]])
        );
    }

    #[test]
    fn test_u32_matrix_variable() {
        let prop = make_prop(
            "u32_matrix_variable",
            &[
                0x00, 0x00, 0x00, 0x01, /* */
                0x00, 0x00, 0x00, 0x01, /* */
                0x00, 0x00, 0x00, 0x04, /* */
                0x00, 0x00, 0x00, 0x04, /* */
                0x00, 0x00, 0x00, 0x05, /* */
                0x00, 0x00, 0x00, 0x06, /* */
            ],
        );

        let lookup =
            FakeTypeLookup::new(&[PropertyType::Uint32Matrix], Some([[3, 3], [1, 2]].into()));

        assert_eq!(
            prop.value_json("test", &lookup, JsonPath::new())
                .expect("valid value"),
            json!([[1, 1, 4], [4, 5, 6]])
        );
    }
}
