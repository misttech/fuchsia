// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use fuchsia_hash::{HASH_SIZE, Hash};

pub mod boot;
pub mod builtin_url;
pub mod errors;
mod fuchsia_pkg_absolute_component_url;
mod fuchsia_pkg_absolute_package_url;
mod fuchsia_pkg_component_url;
mod fuchsia_pkg_package_url;
mod fuchsia_pkg_pinned_absolute_package_url;
mod fuchsia_pkg_unpinned_absolute_package_url;
pub(crate) mod generic;
mod host;
mod parse;
mod path;
mod relative_component_url;
mod relative_package_url;
mod repository_url;
mod resource;
pub mod test;

pub use crate::errors::ParseError;
pub use crate::fuchsia_pkg_absolute_component_url::FuchsiaPkgAbsoluteComponentUrl;
pub use crate::fuchsia_pkg_absolute_package_url::FuchsiaPkgAbsolutePackageUrl;
pub use crate::fuchsia_pkg_component_url::FuchsiaPkgComponentUrl;
pub use crate::fuchsia_pkg_package_url::FuchsiaPkgPackageUrl;
pub use crate::fuchsia_pkg_pinned_absolute_package_url::FuchsiaPkgPinnedAbsolutePackageUrl;
pub use crate::fuchsia_pkg_unpinned_absolute_package_url::FuchsiaPkgUnpinnedAbsolutePackageUrl;
pub mod fuchsia_pkg {
    pub use crate::{
        FuchsiaPkgAbsoluteComponentUrl as AbsoluteComponentUrl,
        FuchsiaPkgAbsolutePackageUrl as AbsolutePackageUrl, FuchsiaPkgComponentUrl as ComponentUrl,
        FuchsiaPkgPackageUrl as PackageUrl,
        FuchsiaPkgPinnedAbsolutePackageUrl as PinnedAbsolutePackageUrl,
        FuchsiaPkgUnpinnedAbsolutePackageUrl as UnpinnedAbsolutePackageUrl,
    };
}
pub use crate::generic::{NoneHash, NoneHost};
pub use crate::parse::{MAX_PACKAGE_PATH_SEGMENT_BYTES, PackageName, PackageVariant};
pub use crate::path::Path;
pub(crate) use crate::path::parse_path_to_name_and_variant;
pub use crate::relative_component_url::RelativeComponentUrl;
pub use crate::relative_package_url::RelativePackageUrl;
pub use crate::repository_url::RepositoryUrl;
pub use crate::resource::{Resource, ResourcePathError};

use crate::host::Host;
use percent_encoding::{AsciiSet, CONTROLS};
use std::sync::LazyLock;

/// The [WHATWG fragment percent-encode set](<https://url.spec.whatwg.org/#fragment-percent-encode-set>)
/// plus `%` and `#` so that percent-decoded [`Resource`] paths round-trip losslessly.
const FRAGMENT: &AsciiSet =
    &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`').add(b'%').add(b'#');

/// The printable ASCII characters that URL parsing requires to be percent-encoded inside
/// `#<fragment>`, where a `%` only counts as encoded when it begins a valid `%HH` escape.
///
/// Together with control characters (which are also rejected when unencoded), these are exactly
/// the characters of [`FRAGMENT`]. Non-ASCII characters, which percent-encoding always encodes,
/// must also be percent-encoded.
const FRAGMENT_MUST_PERCENT_ENCODE: &[u8] = b" \"#%<>`";

const RELATIVE_SCHEME: &str = "relative";

/// A default base URL from which to parse relative component URL
/// components.
static RELATIVE_BASE: LazyLock<url::Url> =
    LazyLock::new(|| url::Url::parse(&format!("{RELATIVE_SCHEME}:///")).unwrap());

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scheme {
    Builtin,
    FuchsiaPkg,
    FuchsiaBoot,
}

impl std::fmt::Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Builtin => builtin_url::SCHEME,
            Self::FuchsiaPkg => repository_url::SCHEME,
            Self::FuchsiaBoot => boot::SCHEME_STR,
        }
        .fmt(f)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct UrlParts {
    scheme: Option<Scheme>,
    host: Option<Host>,
    path: Option<Path>,
    hash: Option<Hash>,
    resource: Option<Resource>,
}

