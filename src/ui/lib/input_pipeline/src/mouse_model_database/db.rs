// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{data, data_import_from_chromiumos};
use sorted_vec_map::SortedVecMap;
use std::sync::LazyLock;

// TODO: b/507052772 - Use a database or a resource file to store mouse data,
// to avoid loading all data to RAM.

/// Mouse have a sensor that tells how far they moved in "counts", depends
/// on sensor, mouse will report different CPI (counts per inch). Currently,
/// "standard" mouse is 1000 CPI, and it can up to 8000 CPI. We need this
/// database to understand how far the mouse moved.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MouseModel {
    pub(crate) identifier: &'static str,
    pub(crate) vendor_product_id: &'static str,
    pub(crate) counts_per_mm: u32,
}

const MM_PER_INCH: f32 = 25.4;
pub(crate) const DEFAULT_COUNTS_PER_MM: u32 = (1000.0 / MM_PER_INCH) as u32;

static DB: LazyLock<SortedVecMap<String, MouseModel>> = LazyLock::new(|| {
    let mut db = SortedVecMap::new();

    for (key, (cpi, identifier)) in data::MODELS.iter() {
        db.insert(
            key.to_string(),
            MouseModel {
                identifier,
                vendor_product_id: *key,
                counts_per_mm: ((*cpi as f32) / MM_PER_INCH) as u32,
            },
        );
    }

    for (key, (cpi, identifier)) in data_import_from_chromiumos::MODELS.iter() {
        db.insert(
            key.to_string(),
            MouseModel {
                identifier,
                vendor_product_id: *key,
                counts_per_mm: ((*cpi as f32) / MM_PER_INCH) as u32,
            },
        );
    }

    db
});

/// "Standard" mouse CPI and polling rate.
const DEFAULT_MODEL: MouseModel = MouseModel {
    identifier: "default mouse",
    vendor_product_id: "*:*",
    counts_per_mm: DEFAULT_COUNTS_PER_MM,
};

pub(crate) fn get_mouse_model(
    device_info: Option<fidl_next_fuchsia_input_report::DeviceInformation>,
) -> MouseModel {
    match device_info {
        None => DEFAULT_MODEL.clone(),
        Some(device_info) => {
            let vid = to_hex(device_info.vendor_id.unwrap_or_default());
            let pid = to_hex(device_info.product_id.unwrap_or_default());
            let key = format!("{}:{}", vid, pid);

            // 1. Exact match
            if let Some(m) = DB.get(&key) {
                return m.clone();
            }

            // 2. Pattern match
            for (k, m) in DB.iter() {
                if k.contains('*') {
                    let pattern = glob::Pattern::new(k).expect("invalid glob pattern in DB");
                    if pattern.matches(&key) {
                        return m.clone();
                    }
                }
            }

            // 3. Default match
            let default_key = format!("{}:*", vid);
            if let Some(m) = DB.get(&default_key) {
                return m.clone();
            }

            DEFAULT_MODEL.clone()
        }
    }
}

/// usb vendor_id and product_id are 16 bit int.
fn to_hex(id: u32) -> String {
    format!("{:04x}", id)
}

#[cfg(test)]
mod test {
    use super::super::{data, data_import_from_chromiumos};
    use super::*;
    use regex::Regex;
    use std::collections::HashSet;
    use test_case::test_case;

    fn new_mouse_model(
        vendor_product_id: &'static str,
        counts_per_inch: u32,
        identifier: &'static str,
    ) -> MouseModel {
        MouseModel {
            identifier,
            vendor_product_id,
            counts_per_mm: ((counts_per_inch as f32) / MM_PER_INCH) as u32,
        }
    }

