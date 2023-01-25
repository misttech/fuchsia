// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::collections::VecDeque;

use crate::path::JsonPath;

use super::{DevicetreeFixupError, DevicetreeLookup};

#[derive(thiserror::Error, Debug)]
pub enum PhandleError {
    #[error("Invalid value in phandle array at {0}")]
    InvalidValue(JsonPath),
    #[error("node referenced by phandle ({1}) had no {0} value")]
    NodeHadNoProperty(String, JsonPath),
    #[error("phandle {0:x} is not valid in property {1}")]
    InvalidPhandle(u32, JsonPath),
}

/// A |PhandleIterator| takes an array of integer values of the format
/// [<phandle> <cell>...]... and uses a provided |propname| to determine
/// the number of cells each phandle expects as arguments.
/// For instance, take this simple example:
/// ```
/// gpio1 {
///   #gpio-cells = <2>;
/// }
///
/// gpio2 {
///   #gpio-cells = <1>;
/// }
///
/// my-gpios = <&gpio1 2 3>, <&gpio2 1>
/// ```
/// PhandleIterator over "my-gpios" with propname="#gpio-cells" would yield:
/// [[x, 2, 3], [y, 1]] where x and y are the phandles of gpio1 and gpio2 respectively.
pub struct PhandleIterator<'a> {
    array: &'a [serde_json::Value],
    lookup: &'a dyn DevicetreeLookup,
    propname: &'a str,
    path: JsonPath,
    in_iter: bool,
}

impl<'a> PhandleIterator<'a> {
    /// Create a new PhandleIterator.
    /// |array|: slice of values in the phandle array.
    /// |lookup|: helper used to lookup phandles in the tree.
    /// |path|: path (including property name) to the property we are resolving.
    /// |propname|: name of the property we are resolving.
    pub fn new(
        array: &'a [serde_json::Value],
        lookup: &'a dyn DevicetreeLookup,
        path: JsonPath,
        propname: &'a str,
    ) -> Self {
        PhandleIterator {
            array,
            lookup,
            propname,
            path,
            in_iter: false,
        }
    }
}

impl<'a> Iterator for PhandleIterator<'a> {
    type Item = Result<VecDeque<u64>, PhandleError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut iter = self.array.iter();
        let phandle: u32 = match iter
            .next()
            .map(|v| v.as_u64().and_then(|v| v.try_into().ok()))
        {
            Some(Some(p)) => p,
            Some(None) => return Some(Err(PhandleError::InvalidValue(self.path.clone()))),
            None => return None,
        };

        let taken: Option<VecDeque<u64>> = if phandle == 0xffff_ffff {
            // Calculate the next value.
            iter.take_while(|cell| cell.as_u64() != Some(0xffff_ffff))
                .map(|cell| cell.as_u64())
                .collect()
        } else {
            let mut cells = match self.lookup.get_cells_size(
                self.path.clone(),
                phandle,
                &self.lookup.get_cells_prop_for_prop(self.propname),
            ) {
                Ok(cells) => cells,
                Err(e) => return Some(Err(e)),
            };
            if self.propname == "msi-ranges" {
                cells += 1;
            }
            iter.take(cells as usize)
                .map(|cell| cell.as_u64())
                .collect()
        };

        match taken {
            Some(mut ret) => {
                ret.push_front(phandle.into());
                self.array = &self.array[ret.len()..];
                if self.propname == "interconnects" && !self.in_iter {
                    self.in_iter = true;
                    // Do it again for interconnects, because it expects
                    // two values at a time.
                    let next = self.next();
                    self.in_iter = false;
                    match next {
                        Some(Ok(v)) => ret.extend(v),
                        Some(Err(e)) => return Some(Err(e)),
                        None => {
                            if crate::strict_mode() {
                                return Some(Err(PhandleError::InvalidValue(self.path.clone())));
                            } else {
                                tracing::warn!("interconnects should have a pair of values!");
                            }
                        }
                    }
                }
                Some(Ok(ret))
            }
            None => Some(Err(PhandleError::InvalidValue(self.path.clone()))),
        }
    }
}

