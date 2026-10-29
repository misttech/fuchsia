# Memory Bucket Definitions

This directory contains `.json5` files that define memory buckets for Inspect and Cobalt metrics.

Bucket definitions assign each VMO to a single bucket by applying rules in ascending `order`. Each
VMO falls into the first bucket that matches (or into `Undigested` if none matches).

A bucket matches a VMO when all three criteria (`process`, `vmo`, and `principal`) are satisfied:

* Each criterion is a list of ASCII prefixes matched against the owning process name, the VMO name,
  and the owning principal name (`memory_monitor2` only).
* Prefixes are matched literally, **not** as regular expressions: do not use `.*` wildcards or
  escape special characters (for example, write `"archivist.cm"`, not `"archivist\\.cm.*"`).
* A criterion is satisfied if the name starts with any of its listed prefixes. When no prefix is
  specified (`[]`, or when `principal` is omitted), the criterion is always satisfied.
* If a VMO is shared across multiple processes or principals, the `process` or `principal` criterion
  is satisfied as soon as at least one of its owning processes or principals matches a prefix.

## Example

```json5
[
    {
        name: "Netstack",
        process: [
            "dhcpd.cm",
            "dns_resolver.cm",
            "netstack",
        ],
        vmo: [],
        event_code: 20,
        order: 50,
    },
]
```
