// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::{collections::VecDeque, fmt::Display};

#[derive(Clone, Debug, PartialOrd, Ord, PartialEq, Eq)]
/// Represents a path through a json object.
pub struct JsonPath {
    elements: VecDeque<String>,
}

impl JsonPath {
    pub fn new() -> JsonPath {
        JsonPath {
            elements: VecDeque::new(),
        }
    }

    pub fn extend(&self, elem: &str) -> Self {
        let mut new = self.clone();
        new.elements.push_back(elem.to_owned());
        new
    }

    pub fn extend_index_only(&self, index: usize) -> Self {
        let mut new = self.clone();
        let back = new
            .elements
            .pop_back()
            .unwrap_or_else(|| "/<root>".to_owned());
        new.elements.push_back(format!("{}[{}]", back, index));
        new
    }

    pub fn extend_array_index(&self, elem: &str, index: usize) -> Self {
        let mut new = self.clone();
        new.elements.push_back(format!("{}[{}]", elem, index));
        new
    }

    pub fn back(&self) -> &str {
        self.elements.back().map(|s| s.as_str()).unwrap_or("/")
    }
}

impl Display for JsonPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.elements.is_empty() {
            write!(f, "/")?;
        }
        for el in self.elements.iter() {
            write!(f, "/{}", el)?;
        }

        Ok(())
    }
}
