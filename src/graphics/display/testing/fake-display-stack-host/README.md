# Fake Display Stack Host

The `BUILD.gn` in this directory a component package named
`fake-display-stack-host`. It publishes the
`fuchsia.hardware.display.Service` service that can be used
for hermetic testing without real hardware dependency.

## Introduction

The goal of `fake-display-stack-host` is to provide a display engine
driver (`fake-display`) and a display coordinator driver within a hermetic
testing environment. This removes the dependency on acquiring the display
coordinator, allowing tests to run in display-less environments hermetically and
allowing multiple display coordinator and display driver instances to co-exist.

## Usage

Realms should declare a child component in its component manifest for the
display coordinator connector. The child must be named
`fake-display-stack-host` to make its capabilities be correctly routed.

They should also include the corresponding shard component manifest file
`fake-display-stack-host.shard.cml` to route required capabilities to
the `fake-display-stack-host` child.

They should also explicitly offer the `fuchsia.hardware.display.Service`
service from `fake-display-stack-host` to clients (for example, Scenic).

For example, here is an excerpt of `ui_test_realm`
(`//src/ui/testing/ui_test_realm/meta/scenic.shard.cml`) declaring a fake
display coordinator connector from its own package, offering parent capabilities
to the display coordinator connector, and providing the
`fuchsia.hardware.display.Service` service to Scenic:

```json5
{
  include: [
    "//src/graphics/display/testing/fake-display-stack-host/meta/fake-display-stack-host.shard.cml",
  ],
  children: [
    {
      name: "fake-display-stack-host",
      url: "#meta/fake-display-stack-host.cm",
    },
  ],
  offer: [
    {
      service: "fuchsia.hardware.display.Service",
      from: "#fake-display-stack-host",
      to: ["#scenic"],
    },
  ],
}
```