    #[test_case("*:*", 1000, "default mouse" =>
      MouseModel {
        vendor_product_id: "*:*",
        identifier: "default mouse",
        counts_per_mm: DEFAULT_COUNTS_PER_MM,
      }; "default mouse")]
    #[test_case("0001:*", 1000, "any mouse of vendor" =>
      MouseModel {
        vendor_product_id: "0001:*",
        identifier: "any mouse of vendor",
        counts_per_mm: DEFAULT_COUNTS_PER_MM,
      }; "any mouse of vendor")]
    #[test_case("0001:001*", 1000, "pattern product_id" =>
      MouseModel {
        vendor_product_id: "0001:001*",
        identifier: "pattern product_id",
        counts_per_mm: DEFAULT_COUNTS_PER_MM,
      }; "pattern product_id")]
    #[test_case("0001:0002", 1000, "exact model" =>
      MouseModel {
        vendor_product_id: "0001:0002",
        identifier: "exact model",
        counts_per_mm: DEFAULT_COUNTS_PER_MM,
      }; "exact model")]
    #[fuchsia::test]
    fn test_mouse_model_new(
        vendor_product_id: &'static str,
        cpi: u32,
        identifier: &'static str,
    ) -> MouseModel {
        new_mouse_model(vendor_product_id, cpi, identifier)
    }

    #[test_case(0x046d, 0xc24c =>
      new_mouse_model("046d:c24c", 4000, "Logitech G400s")
      ; "Known mouse")]
    #[test_case(0x046d, 0xc401 =>
      new_mouse_model("046d:c40*", 600, "Logitech Trackballs*")
      ; "pattern match")]
    #[test_case(0x05ac, 0x0000 =>
      new_mouse_model("05ac:*", 373, "Apple mice (other)")
      ; "any match")]
    #[test_case(0x046d, 0x0aaf =>
      new_mouse_model("*:*", 1000, "default mouse")
      ; "Unknown device: this is a microphone")]
    #[fuchsia::test]
    fn test_get_mouse_model(vendor_id: u32, product_id: u32) -> MouseModel {
        get_mouse_model(Some(fidl_next_fuchsia_input_report::DeviceInformation {
            vendor_id: Some(vendor_id),
            product_id: Some(product_id),
            version: Some(0),
            polling_rate: Some(0),
            ..Default::default()
        }))
    }

    #[fuchsia::test]
    fn test_get_mouse_model_none() {
        pretty_assertions::assert_eq!(get_mouse_model(None), DEFAULT_MODEL);
    }

    #[fuchsia::test]
    fn no_duplicated_mouse_model() {
        let mut models: HashSet<(String, String)> = HashSet::new();

        for (key, _) in data_import_from_chromiumos::MODELS.iter() {
            let parts: Vec<&str> = key.split(':').collect();
            let new_inserted = models.insert((parts[0].to_string(), parts[1].to_string()));
            if !new_inserted {
                panic!(
                    "found duplicated mouse model in data_import_from_chromiumos: vendor: {}, product: {}",
                    parts[0], parts[1]
                );
            }
        }

        for (key, _) in data::MODELS.iter() {
            let parts: Vec<&str> = key.split(':').collect();
            let new_inserted = models.insert((parts[0].to_string(), parts[1].to_string()));
            if !new_inserted {
                panic!(
                    "found duplicated mouse model in data: vendor: {}, product: {}",
                    parts[0], parts[1]
                );
            }
        }
    }

    #[fuchsia::test]
    fn validate_vendor_id_product_id_chromiumos() {
        let vendor_id_re = Regex::new(r"^[0-9a-f]{4}$").unwrap();
        let product_id_re = Regex::new(r"^[0-9a-f]{3}[0-9a-f\*]$").unwrap();
        for (key, _) in data_import_from_chromiumos::MODELS.iter() {
            let parts: Vec<&str> = key.split(':').collect();
            let vid = parts[0];
            let pid = parts[1];
            assert!(vendor_id_re.is_match(vid), "vendor id should be 4 low case hex digit");
            if pid != "*" {
                assert!(
                    product_id_re.is_match(pid),
                    r#"product id should be "* only" or "3 low case hex digit with ending *" or "4 low case hex digit""#
                );
            }
        }
    }

    #[fuchsia::test]
    fn validate_vendor_id_product_id_data() {
        let vendor_id_re = Regex::new(r"^[0-9a-f]{4}$").unwrap();
        let product_id_re = Regex::new(r"^[0-9a-f]{3}[0-9a-f\*]$").unwrap();
        for (key, _) in data::MODELS.iter() {
            let parts: Vec<&str> = key.split(':').collect();
            let vid = parts[0];
            let pid = parts[1];
            assert!(vendor_id_re.is_match(vid), "vendor id should be 4 low case hex digit");
            if pid != "*" {
                assert!(
                    product_id_re.is_match(pid),
                    r#"product id should be "* only" or "3 low case hex digit with ending *" or "4 low case hex digit""#
                );
            }
        }
    }
}
