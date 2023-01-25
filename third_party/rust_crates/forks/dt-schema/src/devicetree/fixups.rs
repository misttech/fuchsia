// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

mod addresses;
mod gpios;
mod interrupts;
mod phandle;
mod utils;

use crate::path::JsonPath;

pub use self::utils::PhandleError;

use super::types::PropertyTypeLookup;

#[derive(thiserror::Error, Debug)]
pub enum DevicetreeFixupError {
    #[error("Could not fixup property '{0}' as it was in an unexpected format: {1}")]
    UnexpectedPropertyFormat(JsonPath, serde_json::Value),

    #[error("Invalid phandle array")]
    InvalidPhandle(#[from] PhandleError),

    #[error("Missing property {0}")]
    MissingProperty(JsonPath),

    #[error("JSON error")]
    JsonError(#[from] serde_json::Error),
}

/// Trait that is used by fixups to lookup information about the devicetree.
/// A |DevicetreeLookup| has knowledge of the node it is associated with.
pub trait DevicetreeLookup {
    /// Gets value of the given |cell_prop| at |phandle|.
    fn get_cells_size(
        &self,
        phandle_path: JsonPath,
        phandle: u32,
        cell_prop: &str,
    ) -> Result<u32, PhandleError>;

    /// Gets value of |prop| at this node's parent or an ancestor.
    fn get_prop_from_parents(&self, prop: &str) -> Result<Option<u32>, PhandleError>;

    fn get_cells_prop_for_prop(&self, prop: &str) -> String {
        match prop {
            "assigned-clocks" => "#clock-cells",
            "assigned-clock-parents" => "#clock-cells",
            "cooling-device" => "#cooling-cells",
            "interrupts-extended" => "#interrupt-cells",
            "interconnects" => "#interconnect-cells",
            "mboxes" => "#mbox-cells",
            "sound-dai" => "#sound-dai-cells",
            "msi-parent" => "#msi-cells",
            "msi-ranges" => "#interrupt-cells",
            prop => {
                if prop.ends_with('s') && !prop.contains("gpio") {
                    let name = "#".to_owned() + &prop[0..prop.len() - 1] + "-cells";
                    return name;
                } else {
                    prop
                }
            }
        }
        .to_owned()
    }
}

pub trait DevicetreeFixup: Sized {
    /// Make a new instance of this |fixup|. Returns None if the fixup is not applicable to this property.
    /// Note that |node| is a full device tree node.
    fn new(
        nodename: &str,
        node: &serde_json::Map<String, serde_json::Value>,
        path: JsonPath,
        type_lookup: &dyn PropertyTypeLookup,
    ) -> Result<Option<Self>, DevicetreeFixupError>;

    /// Run the fixup and give the fixed node back.
    fn fixup(
        self,
        lookup: &dyn DevicetreeLookup,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DevicetreeFixupError>;
}

/// Helper function that does a fixup (if it is applicable, i.e. T::new() returns Ok(Some(...))), or just returns the given |value|.
#[tracing::instrument(level = "debug", skip_all, fields(fixup=std::any::type_name::<T>(), path=%path))]
fn do_fixup<T: DevicetreeFixup>(
    nodename: &str,
    value: serde_json::Map<String, serde_json::Value>,
    path: &JsonPath,
    type_lookup: &dyn PropertyTypeLookup,
    lookup: &dyn DevicetreeLookup,
) -> Result<serde_json::Map<String, serde_json::Value>, DevicetreeFixupError> {
    T::new(nodename, &value, path.clone(), type_lookup)?
        .map(|v| v.fixup(lookup))
        .unwrap_or(Ok(value))
}

pub fn fixup_node(
    nodename: &str,
    value: serde_json::Map<String, serde_json::Value>,
    path: &JsonPath,
    type_lookup: &dyn PropertyTypeLookup,
    lookup: &dyn DevicetreeLookup,
) -> Result<serde_json::Map<String, serde_json::Value>, DevicetreeFixupError> {
    let value = do_fixup::<gpios::GpioFixup>(nodename, value, path, type_lookup, lookup)?;
    let value = do_fixup::<addresses::AddressFixup>(nodename, value, path, type_lookup, lookup)?;
    let value = do_fixup::<interrupts::InterruptFixup>(nodename, value, path, type_lookup, lookup)?;
    let value = do_fixup::<phandle::PhandleFixup>(nodename, value, path, type_lookup, lookup)?;
    Ok(value)
}
