// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};

use super::util::{ContainsExt, IsSchema};

#[derive(Deserialize, Debug, Serialize)]
#[serde(untagged)]
/// Represents an "Items" declaration, which could be a bare list of items, or a dict with a min/max.
/// This is only ever used in the context of |Array|, below.
enum Items {
    Array(Vec<serde_json::Value>),
    Object(serde_json::Map<String, serde_json::Value>),
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
/// Represents an array.
pub struct Array {
    items: Option<Items>,
    min_items: Option<usize>,
    max_items: Option<usize>,
}

impl Array {
    /// Returns true if this |Array| represents an array of ints.
    pub fn is_int_array_schema(&self, propname: &str) -> bool {
        let unit_type_re = Regex::new("-(kBps|bits|percent|bp|m?hz|sec|ms|us|ns|ps|mm|nanoamp|(micro-)?ohms|micro(amp|watt)(-hours)?|milliwatt|microvolt|picofarads|(milli)?celsius|kelvin|kpascal)$").unwrap();
        if unit_type_re.is_match(propname).unwrap_or(false) {
            return true;
        }
        let int_array_re = Regex::new("int(8|16|32|64)-array").unwrap();
        let map = match self.items {
            Some(Items::Object(ref o)) => {
                if let Some(all_of) = o.get("allOf").and_then(|v| v.as_array()) {
                    let mut ret = o;
                    for item in all_of.iter().filter_map(|v| v.as_object()) {
                        if let Some(path) = item.get("$ref").and_then(|v| v.as_str()) {
                            return int_array_re.is_match(path).unwrap_or(false);
                        }
                        if let Some(items) = item.get("items").and_then(|v| v.as_object()) {
                            ret = items;
                        }
                    }
                    ret
                } else {
                    o
                }
            }
            _ => return false,
        };

        map.is_int_schema()
    }

    /// Returns |true| if this type represents a uint32 matrix.
    pub fn is_uint32_matrix(&self, propname: &str) -> bool {
        if !self.is_matrix() {
            false
        } else {
            let unit_type_re = Regex::new("-(kBps|bits|percent|bp|m?hz|sec|ms|us|ns|ps|mm|nanoamp|(micro-)?ohms|micro(amp|watt)(-hours)?|milliwatt|microvolt|picofarads|(milli)?celsius|kelvin|kpascal)$").unwrap();
            unit_type_re.is_match(propname).unwrap_or(false)
        }
    }

    /// Returns true if this array is a matrix.
    pub fn is_matrix(&self) -> bool {
        match &self.items {
            Some(Items::Object(map)) => map.contains_any(&["items", "maxItems", "minItems"]),
            Some(Items::Array(array)) => {
                for item in array.iter().filter_map(|f| f.as_object()) {
                    if item.contains_any(&["items", "maxItems", "minItems"]) {
                        return true;
                    }
                }
                false
            }
            None => false,
        }
    }

    /// If this array is a matrix, returns the inner dimension of the matrix.
    pub fn get_child_dim(&self) -> Result<[usize; 2], serde_json::Error> {
        let array: Array = match &self.items {
            Some(Items::Array(vec)) => {
                if vec.len() == 1 {
                    serde_json::from_value(vec[0].clone())?
                } else {
                    return Ok([0, 0]);
                }
            }
            Some(Items::Object(obj)) => serde_json::from_value(obj.clone().into())?,
            None => return Ok([1, 0]),
        };

        Ok(array.get_dim())
    }

    /// Get the [minimum, maximum] length of this array.
    pub fn get_dim(&self) -> [usize; 2] {
        if let Some(Items::Array(ref items)) = self.items {
            [self.min_items.unwrap_or(items.len()), items.len()]
        } else {
            [
                self.min_items.unwrap_or(1),
                self.max_items
                    .unwrap_or_else(|| self.min_items.unwrap_or(0)),
            ]
        }
    }

    /// Returns true if this array has an explicit |items| definition associated with it.
    pub fn has_items(&self) -> bool {
        self.items.is_some()
    }

    pub fn is_string_schema(&self) -> bool {
        match self.items.as_ref() {
            // TODO(simonshields): should this be less aggressive? To match the upstream code it should be
            // array.iter().first().and_then(|v| v.as_object()).any(|v| v.is_string_schema())
            Some(Items::Array(array)) => array
                .iter()
                .filter_map(|v| v.as_object())
                .any(|v| v.is_string_schema()),
            Some(Items::Object(map)) => map.is_string_schema(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn items_with_len(len: usize) -> Items {
        let mut vec = Vec::new();
        vec.resize(len, json!(null));

        Items::Array(vec)
    }

    #[test]
    fn test_array_get_dim() {
        assert_eq!(
            Array {
                items: None,
                min_items: Some(2),
                max_items: Some(3),
            }
            .get_dim(),
            [2, 3]
        );

        assert_eq!(
            Array {
                items: Some(items_with_len(3)),
                min_items: None,
                max_items: None,
            }
            .get_dim(),
            [3, 3]
        );

        assert_eq!(
            Array {
                items: Some(items_with_len(4)),
                min_items: Some(2),
                max_items: Some(3),
            }
            .get_dim(),
            [2, 4]
        )
    }
}
