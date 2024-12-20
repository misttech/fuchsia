// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::MonikerError;
use cm_types::{LongName, Name};
use core::cmp::Ordering;
use std::fmt;

/// A [ChildName] locally identifies a child component instance using the name assigned by
/// its parent and its collection (if present). It is the building block of [Moniker].
///
/// Display notation: "[collection:]name".
#[derive(Eq, PartialEq, Clone, Hash)]
pub struct ChildName {
    pub name: LongName,
    pub collection: Option<Name>,
}

impl ChildName {
    pub fn new(name: LongName, collection: Option<Name>) -> Self {
        Self { name, collection }
    }

    pub fn try_new<S>(name: S, collection: Option<S>) -> Result<Self, MonikerError>
    where
        S: AsRef<str> + Into<String>,
    {
        let name = LongName::new(name)?;
        let collection = match collection {
            Some(coll) => {
                let coll_name = Name::new(coll)?;
                Some(coll_name)
            }
            None => None,
        };
        Ok(Self { name, collection })
    }

    /// Parses a `ChildName` from a string.
    ///
    /// Input strings should be of the format `[collection:]name`, e.g. `foo` or `biz:foo`.
    pub fn parse<T: AsRef<str>>(rep: T) -> Result<Self, MonikerError> {
        let rep = rep.as_ref();
        let parts: Vec<&str> = rep.split(":").collect();
        let (coll, name) = match parts.len() {
            1 => (None, parts[0]),
            2 => (Some(parts[0]), parts[1]),
            _ => return Err(MonikerError::invalid_moniker(rep)),
        };
        ChildName::try_new(name, coll)
    }

    pub fn name(&self) -> &LongName {
        &self.name
    }

    pub fn collection(&self) -> Option<&Name> {
        self.collection.as_ref()
    }
}

impl TryFrom<&str> for ChildName {
    type Error = MonikerError;

    fn try_from(rep: &str) -> Result<Self, MonikerError> {
        ChildName::parse(rep)
    }
}

impl From<cm_rust::ChildRef> for ChildName {
    fn from(child_ref: cm_rust::ChildRef) -> Self {
        Self { name: child_ref.name, collection: child_ref.collection }
    }
}

impl From<ChildName> for cm_rust::ChildRef {
    fn from(child_name: ChildName) -> Self {
        Self { name: child_name.name, collection: child_name.collection }
    }
}

impl Ord for ChildName {
    fn cmp(&self, other: &Self) -> Ordering {
        (&self.collection, &self.name).cmp(&(&other.collection, &other.name))
    }
}

impl PartialOrd for ChildName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ChildName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(coll) = &self.collection {
            write!(f, "{}:{}", coll, self.name)
        } else {
            write!(f, "{}", self.name)
        }
    }
}

impl fmt::Debug for ChildName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cm_types::{MAX_LONG_NAME_LENGTH, MAX_NAME_LENGTH};

    #[test]
    fn child_monikers() {
        let m = ChildName::try_new("test", None).unwrap();
        assert_eq!("test", m.name().as_str());
        assert_eq!(None, m.collection());
        assert_eq!("test", format!("{}", m));
        assert_eq!(m, ChildName::try_from("test").unwrap());

        let m = ChildName::try_new("test", Some("coll")).unwrap();
        assert_eq!("test", m.name().as_str());
        assert_eq!(Some(&Name::new("coll").unwrap()), m.collection());
        assert_eq!("coll:test", format!("{}", m));
        assert_eq!(m, ChildName::try_from("coll:test").unwrap());

        let max_coll_length_part = "f".repeat(MAX_NAME_LENGTH);
        let max_name_length_part = "f".repeat(MAX_LONG_NAME_LENGTH);
        let max_moniker_length = format!("{}:{}", max_coll_length_part, max_name_length_part);
        let m = ChildName::parse(max_moniker_length).expect("valid moniker");
        assert_eq!(&max_name_length_part, m.name().as_str());
        assert_eq!(Some(&Name::new(max_coll_length_part).unwrap()), m.collection());

        assert!(ChildName::parse("").is_err(), "cannot be empty");
        assert!(ChildName::parse(":").is_err(), "cannot be empty with colon");
        assert!(ChildName::parse("f:").is_err(), "second part cannot be empty with colon");
        assert!(ChildName::parse(":f").is_err(), "first part cannot be empty with colon");
        assert!(ChildName::parse("f:f:f").is_err(), "multiple colons not allowed");
        assert!(ChildName::parse("@").is_err(), "invalid character in name");
        assert!(ChildName::parse("@:f").is_err(), "invalid character in collection");
        assert!(ChildName::parse("f:@").is_err(), "invalid character in name with collection");
        assert!(
            ChildName::parse(&format!("f:{}", "x".repeat(MAX_LONG_NAME_LENGTH + 1))).is_err(),
            "name too long"
        );
        assert!(
            ChildName::parse(&format!("{}:x", "f".repeat(MAX_NAME_LENGTH + 1))).is_err(),
            "collection too long"
        );
    }

    #[test]
    fn child_moniker_compare() {
        let a = ChildName::try_new("a", None).unwrap();
        let aa = ChildName::try_new("a", Some("a")).unwrap();
        let ab = ChildName::try_new("a", Some("b")).unwrap();
        let ba = ChildName::try_new("b", Some("a")).unwrap();
        let bb = ChildName::try_new("b", Some("b")).unwrap();
        let aa_same = ChildName::try_new("a", Some("a")).unwrap();

        assert_eq!(Ordering::Less, a.cmp(&aa));
        assert_eq!(Ordering::Greater, aa.cmp(&a));
        assert_eq!(Ordering::Less, a.cmp(&ab));
        assert_eq!(Ordering::Greater, ab.cmp(&a));
        assert_eq!(Ordering::Less, a.cmp(&ba));
        assert_eq!(Ordering::Greater, ba.cmp(&a));
        assert_eq!(Ordering::Less, a.cmp(&bb));
        assert_eq!(Ordering::Greater, bb.cmp(&a));

        assert_eq!(Ordering::Less, aa.cmp(&ab));
        assert_eq!(Ordering::Greater, ab.cmp(&aa));
        assert_eq!(Ordering::Less, aa.cmp(&ba));
        assert_eq!(Ordering::Greater, ba.cmp(&aa));
        assert_eq!(Ordering::Less, aa.cmp(&bb));
        assert_eq!(Ordering::Greater, bb.cmp(&aa));
        assert_eq!(Ordering::Equal, aa.cmp(&aa_same));
        assert_eq!(Ordering::Equal, aa_same.cmp(&aa));

        assert_eq!(Ordering::Greater, ab.cmp(&ba));
        assert_eq!(Ordering::Less, ba.cmp(&ab));
        assert_eq!(Ordering::Less, ab.cmp(&bb));
        assert_eq!(Ordering::Greater, bb.cmp(&ab));

        assert_eq!(Ordering::Less, ba.cmp(&bb));
        assert_eq!(Ordering::Greater, bb.cmp(&ba));
    }
}
