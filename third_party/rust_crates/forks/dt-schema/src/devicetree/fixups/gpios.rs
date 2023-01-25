// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::collections::VecDeque;

use crate::{devicetree::types::PropertyTypeLookup, path::JsonPath};

use super::{
    utils::{PhandleError, PhandleIterator},
    DevicetreeFixup, DevicetreeFixupError,
};

/// |GpioFixup| converts any GPIO properties to matrices.
pub struct GpioFixup {
    node: serde_json::Map<String, serde_json::Value>,
    path: JsonPath,
}

fn key_is_gpio(key: &String) -> bool {
    (key.ends_with("-gpio") || key.ends_with("-gpios") || key == "gpio" || key == "gpios")
        && !key.ends_with(",nr-gpios")
}

impl DevicetreeFixup for GpioFixup {
    fn new(
        _nodename: &str,
        node: &serde_json::Map<String, serde_json::Value>,
        path: JsonPath,
        _type_lookup: &dyn PropertyTypeLookup,
    ) -> Result<Option<Self>, super::DevicetreeFixupError> {
        // This fixup applies to nodes with any -gpio properties, but not a gpio-hog one.
        let applies = node.keys().any(key_is_gpio) && !node.keys().any(|key| key == "gpio-hog");
        if !applies {
            Ok(None)
        } else {
            Ok(Some(GpioFixup {
                node: node.clone(),
                path,
            }))
        }
    }

    fn fixup(
        mut self,
        lookup: &dyn super::DevicetreeLookup,
    ) -> Result<serde_json::Map<String, serde_json::Value>, super::DevicetreeFixupError> {
        for (key, value) in self.node.iter_mut().filter(|(key, _)| key_is_gpio(key)) {
            let array = match value
                .as_array()
                .filter(|v| v.len() == 1)
                .and_then(|v| v[0].as_array())
            {
                Some(array) => array,
                None => {
                    return Err(DevicetreeFixupError::UnexpectedPropertyFormat(
                        self.path.extend(key),
                        value.clone(),
                    ))
                }
            };

            let collected: Result<Vec<VecDeque<u64>>, PhandleError> =
                PhandleIterator::new(array, lookup, self.path.extend(key), "#gpio-cells").collect();
            *value = serde_json::to_value(collected?)?;
        }

        Ok(self.node)
    }
}

#[cfg(test)]
mod tests {

    use serde_json::json;

    use crate::devicetree::fixups::utils::for_tests::FakeLookup;

    use super::*;

    #[test]
    fn test_gpio_translation() {
        let fake_node = json!({
            "test-gpios": [[0xface, 0x1, 0x2, 0xbeef, 0x3, 0x4, 0x5]],
        });
        let lookup = FakeLookup::new()
            .with_phandle(0xface, "#gpio-cells", 2)
            .with_phandle(0xbeef, "#gpio-cells", 3);

        let fixup = GpioFixup::new("", fake_node.as_object().unwrap(), JsonPath::new(), &lookup)
            .expect("valid dt object")
            .expect("schema applies")
            .fixup(&lookup)
            .expect("fixup ok");

        assert_eq!(
            Into::<serde_json::Value>::into(fixup),
            json!({
                "test-gpios": [[0xface, 0x1, 0x2], [0xbeef, 0x3, 0x4, 0x5]],
            })
        );
    }
}