impl UrlParts {
    fn parse(input: &str) -> Result<Self, ParseError> {
        // Validate the raw `#<fragment>` of `input` rather than the `url` crate's view of it,
        // since the `url` crate strips trailing C0 control and space characters and ASCII tab and
        // newline characters from it, and percent-encodes characters that must already be
        // percent-encoded.
        let fragment = input.split_once('#').map(|(_, fragment)| fragment);
        match url::Url::parse(input) {
            Ok(url) => {
                let scheme = match url.scheme() {
                    builtin_url::SCHEME => Scheme::Builtin,
                    repository_url::SCHEME => Scheme::FuchsiaPkg,
                    boot::SCHEME_STR => Scheme::FuchsiaBoot,
                    _ => return Err(ParseError::InvalidScheme),
                };
                Self::from_scheme_and_url(Some(scheme), &url, fragment)
            }
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                Self::from_scheme_and_url(None, &RELATIVE_BASE.join(input)?, fragment)
            }
            Err(e) => Err(e)?,
        }
    }
    /// `fragment` is the raw (not yet percent-decoded) `#<fragment>` of the URL that `url` was
    /// parsed from.
    fn from_scheme_and_url(
        scheme: Option<Scheme>,
        url: &url::Url,
        fragment: Option<&str>,
    ) -> Result<Self, ParseError> {
        if url.port().is_some() {
            return Err(ParseError::CannotContainPort);
        }

        if !url.username().is_empty() {
            return Err(ParseError::CannotContainUsername);
        }

        if url.password().is_some() {
            return Err(ParseError::CannotContainPassword);
        }

        let host = url
            .host_str()
            .filter(|s| !s.is_empty())
            .map(|s| Host::parse(s.to_string()))
            .transpose()?;

        // When parsing URLs, there are three kinds of scheme that affect the parsed host and path:
        //   * File: file
        //   * SpecialNotFile: http, https, ws, wss, ftp
        //   * NotSpecial: <anything else>
        // https://cs.opensource.google/fuchsia/fuchsia/+/main:third_party/rust_crates/vendor/url-2.3.1/src/parser.rs;l=152-168;drc=5d413711388939e4a532a0ab8bfb331e6044ee02
        //
        // | input         | host | path   | cannot-be-a-base | stringified   |
        // |---------------+------+--------+------------------+---------------|
        // | file:text     |      | /text  | false            | file:///text  |
        // | file:/text    |      | /text  | false            | file:///text  |
        // | file://text   | text | /      | false            | file://text/  |
        // | file://text/  | text | /      | false            | file://text/  |
        // | file://text// | text | /      | false            | file://text/  |
        // | file:///text  |      | /text  | false            | file:///text  |
        // | file:///text/ |      | /text/ | false            | file:///text/ |
        // |               |      |        |                  |               |
        // | http:text     | text | /      | false            | http://text/  |
        // | http:/text    | text | /      | false            | http://text/  |
        // | http://text   | text | /      | false            | http://text/  |
        // | http://text/  | text | /      | false            | http://text/  |
        // | http://text// | text | //     | false            | http://text// |
        // | http:///text  | text | /      | false            | http://text/  |
        // | http:///text/ | text | /      | false            | http://text/  |
        // |               |      |        |                  |               |
        // | else:text     |      | text   | true             | else:text     |
        // | else:/text    |      | /text  | false            | else:/text    |
        // | else://text   | text |        | false            | else://text   |
        // | else://text/  | text | /      | false            | else://text/  |
        // | else://text// | text | //     | false            | else://text// |
        // | else:///text  |      | /text  | false            | else:///text  |
        // | else:///text/ |      | /text/ | false            | else:///text/ |
        //
        // Fuchsia uses URLs to encode:
        //   * scheme: which resolver should CM use
        //   * host: which package store should the resolver use
        //   * path: which package from the package store
        //   * hash: exactly specify the package instead of using the store's path -> hash mapping
        //   * resource: which file in the package
        //
        // Because the path is used to identify a package (some named thing in a collection) and
        // we have control over how the things are allowed to be named, we simplify the situation
        // by ignoring the leading slash (i.e. if the path is just a slash we treat the path as
        // absent and if the path starts with a slash we trim the slash) and requiring that packages
        // do not start with a slash.
        let path = if url.path().is_empty() || url.path() == "/" {
            None
        } else {
            Some(url.path().strip_prefix("/").unwrap_or_else(|| url.path()).parse()?)
        };

        let hash = parse_query_pairs(url.query_pairs())?;

        let resource = fragment.map(parse_fragment).transpose()?;

        Ok(Self { scheme, host, path, hash, resource })
    }
}

