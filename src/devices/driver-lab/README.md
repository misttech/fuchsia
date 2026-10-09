# driver-lab proxy driver

This directory contains the `driver-lab` proxy driver, a standalone driver
that can claim device nodes and expose them to host-side scripting at run-time
for rapid driver development and debugging.

See the [specification](./SPEC.md) for more details.

**Note:** This code is experimental and has not been subject to the standard
level of human code review.

---

Engineering-only DFv2 driver (`lab_proxy`) providing policy-checked,
audited hardware access for host-driven driver development. Phase 1
offers read-only access and serves the `fuchsia.driver.lab` wire
contract consumed by the host tooling at `//tools/driver-lab`. The
proxy binds only to nodes explicitly marked with the
`fuchsia.driver.lab.PROXY_TARGET` property -- no production node carries
it, so the proxy never binds opportunistically.

Every operation is validated against the immutable target ceiling and
the session's exact allowlist immediately before access, and every
attempt -- including rejections -- is recorded in a bounded audit ring.
The proxy must never be offered by production images.

The normative specification is [SPEC.md](SPEC.md); the companion host
tooling specification is at `//tools/driver-lab/SPEC.md`.

## Tracing

Every audit record is also written as a Fuchsia trace instant event, and
every successful write samples a counter track named
`<node>/<resource_name>`, so probes and register or knob A/B arms line up
with scheduler, power, and other system trace data.

| Category         | Contents                                                |
| ---------------- | ------------------------------------------------------- |
| `driver_lab`     | All audit records except interrupt taps; write counters |
| `driver_lab:irq` | Interrupt taps, which can be high-rate                  |

```
$ ffx trace start --categories '#default,driver_lab,driver_lab:irq'
```

Event arguments use the field names of `target-audit.jsonl` evidence rows,
so events can be joined with a bundle's rows on `seq`. The `node` argument
identifies the emitting instance when several are traced at once. Embedded
servers in builds without `enable_driver_lab` emit no events. See
[`src/trace.rs`](src/trace.rs).

## Testing

Host-side unit tests for the policy/audit/executor core:

```
$ fx test --host lab_proxy_core_lib_test
```

Target unit tests and driver realm tests:

```
$ fx test lab_proxy-unit-test lab_proxy-realm-test
```