pub fn get_cells_property_from_json(
    node: &serde_json::Map<String, serde_json::Value>,
    path: &JsonPath,
    name: &str,
) -> Result<u64, DevicetreeFixupError> {
    match node.get(name) {
        Some(serde_json::Value::Array(array)) => {
            if array.len() != 1
                || !array[0]
                    .as_array()
                    .map(|v| v.len() == 1 && v[0].is_u64())
                    .unwrap_or(false)
            {
                return Err(DevicetreeFixupError::UnexpectedPropertyFormat(
                    path.extend(name),
                    array.clone().into(),
                ));
            }

            let value = array[0].as_array().unwrap()[0].as_u64().unwrap();
            Ok(value)
        }
        Some(not_number) => Err(DevicetreeFixupError::UnexpectedPropertyFormat(
            path.extend(name),
            not_number.clone(),
        )),
        None => Err(DevicetreeFixupError::MissingProperty(path.extend(name))),
    }
}

#[cfg(test)]
pub mod for_tests {
    use std::collections::{BTreeSet, HashMap};

    use crate::{
        devicetree::{fixups::DevicetreeLookup, types::PropertyTypeLookup},
        path::JsonPath,
        validator::{dimension::Dimension, property_type::PropertyType},
    };

    use super::PhandleError;

    pub struct FakeLookup {
        cells_by_phandle: HashMap<(u32, String), u32>,
        values_from_parents: HashMap<String, u32>,
        type_info: HashMap<String, (BTreeSet<PropertyType>, Option<Dimension>)>,
    }

    impl FakeLookup {
        pub fn new() -> Self {
            FakeLookup {
                cells_by_phandle: HashMap::new(),
                values_from_parents: HashMap::new(),
                type_info: HashMap::new(),
            }
        }

        /// Registers |prop| for |phandle| to be returned by |get_cells_size|.
        pub fn with_phandle(mut self, phandle: u32, prop: &str, cells: u32) -> Self {
            self.cells_by_phandle
                .insert((phandle, prop.to_owned()), cells);
            self
        }

        /// Registers |prop| to be returned by |get_cells_size_parents|.
        pub fn with_parent(mut self, prop: &str, cells: u32) -> Self {
            self.values_from_parents.insert(prop.to_owned(), cells);
            self
        }

        pub fn with_prop_type(
            mut self,
            prop: &str,
            types: &[PropertyType],
            dim: Option<Dimension>,
        ) -> Self {
            self.type_info
                .insert(prop.to_owned(), (types.iter().copied().collect(), dim));
            self
        }
    }

    impl DevicetreeLookup for FakeLookup {
        fn get_cells_size(
            &self,
            phandle_path: JsonPath,
            phandle: u32,
            cell_prop: &str,
        ) -> Result<u32, PhandleError> {
            self.cells_by_phandle
                .get(&(phandle, cell_prop.to_owned()))
                .copied()
                .ok_or_else(|| PhandleError::NodeHadNoProperty(cell_prop.to_owned(), phandle_path))
        }

        fn get_prop_from_parents(&self, prop: &str) -> Result<Option<u32>, PhandleError> {
            Ok(self.values_from_parents.get(prop).copied())
        }
    }

    impl PropertyTypeLookup for FakeLookup {
        fn get_property_type(&self, propname: &str) -> BTreeSet<PropertyType> {
            self.type_info
                .get(propname)
                .map(|v| v.0.clone())
                .unwrap_or_default()
        }

        fn get_property_dimensions(
            &self,
            propname: &str,
        ) -> Option<crate::validator::dimension::Dimension> {
            self.type_info.get(propname).and_then(|v| v.1)
        }
    }
}