/// Returns the first character of the raw (not yet percent-decoded) `fragment` that must be
/// percent-encoded but is not: any control or other non-ASCII character or
/// [`FRAGMENT_MUST_PERCENT_ENCODE`] character, except for a `%` that begins a valid `%HH` escape.
///
/// Scanning bytes finds the same character as scanning characters: the scan stops at or before the
/// first non-ASCII byte, which begins a multi-byte UTF-8 sequence because every earlier byte is
/// ASCII.
fn find_unencoded_fragment_char(fragment: &str) -> Option<char> {
    let bytes = fragment.as_bytes();
    let index = bytes.iter().enumerate().position(|(index, &byte)| {
        let begins_escape = byte == b'%'
            && bytes
                .get(index + 1..index + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
        !byte.is_ascii()
            || byte.is_ascii_control()
            || (FRAGMENT_MUST_PERCENT_ENCODE.contains(&byte) && !begins_escape)
    })?;
    fragment[index..].chars().next()
}

/// Validates the raw (not yet percent-decoded) `#<fragment>` of a URL and percent-decodes it into
/// a [`Resource`].
///
/// Rejects the first character in `fragment` that must be percent-encoded: a C0
/// (`\u{0000}`..=`\u{001F}`) or DEL/C1 (`\u{007F}`..=`\u{009F}`) control character, another
/// non-ASCII character, or a `FRAGMENT_MUST_PERCENT_ENCODE` character (`' '`, `'"'`, `'#'`,
/// `'<'`, `'>'`, `` '`' ``, or a `'%'` that does not begin a valid `%HH` escape). This also
/// rejects ASCII whitespace anywhere in `fragment`, since `' '` must be percent-encoded and all
/// other ASCII whitespace characters are control characters. An unencoded NUL or newline is
/// rejected like its percent-encoded form, since encoding it would not make the resource path
/// valid. An empty `fragment` (a trailing `#`) is rejected as an empty resource path.
fn parse_fragment(fragment: &str) -> Result<Resource, ParseError> {
    if let Some(character) = find_unencoded_fragment_char(fragment) {
        return Err(ParseError::InvalidResourcePath(match character {
            '\0' => ResourcePathError::NameContainsNull,
            '\n' => ResourcePathError::NameContainsNewline,
            character => ResourcePathError::UrlCannotContainUnencodedCharacter { character },
        }));
    }
    let resource = percent_encoding::percent_decode(fragment.as_bytes())
        .decode_utf8()
        .map_err(ParseError::ResourcePathPercentDecode)?;
    Resource::try_from(resource.into_owned()).map_err(ParseError::InvalidResourcePath)
}

/// After all other checks, ensure the input string does not change when joined
/// with the `RELATIVE_BASE` URL, and then removing the base (inverse-join()).
fn validate_inverse_relative_url(input: &str) -> Result<(), ParseError> {
    let relative_url = RELATIVE_BASE.join(input)?;
    let unbased = RELATIVE_BASE.make_relative(&relative_url);
    if Some(input) == unbased.as_deref() {
        Ok(())
    } else {
        Err(ParseError::InvalidRelativePath(input.to_string(), unbased))?
    }
}

fn parse_query_pairs(pairs: url::form_urlencoded::Parse<'_>) -> Result<Option<Hash>, ParseError> {
    let mut query_hash = None;
    for (key, value) in pairs {
        if key == "hash" {
            if query_hash.is_some() {
                return Err(ParseError::MultipleHashes);
            }
            query_hash = Some(value.parse().map_err(ParseError::InvalidHash)?);
            // fuchsia-pkg URLs require lowercase hex characters, but fuchsia_hash::Hash::parse
            // accepts uppercase A-F.
            if !value.bytes().all(|b| (b >= b'0' && b <= b'9') || (b >= b'a' && b <= b'f')) {
                return Err(ParseError::UpperCaseHash);
            }
        } else {
            return Err(ParseError::ExtraQueryParameters);
        }
    }
    Ok(query_hash)
}

/// A URL locating a Fuchsia component. Can be either absolute or relative.
/// See [`AbsoluteComponentUrl`] and [`RelativeComponentUrl`] for more details.
pub type ComponentUrl =
    generic::ComponentUrl<Scheme, generic::OptionHost, Option<Path>, Option<Hash>>;

/// A URL locating a Fuchsia component.
/// Has the form "<scheme>://[host][/<path>][?hash=<hash>]#<resource>" where:
///   * "scheme" is a [`Scheme`]
///   * "host" is an optional [`Host`]
///   * "path" is an optional [`Path`]
///   * "hash" is an optional package [`Hash`]
///   * "resource" is a [`Resource`]
pub type AbsoluteComponentUrl =
    generic::AbsoluteComponentUrl<Scheme, generic::OptionHost, Option<Path>, Option<Hash>>;

/// A URL locating a Fuchsia package. Can be either absolute or relative.
/// See [`AbsolutePackageUrl`] and [`RelativePackageUrl`] for more details.
pub type PackageUrl = generic::PackageUrl<Scheme, generic::OptionHost, Option<Path>, Option<Hash>>;

/// A URL locating a Fuchsia package.
/// Has the form "<scheme>://[host][/<path>][?hash=<hash>]" where:
///   * "scheme" is a [`Scheme`](crate::Scheme)
///   * "host" is an optional [`Host`](crate::Host)
///   * "path" is an optional [`Path`](crate::Path)
///   * "hash" is an optional package [`Hash`](fuchsia_hash::Hash)
pub type AbsolutePackageUrl =
    generic::AbsolutePackageUrl<Scheme, generic::OptionHost, Option<Path>, Option<Hash>>;

// Supertrait for sealing traits in this crate.
trait Sealer {}

#[cfg(test)]
mod test_validate_inverse_relative_url {
    use super::*;
    use assert_matches::assert_matches;

    macro_rules! test_err {
        (
            $(
                $test_name:ident => {
                    path = $path:expr,
                    some_unbased = $some_unbased:expr,
                }
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    let err = ParseError::InvalidRelativePath(
                        $path.to_string(),
                        $some_unbased.map(|s: &str| s.to_string()),
                    );
                    assert_matches!(
                        validate_inverse_relative_url($path),
                        Err(e) if e == err,
                        "the url {:?}; expected = {:?}",
                        $path, err
                    );
                }
            )+
        }
    }

    test_err! {
        err_slash_prefix => {
            path = "/name",
            some_unbased = Some("name"),
        }
        err_three_slashes_prefix => {
            path = "///name",
            some_unbased = Some("name"),
        }
        err_slash_prefix_with_resource => {
            path = "/name#resource",
            some_unbased = Some("name#resource"),
        }
        err_three_slashes_prefix_and_resource => {
            path = "///name#resource",
            some_unbased = Some("name#resource"),
        }
        err_masks_host_must_be_empty_err => {
            path = "//example.org/name",
            some_unbased = None,
        }
        err_dot_masks_missing_name_err => {
            path = ".",
            some_unbased = Some(""),
        }
        err_dot_dot_masks_missing_name_err => {
            path = "..",
            some_unbased = Some(""),
        }
    }

    #[test]
    fn success() {
        for path in ["name", "other3-name", "name#resource", "name#reso%09urce"] {
            let () = validate_inverse_relative_url(path).unwrap();
        }
    }
}

#[cfg(test)]
mod test_url_parts {
    use super::*;
    use assert_matches::assert_matches;

