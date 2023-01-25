// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.


use std::collections::BTreeSet;

use crate::validator::{dimension::Dimension, property_type::PropertyType};

pub trait PropertyTypeLookup {
    fn get_property_type(&self, propname: &str) -> BTreeSet<PropertyType>;

    fn get_property_dimensions(&self, propname: &str) -> Option<Dimension>;
}
