// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::errors::ParseError;

const MAX_HOST_BYTES: usize = 253;
const MAX_HOST_LABEL_BYTES: usize = 63;

// The host of a fuchsia-pkg:// URL.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Host(String);

impl Host {
    /// Returns an error if the provided hostname does not comply with the package URL spec:
    /// https://fuchsia.dev/fuchsia-src/concepts/packages/package_url#repository
    ///
    /// Hostnames have a maximum total length of 253 bytes and consist of dot (`.`)-delimited LDH
    /// labels, each of 1 to 63 characters from `0`..=`9`, `a`..=`z`, and `-`, neither starting nor
    /// ending with `-`.
    pub fn parse(host: String) -> Result<Self, ParseError> {
        if host.is_empty() {
            return Err(ParseError::EmptyHost);
        }

        if host.len() > MAX_HOST_BYTES {
            return Err(ParseError::InvalidHost);
        }

        for label in host.split('.') {
            if label.is_empty()
                || label.len() > MAX_HOST_LABEL_BYTES
                || label.starts_with('-')
                || label.ends_with('-')
                || !label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(ParseError::InvalidHost);
            }
        }
        Ok(Self(host))
    }
}

impl AsRef<str> for Host {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<Host> for String {
    fn from(host: Host) -> Self {
        host.0
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_err() {
        let max_label = "a".repeat(MAX_HOST_LABEL_BYTES);
        let too_long_label = "a".repeat(MAX_HOST_LABEL_BYTES + 1);
        let too_long_host = format!("{max_label}.{max_label}.{max_label}.{}", "d".repeat(62));
        assert_eq!(too_long_host.len(), MAX_HOST_BYTES + 1);

        for host in [
            "FuChSiA.CoM",
            "FUCHSIA_1.com",
            "FUCHSIA-1.COM",
            "fuchsia-①.com",
            "RISCV.fuchsia.com",
            "RV64.fuchsia.com",
            "fu_chsia.com",
            "fu:chsia.com",
            "fu#chsia.com",
            "fu?chsia.com",
            "fu/chsia.com",
            ".",
            "..",
            ".example.org",
            "example.org.",
            "ex..ample.org",
            "-",
            "-example.org",
            "example-.org",
            "example.-org",
            "example.org-",
            "xn--",
            too_long_label.as_str(),
            too_long_host.as_str(),
        ] {
            assert_eq!(
                Host::parse(host.to_string()),
                Err(ParseError::InvalidHost),
                "the host string {:?}",
                host
            );
        }

        assert_eq!(Host::parse("".into()), Err(ParseError::EmptyHost));
    }

    #[test]
    fn parse_ok() {
        let max_label = "a".repeat(MAX_HOST_LABEL_BYTES);
        let max_host = format!("{max_label}.{max_label}.{max_label}.{}", "d".repeat(61));
        assert_eq!(max_host.len(), MAX_HOST_BYTES);

        for host in [
            "example.org",
            "ex.am.ple.org",
            "example0.org",
            "ex-ample.org",
            "a",
            "1",
            "a--b",
            "127.1",
            "127.0.0.1",
            "256.0.0.1",
            "xn--bcher-kva",
            "xn--99999",
            max_label.as_str(),
            max_host.as_str(),
        ] {
            assert_eq!(
                Host::parse(host.to_string()).unwrap().as_ref(),
                host,
                "the host string {:?}",
                host
            );
        }
    }
}