    macro_rules! test_parse_err {
        (
            $(
                $test_name:ident => {
                    url = $url:expr,
                    err = $err:pat,
                }
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    assert_matches!(
                        UrlParts::parse($url),
                        Err($err)
                    );
                }
            )+
        }
    }

    test_parse_err! {
        err_invalid_scheme => {
            url = "bad-scheme://example.org",
            err = ParseError::InvalidScheme,
        }
        err_port => {
            url = "fuchsia-pkg://example.org:1",
            err = ParseError::CannotContainPort,
        }
        err_username => {
            url = "fuchsia-pkg://user@example.org",
            err = ParseError::CannotContainUsername,
        }
        err_password => {
            url = "fuchsia-pkg://:password@example.org",
            err = ParseError::CannotContainPassword,
        }
        err_invalid_host => {
            url = "fuchsia-pkg://exa$mple.org",
            err = ParseError::InvalidHost,
        }
        // Path validation covered by test_validate_path, this just checks that the path is
        // validated at all.
        err_invalid_path => {
            url = "fuchsia-pkg://example.org//",
            err = ParseError::InvalidPathSegment(_),
        }
        err_empty_hash => {
            url = "fuchsia-pkg://example.org/?hash=",
            err = ParseError::InvalidHash(_),
        }
        err_invalid_hash => {
            url = "fuchsia-pkg://example.org/?hash=INVALID_HASH",
            err = ParseError::InvalidHash(_),
        }
        err_uppercase_hash => {
            url = "fuchsia-pkg://example.org/?hash=A000000000000000000000000000000000000000000000000000000000000000",
            err = ParseError::UpperCaseHash,
        }
        err_hash_too_long => {
            url = "fuchsia-pkg://example.org/?hash=00000000000000000000000000000000000000000000000000000000000000001",
            err = ParseError::InvalidHash(_),
        }
        err_hash_too_short => {
            url = "fuchsia-pkg://example.org/?hash=000000000000000000000000000000000000000000000000000000000000000",
            err = ParseError::InvalidHash(_),
        }
        err_multiple_hashes => {
            url = "fuchsia-pkg://example.org/?hash=0000000000000000000000000000000000000000000000000000000000000000&\
            hash=0000000000000000000000000000000000000000000000000000000000000000",
            err = ParseError::MultipleHashes,
        }
        err_non_hash_query_parameter => {
            url = "fuchsia-pkg://example.org/?invalid-key=invalid-value",
            err = ParseError::ExtraQueryParameters,
        }
        err_resource_slash => {
            url = "fuchsia-pkg://example.org/name#/",
            err = ParseError::InvalidResourcePath(ResourcePathError::PathStartsWithSlash),
        }
        err_resource_leading_slash => {
            url = "fuchsia-pkg://example.org/name#/resource",
            err = ParseError::InvalidResourcePath(ResourcePathError::PathStartsWithSlash),
        }
        err_resource_trailing_slash => {
            url = "fuchsia-pkg://example.org/name#resource/",
            err = ParseError::InvalidResourcePath(ResourcePathError::PathEndsWithSlash),
        }
        err_resource_empty_segment => {
            url = "fuchsia-pkg://example.org/name#resource//other",
            err = ParseError::InvalidResourcePath(ResourcePathError::NameEmpty),
        }
        err_resource_bad_segment => {
            url = "fuchsia-pkg://example.org/name#resource/./other",
            err = ParseError::InvalidResourcePath(ResourcePathError::NameIsDot),
        }
        err_resource_percent_encoded_null => {
            url = "fuchsia-pkg://example.org/name#resource%00",
            err = ParseError::InvalidResourcePath(ResourcePathError::NameContainsNull),
        }
        err_resource_unencoded_null => {
            url =  "fuchsia-pkg://example.org/name#reso\x00urce",
            err = ParseError::InvalidResourcePath(ResourcePathError::NameContainsNull),
        }
        err_resource_empty_fragment => {
            url = "fuchsia-pkg://example.org/name#",
            err = ParseError::InvalidResourcePath(ResourcePathError::PathIsEmpty),
        }
        err_relative_resource_empty_fragment => {
            url = "name#",
            err = ParseError::InvalidResourcePath(ResourcePathError::PathIsEmpty),
        }
        err_resource_unencoded_control_character => {
            url = "fuchsia-pkg://example.org/name#reso\turce",
            err = ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '\t' }
            ),
        }
        err_resource_unencoded_newline => {
            url = "fuchsia-pkg://example.org/name#reso\nurce",
            err = ParseError::InvalidResourcePath(ResourcePathError::NameContainsNewline),
        }
        err_resource_trailing_carriage_return => {
            url = "fuchsia-pkg://example.org/name#resource\r",
            err = ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '\r' }
            ),
        }
        err_resource_trailing_space => {
            url = "fuchsia-pkg://example.org/name#resource ",
            err = ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: ' ' }
            ),
        }
        err_resource_unencoded_space => {
            url = "fuchsia-pkg://example.org/name#reso urce",
            err = ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: ' ' }
            ),
        }
        err_resource_unencoded_number_sign => {
            url = "fuchsia-pkg://example.org/name#reso#urce",
            err = ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '#' }
            ),
        }
        err_resource_invalid_percent_escape => {
            url = "fuchsia-pkg://example.org/name#reso%zzurce",
            err = ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '%' }
            ),
        }
        err_resource_unencoded_non_ascii => {
            url = "fuchsia-pkg://example.org/name#reso☺urce",
            err = ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '☺' }
            ),
        }
        err_relative_resource_unencoded_non_ascii => {
            url = "name#reso☺urce",
            err = ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '☺' }
            ),
        }
        err_resource_percent_encoded_invalid_utf8 => {
            url = "fuchsia-pkg://example.org/name#reso%FFurce",
            err = ParseError::ResourcePathPercentDecode(_),
        }
    }

    macro_rules! test_parse_ok {
        (
            $(
                $test_name:ident => {
                    url = $url:expr,
                    scheme = $scheme:expr,
                    host = $host:expr,
                    path = $path:expr,
                    hash = $hash:expr,
                    resource = $resource:expr,
                }
            )+
        ) => {
            $(
                #[test]
                fn $test_name() {
                    assert_eq!(
                        UrlParts::parse($url).unwrap(),
                        UrlParts {
                            scheme: $scheme,
                            host: $host,
                            path: $path.map(|s| s.parse::<Path>().unwrap()),
                            hash: $hash,
                            resource: $resource,
                        }
                    )
                }
            )+
        }
    }

    test_parse_ok! {
        ok_fuchsia_pkg_scheme => {
            url =  "fuchsia-pkg://",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = None,
        }
        ok_fuchsia_boot_scheme => {
            url =  "fuchsia-boot://",
            scheme = Some(Scheme::FuchsiaBoot),
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = None,
        }
        ok_host => {
            url =  "fuchsia-pkg://example.org",
            scheme = Some(Scheme::FuchsiaPkg),
            host = Some(Host::parse("example.org".into()).unwrap()),
            path = Option::<&str>::None,
            hash = None,
            resource = None,
        }
        ok_path_single_segment => {
            url =  "fuchsia-pkg:///name",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Some("name"),
            hash = None,
            resource = None,
        }
        ok_path_multiple_segment => {
            url =  "fuchsia-pkg:///name/variant/other",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Some("name/variant/other"),
            hash = None,
            resource = None,
        }
        ok_hash => {
            url =  "fuchsia-pkg://?hash=0000000000000000000000000000000000000000000000000000000000000000",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Option::<&str>::None,
            hash = Some(
                "0000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
            ),
            resource = None,
        }
        ok_resource_single_segment => {
            url =  "fuchsia-pkg://#resource",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = Some("resource".parse().unwrap()),
        }
        ok_resource_multiple_segment => {
            url =  "fuchsia-pkg://#resource/again/third",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = Some("resource/again/third".parse().unwrap()),
        }
        ok_resource_encoded_control_character => {
            url =  "fuchsia-pkg://#reso%09urce",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = Some("reso\turce".parse().unwrap()),
        }
        ok_resource_encoded_reserved_characters => {
            url =  "fuchsia-pkg://#reso%20%22%23%25%3C%3E%60urce",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = Some("reso \"#%<>`urce".parse().unwrap()),
        }
        ok_resource_encoded_non_ascii => {
            url =  "fuchsia-pkg://#reso%E2%98%BAurce",
            scheme = Some(Scheme::FuchsiaPkg),
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = Some("reso☺urce".parse().unwrap()),
        }
        ok_all_fields => {
            url =  "fuchsia-pkg://example.org/name\
            ?hash=0000000000000000000000000000000000000000000000000000000000000000\
            #resource",
            scheme = Some(Scheme::FuchsiaPkg),
            host = Some(Host::parse("example.org".into()).unwrap()),
            path = Some("name"),
            hash = Some(
                "0000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
            ),
            resource = Some("resource".parse().unwrap()),
        }
        ok_relative_path_single_segment => {
            url =  "name",
            scheme = None,
            host = None,
            path = Some("name"),
            hash = None,
            resource = None,
        }
        ok_relative_path_single_segment_leading_slash => {
            url =  "/name",
            scheme = None,
            host = None,
            path = Some("name"),
            hash = None,
            resource = None,
        }
        ok_relative_path_multiple_segment => {
            url =  "name/variant/other",
            scheme = None,
            host = None,
            path = Some("name/variant/other"),
            hash = None,
            resource = None,
        }
        ok_relative_path_multiple_segment_leading_slash => {
            url =  "/name/variant/other",
            scheme = None,
            host = None,
            path = Some("name/variant/other"),
            hash = None,
            resource = None,
        }
        ok_relative_hash => {
            url =  "?hash=0000000000000000000000000000000000000000000000000000000000000000",
            scheme = None,
            host = None,
            path = Option::<&str>::None,
            hash = Some(
                "0000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
            ),
            resource = None,
        }
        ok_relative_resource_single_segment => {
            url =  "#resource",
            scheme = None,
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = Some("resource".parse().unwrap()),
        }
        ok_relative_resource_multiple_segment => {
            url =  "#resource/again/third",
            scheme = None,
            host = None,
            path = Option::<&str>::None,
            hash = None,
            resource = Some("resource/again/third".parse().unwrap()),
        }
        ok_relative_all_fields => {
            url =  "name\
            ?hash=0000000000000000000000000000000000000000000000000000000000000000\
            #resource",
            scheme = None,
            host = None,
            path = Some("name"),
            hash = Some(
                "0000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
            ),
            resource = Some("resource".parse().unwrap()),
        }
    }
}

