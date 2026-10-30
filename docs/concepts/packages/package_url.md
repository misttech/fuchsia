# Fuchsia package URLs

A Fuchsia package URL looks like the following:

Note: You can use the code block in [Build a package URL](#build-package-url)
to create a package URL to then copy and paste.

```
fuchsia-pkg://<repository>/<package-name>?hash=<package-hash>#<resource-path>
```

It consists of the following components, which form the full package URL:

* [URL scheme](#url-scheme): Required
* [Repository](#repository): Required
* [Package name](#package-name): Required
* [Package hash](#package-hash): Optional
* [Resource path](#resource-path): Optional

## URL scheme {#url-scheme}

The `fuchsia-pkg` URL scheme combines identifying characteristics to
establish a means for referring to a repository, a package, or a
resource, depending on which parts are included.

### Syntax

**Required**

```
{{ '<strong>' }}fuchsia-pkg://{{ '</strong>' }}<repository>[/<package-name>[?hash=<package-hash>][#<resource-path>]]
```

The scheme of a Fuchsia package are the case-insensitive characters, `fuchsia-pkg://`.

Although the canonical form is lower-case, URL scheme encoding is case-insensitive therefore
the system handles all cases.

## Repository {#repository}

The package URL contains a repository [hostname] to identify the package's
source. A hostname is defined as a sequence of dot (`.`)-delimited [LDH labels],
each of which consists of 1 to 63 of the following latin-1 characters, and does
not begin or end with a hyphen (`-`):

* Digits (`0` to `9`)
* Lower-case letters (`a` to `z`)
* Hyphen (`-`)

No other characters are permitted.

The total maximum length of a hostname is 253 characters including the dots.

### Repository root verification (known sources)

The repository's root role (a quorum of one or more public or private key pairs)
establishes a chain of trust such that package authenticity, integrity, and
freshness can be verified cryptographically. The root role signs keys for more
limited roles which are then used to sign package metadata and the targets
themselves. See [TUF Security][TUF Security] and
[TUF roles and metadata][TUF METADATA] for more details.

To verify that a package is authentic, you must verify that the repository
from which it is being downloaded is authentic.

This will be implemented by maintaining a list of known source repositories
with their public keys on the device. Packages from unknown sources will
be rejected, although, on certain build types, new repositories can be added
at runtime.

### Syntax

**Required**

```
fuchsia-pkg://{{ '<strong>' }}<repository>{{ '</strong>' }}/<package-name>?hash=<package-hash>#<resource-path>
```

#### Examples

`fuchsia-pkg://{{ '<strong>' }}fuchsia.com{{ '</strong>' }}`

## Package name {#package-name}

A package name is a symbolic label that identifies a package. The package name is
used to locate package metadata within a repository. Package metadata must be signed
by a role which is trusted by the repository root.

A package name consists of a sequence of up to 255 of the following latin-1
characters in any order:

* Digits (`0` to `9`)
* Lower-case letters (`a` to `z`)
* Hyphen (`-`)
* Underscore (`_`)
* Period (`.`)

No other characters are permitted.

Each package name must be unique among all packages in a repository.
Packages within different repositories are considered distinct even
if they have the same name.

### Syntax

**Required**

```
fuchsia-pkg://<repository>/{{ '<strong>' }}<package-name>{{ '</strong>' }}?hash=<package-hash>#<resource-path>
```

There must be a single `/` character between the repository and [package name](#package-name).

#### Examples

* `fuchsia-pkg://fuchsia.com/{{ '<strong>' }}fuchsia-shell-utils{{ '</strong>' }}`
* `fuchsia-pkg://fuchsia.com/{{ '<strong>' }}fuchsia-shell-fonts{{ '</strong>' }}`
* `fuchsia-pkg://fuchsia.com/{{ '<strong>' }}fuchsia-shell-scenic{{ '</strong>' }}`

## Package hash {#package-hash}

A package hash is the [merkleroot] of the package's meta.far.  Because the
package's metadata encodes the content addresses of the package's files, any
changes to the package's metadata or content will produce a different package
hash, thereby making it possible to distinguish each unique revision of the
package.

If the package hash is missing, the package resolver fetches the resources
from the newest revision of the package available to the client.

A package hash is represented as a hex-encoded string consisting of exactly 64
of the following latin-1 characters: digits (`0` to `9`) and lower-case letters
(`a` to `f`).  No other characters are permitted.

### Syntax

**Optional**

```
fuchsia-pkg://<repository>/<package-name>{{ '<strong>' }}?hash=<package-hash>{{ '</strong>' }}#<resource-path>
```

Only valid if a package name is specified.

Must begin with the string `?hash=` followed by the [package hash](#package-hash).

#### Examples

`fuchsia-pkg://google.com/chrome{{ '<strong>' }}?hash=80e8721f4eba5437c8b6e1604f6ee384f42aed2b6dfbfd0b616a864839cd7b4a#meta/webview.component{{ '</strong>' }}`

## Resource path {#resource-path}

A resource path is a UTF-8 string that identifies a resource within a package.
It is a relative file path consisting of single-`/`-delimited segments. Each
segment must be a non-empty UTF-8 byte sequence of at most 255 bytes, not equal
to `.` or `..`, and must not contain NUL (`\0`) or newline (`\n`) characters.
This definition matches [Fuchsia filesystem paths] except that it requires
valid UTF-8 (the parser decodes `%HH` escapes as UTF-8 and rejects other byte
sequences), rejects newline characters, and has no limit on the total path
length.

In a URL, a single `#` character precedes the percent-encoded resource path.

Note: The scheme, [repository hostname](#repository-hostname),
[package name](#package-name), and [package hash](#package-hash) components are
all defined to use a restricted subset of characters, none of which require
encoding, unlike the resource path.

The Fuchsia URL parser decodes each `%HH` escape (where `HH` is two hexadecimal
digits in either case) in `#<resource-path>` as in [RFC 3986], and then
validates the decoded resource path. Because the parser decodes escapes before
it splits the resource path into segments, `%2F` acts as a segment separator.

You must percent-encode the following characters, which the parser rejects when
they appear unencoded after the `#`:

| Character | Encoding |
| --------- | -------- |
| Space | `%20` |
| Double quote (`"`) | `%22` |
| Number sign (`#`) | `%23` |
| Percent sign (`%`), unless it begins a `%HH` escape | `%25` |
| Less-than sign (`<`) | `%3C` |
| Greater-than sign (`>`) | `%3E` |
| Backtick (`` ` ``) | `%60` |
| Control characters U+0000 to U+001F | `%00` to `%1F` |
| Control characters U+007F to U+009F | `%7F` and `%C2%80` to `%C2%9F` |
| Any other non-ASCII character | Its UTF-8 bytes, such as `%C3%A9` for `é` |

Because every `%` must begin a `%HH` escape, the parser rejects a `%` that isn't
followed by two hexadecimal digits (for example, in `100%` or `%zz`). You can
include any other ASCII character unencoded. Requiring non-ASCII characters to
be percent-encoded keeps every URL ASCII, so that characters that are invisible
or that look like other characters (such as a no-break space, a right-to-left
override, or a Cyrillic `о` (U+043E)) can't hide in a URL. Because the decoded
resource path must still follow the rules above, `%00` (NUL) and `%0A`
(newline) are invalid even though they are encoded.

Canonical URLs encode the resource path using the
[WHATWG fragment percent-encode set] plus `%` and `#`. This encodes exactly the
characters in the preceding table, using uppercase hexadecimal digits.

For example, `hello/unicode/%F0%9F%98%81` decodes to `hello/unicode/😁`, whereas
the parser rejects an unencoded `#hello/unicode/😁`. Similarly,
`data/my%20file.txt` decodes to `data/my file.txt` and `data/100%25.txt` decodes
to `data/100%.txt`, whereas the parser rejects an unencoded `#data/my file.txt`
or `#data/100%.txt`.

### Syntax

**Optional**

Only valid if a package was specified.

```
fuchsia-pkg://<repository>/<package-name>?hash=<package-hash>{{ '<strong>' }}#<resource-path>{{ '</strong>' }}
```

#### Examples

* `fuchsia-pkg://fuchsia.com/fuchsia-shell-utils{{ '<strong>' }}#bin/ls{{ '</strong>' }}`
* `fuchsia-pkg://google.com/chrome{{ '<strong>' }}#meta/webview.component{{ '</strong>' }}`
* `fuchsia-pkg://google.com/chrome{{ '<strong>' }}#lib/mylibrary.so{{ '</strong>' }}`

## Build a package URL {#build-package-url}

You can use the editable variables to build your package URL and then copy it:

```
fuchsia-pkg://{{ '<var>' }}repository{{ '</var>' }}/{{ '<var>' }}package-name{{ '</var>' }}?hash={{ '<var>' }}package-hash{{ '</var>' }}#{{ '<var>' }}resource-path{{ '</var>' }}
```

<!--xrefs-->
[TUF Specification]: https://github.com/theupdateframework/specification/blob/HEAD/tuf-spec.md#4-document-formats
[TUF Security]: https://theupdateframework.github.io/security.html
[TUF Metadata]: https://theupdateframework.github.io/metadata.html
[hostname]: https://en.wikipedia.org/wiki/Hostname
[RFC 1123]: https://tools.ietf.org/html/rfc1123
[RFC 5890]: https://tools.ietf.org/html/rfc5890
[LDH labels]: https://tools.ietf.org/html/rfc5890#section-2.3.1
[Fuchsia filesystem paths]: /docs/concepts/process/namespaces.md#object-relative-path-expressions
[RFC 3986]: https://tools.ietf.org/html/rfc3986#page-11
[WHATWG fragment percent-encode set]: https://url.spec.whatwg.org/#fragment-percent-encode-set
[merkleroot]: /docs/concepts/security/merkleroot.md