#[cfg(test)]
mod test_fragment {
    use super::*;
    use assert_matches::assert_matches;

    const HASH_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn rejects_unencoded_whitespace_and_control_characters() {
        // Trailing whitespace and unencoded C0/C1 control characters in `#<fragment>` are rejected
        // rather than stripped; they must be percent-encoded.
        for (bad_control_fragment, character) in [
            ("fuchsia-pkg://example.org/name#meta/a\tb.cm", '\t'),
            ("fuchsia-pkg://example.org/name#meta/a\rb.cm", '\r'),
            ("fuchsia-pkg://example.org/name#meta/a\x07b.cm", '\x07'),
            ("fuchsia-pkg://example.org/name#meta/a\x7fb.cm", '\x7f'),
            ("fuchsia-pkg://example.org/name#meta/a\u{0085}b.cm", '\u{0085}'),
        ] {
            assert_matches!(
                FuchsiaPkgAbsoluteComponentUrl::parse(bad_control_fragment),
                Err(ParseError::InvalidResourcePath(
                    ResourcePathError::UrlCannotContainUnencodedCharacter { character: c }
                )) if c == character,
                "input: {bad_control_fragment:?}"
            );
        }
        assert_matches!(
            FuchsiaPkgAbsoluteComponentUrl::parse("fuchsia-pkg://example.org/name#meta/a\0b.cm"),
            Err(ParseError::InvalidResourcePath(ResourcePathError::NameContainsNull))
        );
        assert_matches!(
            FuchsiaPkgAbsoluteComponentUrl::parse("fuchsia-pkg://example.org/name#meta/a\nb.cm"),
            Err(ParseError::InvalidResourcePath(ResourcePathError::NameContainsNewline))
        );
        // Trailing ASCII whitespace gets no special treatment: a trailing `' '` is rejected like
        // any other unencoded `' '`, and all other ASCII whitespace characters are control
        // characters.
        for (bad_trailing_ws_fragment, expected) in [
            (
                "fuchsia-pkg://example.org/name#meta/a.cm ",
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: ' ' },
            ),
            (
                "fuchsia-pkg://example.org/name# ",
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: ' ' },
            ),
            (
                "name#meta/a.cm ",
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: ' ' },
            ),
            (
                "fuchsia-pkg://example.org/name#meta/a.cm\t",
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '\t' },
            ),
            (
                "fuchsia-pkg://example.org/name#meta/a.cm\r",
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '\r' },
            ),
            ("fuchsia-pkg://example.org/name#meta/a.cm\n", ResourcePathError::NameContainsNewline),
            (
                "fuchsia-pkg://example.org/name#meta/a.cm\x0c",
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '\x0c' },
            ),
        ] {
            assert_matches!(
                ComponentUrl::parse(bad_trailing_ws_fragment),
                Err(ParseError::InvalidResourcePath(e)) if e == expected,
                "input: {bad_trailing_ws_fragment:?}"
            );
        }
        let encoded_space_comp =
            FuchsiaPkgAbsoluteComponentUrl::parse("fuchsia-pkg://example.org/name#meta/a%20b.cm")
                .unwrap();
        assert_eq!(encoded_space_comp.resource().as_ref(), "meta/a b.cm");
        assert_eq!(encoded_space_comp.to_string(), "fuchsia-pkg://example.org/name#meta/a%20b.cm");
        let encoded_trailing_space_comp = FuchsiaPkgAbsoluteComponentUrl::parse(
            "fuchsia-pkg://example.org/name#meta/a%20b.cm%20",
        )
        .unwrap();
        assert_eq!(encoded_trailing_space_comp.resource().as_ref(), "meta/a b.cm ");
        assert_eq!(
            encoded_trailing_space_comp.to_string(),
            "fuchsia-pkg://example.org/name#meta/a%20b.cm%20"
        );
        let encoded_tab_comp =
            FuchsiaPkgAbsoluteComponentUrl::parse("fuchsia-pkg://example.org/name#meta/a%09b.cm")
                .unwrap();
        assert_eq!(encoded_tab_comp.resource().as_ref(), "meta/a\tb.cm");
        assert_eq!(encoded_tab_comp.to_string(), "fuchsia-pkg://example.org/name#meta/a%09b.cm");
    }

    #[test]
    fn fragment_requires_percent_encoding_of_reserved_characters() {
        // `' '`, `'"'`, `'#'`, `'%'`, `'<'`, `'>'`, `` '`' ``, and non-ASCII characters (including
        // those that render invisibly or like other characters) must be percent-encoded anywhere in
        // `#<fragment>` (leading, interior, or trailing) for every URL shape with a fragment.
        // None of the prefixes or suffixes below begin with two hex digits, so `'%'` never begins
        // a valid `%HH` escape here.
        for (character, encoded) in [
            (' ', "%20"),
            ('"', "%22"),
            ('#', "%23"),
            ('%', "%25"),
            ('<', "%3C"),
            ('>', "%3E"),
            ('`', "%60"),
            ('é', "%C3%A9"),
            // No-break space.
            ('\u{a0}', "%C2%A0"),
            // Zero-width space.
            ('\u{200b}', "%E2%80%8B"),
            // Right-to-left override.
            ('\u{202e}', "%E2%80%AE"),
            // Cyrillic small letter o.
            ('\u{43e}', "%D0%BE"),
        ] {
            for (prefix, suffix) in [("", "meta/a.cm"), ("meta/a", "b.cm"), ("meta/a.cm", "")] {
                let expected = ResourcePathError::UrlCannotContainUnencodedCharacter { character };
                let fragment = format!("{prefix}{character}{suffix}");
                for raw_url in [
                    format!("fuchsia-pkg://example.org/name#{fragment}"),
                    format!("fuchsia-pkg://example.org/name?hash={HASH_HEX}#{fragment}"),
                    format!("name#{fragment}"),
                    format!("fuchsia-boot:///path#{fragment}"),
                    format!("fuchsia-builtin://#{fragment}"),
                ] {
                    assert_matches!(
                        UrlParts::parse(&raw_url),
                        Err(ParseError::InvalidResourcePath(e)) if e == expected,
                        "input: {raw_url:?}"
                    );
                    assert_matches!(
                        ComponentUrl::parse(&raw_url),
                        Err(ParseError::InvalidResourcePath(e)) if e == expected,
                        "input: {raw_url:?}"
                    );
                }

                // The percent-encoded form is accepted, decoded into the `Resource`, and
                // re-encoded identically by `Display`.
                let decoded_resource = format!("{prefix}{character}{suffix}");
                let encoded_url =
                    format!("fuchsia-pkg://example.org/name#{prefix}{encoded}{suffix}");
                let comp = FuchsiaPkgAbsoluteComponentUrl::parse(&encoded_url).unwrap();
                assert_eq!(comp.resource().as_ref(), decoded_resource, "input: {encoded_url:?}");
                assert_eq!(comp.to_string(), encoded_url);
            }
        }

        // Each URL type rejects the unencoded form, including package URL types, which validate
        // `#<fragment>` before rejecting a resource.
        assert_matches!(
            FuchsiaPkgAbsoluteComponentUrl::parse("fuchsia-pkg://example.org/name#a<b"),
            Err(ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '<' }
            ))
        );
        assert_matches!(
            RelativeComponentUrl::parse("name#a\"b"),
            Err(ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '"' }
            ))
        );
        assert_matches!(
            RelativeComponentUrl::parse("name#café"),
            Err(ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: 'é' }
            ))
        );
        assert_matches!(
            boot::AbsoluteComponentUrl::parse("fuchsia-boot:///#meta/a b.cm"),
            Err(ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: ' ' }
            ))
        );
        assert_matches!(
            builtin_url::BuiltinUrl::parse("fuchsia-builtin://#a`b"),
            Err(ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '`' }
            ))
        );
        assert_matches!(
            FuchsiaPkgComponentUrl::parse("fuchsia-pkg://example.org/name#a#b"),
            Err(ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '#' }
            ))
        );
        assert_matches!(
            AbsoluteComponentUrl::parse("fuchsia-pkg://example.org/path#100%"),
            Err(ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '%' }
            ))
        );
        assert_matches!(
            FuchsiaPkgAbsolutePackageUrl::parse("fuchsia-pkg://example.org/name#a>b"),
            Err(ParseError::InvalidResourcePath(
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '>' }
            ))
        );
        assert_eq!(
            ResourcePathError::UrlCannotContainUnencodedCharacter { character: '#' }.to_string(),
            "URL fragments cannot contain unencoded '#'; it must be percent-encoded"
        );

        // Lowercase escapes are accepted and displayed with uppercase hex digits.
        let lowercase = FuchsiaPkgAbsoluteComponentUrl::parse("fuchsia-pkg://a/b#%3ca%3e").unwrap();
        assert_eq!(lowercase.resource().as_ref(), "<a>");
        assert_eq!(lowercase.to_string(), "fuchsia-pkg://a/b#%3Ca%3E");

        // `?` may remain unencoded, while a literal `%` and non-ASCII characters must be encoded.
        let mixed =
            FuchsiaPkgAbsoluteComponentUrl::parse("fuchsia-pkg://a/b#100%25/?/%C3%A9").unwrap();
        assert_eq!(mixed.resource().as_ref(), "100%/?/é");
        assert_eq!(mixed.to_string(), "fuchsia-pkg://a/b#100%25/?/%C3%A9");
        let relative = RelativeComponentUrl::parse("name#100%25/?/%C3%A9").unwrap();
        assert_eq!(relative.resource().as_ref(), "100%/?/é");
        assert_eq!(relative.to_string(), "name#100%25/?/%C3%A9");

        // The requirement only applies to URL syntax: a decoded `Resource` may contain these
        // characters, and `Display` always percent-encodes them.
        let pkg = FuchsiaPkgAbsolutePackageUrl::parse("fuchsia-pkg://a/b").unwrap();
        let comp = FuchsiaPkgAbsoluteComponentUrl::from_package_url_and_resource(
            pkg,
            "a b\"#%<>`cé".to_string(),
        )
        .unwrap();
        assert_eq!(comp.to_string(), "fuchsia-pkg://a/b#a%20b%22%23%25%3C%3E%60c%C3%A9");
        assert_eq!(FuchsiaPkgAbsoluteComponentUrl::parse(&comp.to_string()).unwrap(), comp);
        let boot_comp = boot::AbsoluteComponentUrl::new(None, "a b\"#%<>`cé".parse().unwrap());
        assert_eq!(boot_comp.to_string(), "fuchsia-boot://#a%20b%22%23%25%3C%3E%60c%C3%A9");
        assert_eq!(
            boot::AbsoluteComponentUrl::parse(&boot_comp.to_string()).unwrap().resource(),
            boot_comp.resource()
        );
    }

    #[test]
    fn fragment_must_percent_encode_matches_fragment_set() {
        // An ASCII or non-ASCII character must be percent-encoded in `#<fragment>` exactly when
        // `Display` percent-encodes it.
        let ascii = (0u8..0x80).map(char::from);
        let non_ascii = [
            '\u{80}', '\u{85}', '\u{9f}', 'é', '☺', '𝄞', '\u{a0}', '\u{200b}', '\u{202e}',
            '\u{feff}',
        ];
        for character in ascii.chain(non_ascii) {
            let unencoded = character.to_string();
            let encoded_by_display = percent_encoding::utf8_percent_encode(&unencoded, FRAGMENT)
                .to_string()
                != unencoded;
            if character.is_ascii() {
                assert_eq!(
                    character.is_ascii_control()
                        || FRAGMENT_MUST_PERCENT_ENCODE.contains(&(character as u8)),
                    encoded_by_display,
                    "character {character:?}"
                );
            }
            assert_eq!(
                find_unencoded_fragment_char(&format!("a{character}b")),
                encoded_by_display.then_some(character),
                "character {character:?}"
            );
        }
        // A `%` that begins a valid `%HH` escape is encoded; any other `%` is not.
        for (fragment, expected) in [
            ("%41", None),
            ("%4a%4A%aa", None),
            ("a%2Fb%25", None),
            ("%C3%A9", None),
            ("%", Some('%')),
            ("%4", Some('%')),
            ("%4g", Some('%')),
            ("%g4", Some('%')),
            ("%%41", Some('%')),
            ("%41%", Some('%')),
            ("%C3%A9%", Some('%')),
            ("%é", Some('%')),
            ("é%", Some('é')),
        ] {
            assert_eq!(find_unencoded_fragment_char(fragment), expected, "fragment {fragment:?}");
        }
    }

    #[test]
    fn rejects_first_unencoded_character() {
        // The first character that must be percent-encoded determines the error, where an
        // unencoded NUL or newline is rejected like its percent-encoded form.
        for (fragment, expected) in [
            ("a b\0c", ResourcePathError::UrlCannotContainUnencodedCharacter { character: ' ' }),
            ("a\0b c", ResourcePathError::NameContainsNull),
            ("a\tb\nc", ResourcePathError::UrlCannotContainUnencodedCharacter { character: '\t' }),
            ("a\nb\0c", ResourcePathError::NameContainsNewline),
            ("100%\t", ResourcePathError::UrlCannotContainUnencodedCharacter { character: '%' }),
            ("é\u{85}", ResourcePathError::UrlCannotContainUnencodedCharacter { character: 'é' }),
            (
                "\u{85}é",
                ResourcePathError::UrlCannotContainUnencodedCharacter { character: '\u{85}' },
            ),
        ] {
            let url = format!("fuchsia-pkg://example.org/name#{fragment}");
            assert_matches!(
                FuchsiaPkgAbsoluteComponentUrl::parse(&url),
                Err(ParseError::InvalidResourcePath(e)) if e == expected,
                "input: {url:?}"
            );
        }
    }

    #[test]
    fn percent_encoded_fragments_round_trip() {
        // Every `%` must begin a valid `%HH` escape.
        for url in [
            "fuchsia-pkg://example.org/name#100%",
            "fuchsia-pkg://example.org/name#%4",
            "fuchsia-pkg://example.org/name#a%2",
            "fuchsia-pkg://example.org/name#%zz",
            "fuchsia-pkg://example.org/name#%4g",
            "fuchsia-pkg://example.org/name#%g4",
            "fuchsia-pkg://example.org/name#%%20",
            "fuchsia-pkg://example.org/name#%20%",
            "fuchsia-pkg://example.org/name#%FF%",
        ] {
            assert_matches!(
                FuchsiaPkgAbsoluteComponentUrl::parse(url),
                Err(ParseError::InvalidResourcePath(
                    ResourcePathError::UrlCannotContainUnencodedCharacter { character: '%' }
                )),
                "input: {url:?}"
            );
            assert_matches!(
                serde_json::from_value::<FuchsiaPkgAbsoluteComponentUrl>(
                    serde_json::Value::String(url.to_string())
                ),
                Err(_),
                "input: {url:?}"
            );
        }

        // Valid lowercase UTF-8 escapes, decoded interior slashes, and encoded `%` (decoded
        // exactly once), `#`, and `?` combined with `?hash=` round-trip through `Display`.
        for (raw_url, expected_res, expected_display) in [
            (
                "fuchsia-pkg://example.org/name#%e2%98%ba".to_string(),
                "☺",
                "fuchsia-pkg://example.org/name#%E2%98%BA".to_string(),
            ),
            (
                "fuchsia-pkg://example.org/name#a%2Fb".to_string(),
                "a/b",
                "fuchsia-pkg://example.org/name#a/b".to_string(),
            ),
            (
                "fuchsia-pkg://example.org/name#100%25".to_string(),
                "100%",
                "fuchsia-pkg://example.org/name#100%25".to_string(),
            ),
            (
                "fuchsia-pkg://example.org/name#%25zz".to_string(),
                "%zz",
                "fuchsia-pkg://example.org/name#%25zz".to_string(),
            ),
            (
                "fuchsia-pkg://example.org/name#%2520".to_string(),
                "%20",
                "fuchsia-pkg://example.org/name#%2520".to_string(),
            ),
            (
                "fuchsia-pkg://example.org/name#%25%20".to_string(),
                "% ",
                "fuchsia-pkg://example.org/name#%25%20".to_string(),
            ),
            (
                format!("fuchsia-pkg://example.org/name?hash={HASH_HEX}#a%23b?c"),
                "a#b?c",
                format!("fuchsia-pkg://example.org/name?hash={HASH_HEX}#a%23b?c"),
            ),
        ] {
            let parsed = FuchsiaPkgAbsoluteComponentUrl::parse(&raw_url).unwrap();
            assert_eq!(parsed.resource().as_ref(), expected_res, "input: {raw_url:?}");
            assert_eq!(parsed.to_string(), expected_display, "input: {raw_url:?}");
            assert_eq!(
                FuchsiaPkgAbsoluteComponentUrl::parse(&expected_display).unwrap(),
                parsed,
                "input: {raw_url:?}"
            );
            let from_serde: FuchsiaPkgAbsoluteComponentUrl =
                serde_json::from_value(serde_json::Value::String(raw_url.clone())).unwrap();
            assert_eq!(from_serde, parsed, "input: {raw_url:?}");
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            failure_persistence: None,
            ..Default::default()
        })]

        #[test]
        fn display_round_trips(res in crate::test::random_resource_path(1, 5)) {
            // Every resource path, including those with control, reserved, and non-ASCII
            // characters, displays as an ASCII URL that parses back to the same resource.
            let pkg = FuchsiaPkgAbsolutePackageUrl::parse("fuchsia-pkg://a/b").unwrap();
            let url =
                FuchsiaPkgAbsoluteComponentUrl::from_package_url_and_resource(pkg, res.clone())
                    .unwrap();
            let displayed = url.to_string();
            proptest::prop_assert!(displayed.is_ascii(), "{displayed:?}");
            proptest::prop_assert_eq!(
                FuchsiaPkgAbsoluteComponentUrl::parse(&displayed).unwrap(),
                url
            );

            let resource: Resource = res.parse().unwrap();
            let relative = format!("name#{}", resource.percent_encode());
            let relative = RelativeComponentUrl::parse(&relative).unwrap();
            proptest::prop_assert_eq!(relative.resource(), &resource);

            let boot = boot::AbsoluteComponentUrl::new(None, resource.clone());
            let reparsed = boot::AbsoluteComponentUrl::parse(&boot.to_string()).unwrap();
            proptest::prop_assert_eq!(reparsed.resource(), &resource);

            let builtin = format!("fuchsia-builtin://#{}", resource.percent_encode());
            let builtin = builtin_url::BuiltinUrl::parse(&builtin).unwrap();
            proptest::prop_assert_eq!(builtin.resource(), Some(&resource));
        }
    }
}

#[cfg(test)]
mod test_url_type_aliases {
    use super::*;

    #[test]
    fn component_url_round_trip() {
        let abs_empty = "fuchsia-boot://#my-resource";
        assert_eq!(ComponentUrl::parse(abs_empty).unwrap().to_string(), abs_empty);

        let abs_present = "fuchsia-boot://my-host/my/path\
            ?hash=0000000000000000000000000000000000000000000000000000000000000000#my-resource";
        assert_eq!(ComponentUrl::parse(abs_present).unwrap().to_string(), abs_present);

        let abs_encoded = "fuchsia-boot://my-host/my/path#my%20resource%25";
        assert_eq!(ComponentUrl::parse(abs_encoded).unwrap().to_string(), abs_encoded);

        let rel = "my-path#my-resource";
        assert_eq!(ComponentUrl::parse(rel).unwrap().to_string(), rel);
    }

    #[test]
    fn package_url_round_trip() {
        let abs_empty = "fuchsia-boot://";
        assert_eq!(PackageUrl::parse(abs_empty).unwrap().to_string(), abs_empty);

        let abs_present = "fuchsia-boot://my-host/my/path\
            ?hash=0000000000000000000000000000000000000000000000000000000000000000";
        assert_eq!(PackageUrl::parse(abs_present).unwrap().to_string(), abs_present);

        let rel = "my-path";
        assert_eq!(PackageUrl::parse(rel).unwrap().to_string(), rel);
    }
}
