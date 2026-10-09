<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# driver-lab proxy driver specification

This is the normative specification for the engineering-only driver-lab
proxy driver (`lab_proxy`) at `//src/devices/driver-lab`. The companion
host tooling specification lives at `//tools/driver-lab/SPEC.md`.

The specification is delivered in three phases. Phase 1 -- unclaimed-node
binding through existing Driver Framework mechanisms, with no Driver Manager
changes -- is delivered first. Phase 2 -- in-situ cooperative debugging via an
embedded driver-lab library -- extends the architecture to existing, active drivers
without managed takeover or Driver Manager modifications, allowing live register
inspection while preserving device state. Phase 3 -- software state banks,
runtime knobs, and diagnostic triggers -- extends in-situ debugging beyond
physical hardware registers so host plans can observe internal driver state,
inject bounded race-window delays, toggle candidate fixes at runtime, and trigger
diagnostic actions in a single build.

## Phase 1: unclaimed-node proxy

### Implementation status

Changeset numbers (CS) are global across this specification and
`//tools/driver-lab/SPEC.md`, whose changesets interleave with these;
gaps in the numbering below are driver-lab changesets. Every changeset
marks itself complete in this list when it lands, so at any commit this
list shows which changes precede and which follow that commit.

- [x] CS1 `[driver-lab] Proxy driver specification` -- this
      document.
- [x] CS3 `[driver-lab] Phase 1 proxy driver core` -- the
      takeover-ready `fuchsia.driver.lab` wire contract (sections 7.4,
      11) and the host-testable policy core: ceiling and exact
      allowlist enforcement (9, 10), per-boot identity and description
      digests (8.3, 10.2), bounded reads and snapshots (12.1, 12.5),
      session lifecycle and leases (16), and the audit ring (18). Bind
      rules are `false` and no resources are offered.
- [x] CS9 `[driver-lab] Platform resource provider, bind rules,
      realm tests` -- resource acquisition and local MMIO mapping
      (8.4, 15), the `PROXY_TARGET` bind library and property-gated
      binding (7.2, 7.3), driver realm tests (23.3), and the lab_root
      test fixture serving a fake platform device (23.3, and the
      live-activation path documented by CS10).

- [x] CS10 `[driver-lab] Add live-target conformance runbook` (in
      //tools/driver-lab/SPEC.md).
- [x] CS11 `[driver-lab] Host node discovery via fuchsia.driver.development`
      (in //tools/driver-lab/SPEC.md).
- [x] CS12 `[driver-lab] Direct mode client and published-protocol workflow`
      (in //tools/driver-lab/SPEC.md).
- [x] CS13 `[driver-lab] Target ceiling policy manifests and verification`
      (9.2, 9.6, 22, 23.1; milestone P1 remainder).

Remaining phase 1 work:

- [x] CS14 `[driver-lab] Bounded sequences and mutation in proxy driver`
      (11.4, 11.5, 12.2-12.4, 13; milestone P3).
- [x] CS15 `[driver-lab] Host tooling support for mutation and sequences`
      (in //tools/driver-lab/SPEC.md).
- [x] CS16 `[driver-lab] Driver-shaped public Python API` (in
      //tools/driver-lab/SPEC.md).
- [x] CS17 `[driver-lab] Stop-path hardening and cancellation` (19; milestone P2).
- [x] CS18 `[driver-lab] Host CLI subtool expansion and verified teardown`
      (in //tools/driver-lab/SPEC.md).
- [x] CS19 `[driver-lab] Protocol resources, typed endpoints, and heterogeneous sequence`
      (8.1, 8.4, 9.5, 11.5, 11.6, 12, 14.2; milestone P4).
- [ ] Engineering assembly inclusion and production-absence
      verification (7.1, 25).
- [x] CS20 `[driver-lab] Interrupt observation and local acknowledgement`
      (11.7, 17; milestone P5).
- [x] CS21 `[driver-lab] Structured configuration and per-class rate limits`
      (12.1, 22).
- [x] CS22 `[driver-lab] Serial capture and independent recovery integration`
      (in //tools/driver-lab/SPEC.md; milestone H5).

Phase: 1 -- new-driver development. The proxy binds to unclaimed nodes through
existing Driver Framework mechanisms. Existing-driver debugging via an embedded
library is specified in the Phase 2 section of this document.

Scope: standalone proxy driver, target policy, resource execution, audit, and
lifecycle.

Companion specification: host tooling at `//tools/driver-lab/SPEC.md`

### 1. Purpose

The proxy driver provides exclusive, resource-scoped hardware access on a
Fuchsia target for interactive and agent-assisted driver development. It runs
locally on the target, maps only resources offered to its device node, executes
bounded operations, enforces a target ceiling and session allowlist, and
records a target-side audit trail.

The driver-lab capability supports two deployment situations:

1. it is selected as the standalone proxy driver for an unclaimed device node
   during new-driver exploration (phase 1, specified here); or
2. its core functionality is embedded as a library in an existing, active driver
   for in-situ live debugging (phase 2).

The core policy, execution, audit, and wire contracts serve both situations.
Phase 1 specifies the standalone driver; Phase 2 specifies the embedded library.

### 2. Normative language

The terms **must**, **must not**, **should**, and **may** describe requirements,
recommendations, and optional behavior respectively.

The following terms have specific meanings:

- **Proxy:** The standalone DFv2 driver defined here.
- **Target ceiling:** Immutable target policy describing the greatest access
  the proxy may provide for a node/resource class.
- **Session allowlist:** Exact plan-derived permissions supplied when a session
  opens. It may only narrow the target ceiling.
- **Managed takeover:** Driver Manager stops a node's current driver, binds the
  proxy, and later restores the original driver. Defined in the Phase 2
  section of this document.
- **Resource identity:** A stable logical description and digest for an offered
  resource. It never includes a host-provided physical address.
- **Partial completion:** An execution in which at least one operation began
  but the complete plan did not finish successfully.

### 3. Goals

The proxy must:

1. Run in a dedicated driver host.
2. Obtain resources only from the node to which it is bound.
3. Support platform-, PCI-, and other parent-native resource providers through
   narrow backend adapters.
4. Map MMIO locally with device-appropriate cache policy.
5. Retain all objects required for each mapping's lifetime.
6. Represent resources by stable logical IDs and names.
7. Describe resource size, kind, supported operations, target ceiling, and
   identity digest.
8. Enforce both the target ceiling and the exact session allowlist immediately
   before every access.
9. Execute bounded MMIO reads, snapshots, masked writes, polls, delays, and
   barriers.
10. Support resource-specific GPIO, SPI, I2C, serial, clock, reset, and
    interrupt operations without erasing their production semantics.
11. Execute timing-sensitive sequences locally when explicitly requested.
12. Serialize mutation and reject ambiguous concurrent execution.
13. Report explicit per-operation status and partial completion.
14. Record attempted, rejected, successful, failed, and cleanup operations in a
    bounded audit ring.
15. Close or cancel every outstanding request during stop.
16. Contain an experimental driver fault away from Driver Manager and unrelated
    production drivers.
17. Be included only in authorized engineering configurations and remain
    inactive until explicitly bound.

### 4. Non-goals

The proxy must not:

- accept an arbitrary physical address;
- request root resource;
- export MMIO VMOs, interrupt handles, BTIs, IOMMUs, or root-like capabilities
  to the host;
- map resources belonging to another node;
- share ownership with the normal driver for the same node;
- inspect the memory or private state of an active production driver;
- run inside Driver Manager;
- execute arbitrary uploaded code or target-side scripts;
- load a host-provided policy that widens the target ceiling;
- claim transactional rollback for MMIO;
- automatically restore every register from captured "before" values;
- permit unbounded polling, sleeping, snapshots, vectors, or audit output;
- expose DMA programming, arbitrary PCI configuration writes, or SMC calls in
  the initial protocol;
- bind broadly to normal nodes without explicit Driver Manager selection or
  engineering-only bind metadata;
- coordinate operations across multiple device nodes in one session;
- guarantee sub-millisecond timing precision for delays, polls, or
  inter-operation gaps in V1; or
- appear in a production image.

### 5. Architecture

```text
Host tooling
    |
    | ffx target transport
    v
Driver Manager control plane
  discovery: fuchsia.driver.development
    |
    | bind proxy (phase 1) / connect embedded endpoint (phase 2)
    v
Target Component (standalone lab_proxy in phase 1, active driver in phase 2)
    |
    +-- policy and session server
    +-- MMIO executor
    +-- protocol-resource adapters
    +-- interrupt tracker / event tap
    +-- audit ring
    |
    v
Hardware resources mapped to the target driver
```

Driver Manager owns discovery (`fuchsia.driver.development`). The session server
owns the experimental session and bounded hardware execution after connection.

### 6. Design invariants

#### 6.1 Capability confinement

The proxy can access only resources offered to its bound node. It must use the
parent's normal platform, PCI, or bus protocol. It must not obtain a global
resource and reconstruct mappings from host input.

#### 6.2 Hardware safety and concurrency

In phase 1 the standalone proxy binds only to nodes with no bound driver. In
phase 2 the embedded library runs inside the active driver and enforces safety
through cooperative locking, quiesce hooks for mutating sequences, and ceiling
denials on destructive reads.

#### 6.3 Target ceiling is authoritative

The host may provide an exact read consent list and an approved plan, but it
cannot:

- add a resource;
- increase a resource size;
- add an operation width;
- remove a hard deny;
- increase a target deadline;
- enable a disabled resource class; or
- widen a write mask.

#### 6.4 Local execution

MMIO mappings and interrupt objects remain on the target. FIDL carries scalar
operations, protocol transactions, results, descriptions, and audit entries.

#### 6.5 Programming-model fidelity

The target wire contract should preserve the meaning of normal driver
operations:

- protocol-backed resources retain production FIDL request and response
  semantics where possible;
- MMIO remains explicitly width- and offset-based;
- interrupt waiting remains asynchronous;
- reset and clock state changes remain explicit;
- SPI/I2C/serial exchanges do not become generic byte writes; and
- errors are not collapsed into prose.

A generic sequence protocol may coordinate heterogeneous operations locally,
but it must identify the original resource operation precisely. It is an
execution mechanism, not a replacement programming model.

#### 6.6 Bounded execution

Every request has target-enforced bounds. No host field can select an infinite
timeout, unconstrained vector, or blocking dispatcher sleep.

#### 6.7 Explicit partial completion

Writes generally cannot be undone. Execution reports every operation that
began, stops according to declared error rules, and never labels the sequence
atomic.

#### 6.8 Audit is diagnostic, not durable authorization

The proxy audit establishes what it attempted under a run/session context. It
does not prove that the host approval was issued by a particular human.

### 7. Deployment and activation

#### 7.1 Engineering-only packaging

The proxy package should be present by default in the engineering
configurations that support driver-lab workflows. Presence alone grants no
hardware access.

Production images must not offer any of the following, and the build must
verify this:

- the proxy driver package;
- the Driver Manager takeover protocol (phase 2);
- special debug bind metadata;
- target debug policy artifacts; or
- debug capability routes.

The verification mechanism (package absence, a Driver Manager binary variant,
or config gating with capability-route verification) is owned by platform
assembly. This specification requires only that the chosen mechanism be
verifiable at build time.

#### 7.2 Inactive by default

The proxy must not opportunistically claim normal device nodes. In phase 1 it
becomes eligible only through explicit engineering-only node metadata or a
developer-created test node; it may be loaded through engineering assembly
inclusion or ephemeral registration (`fuchsia.driver.registrar`). Plans
cannot supply a driver URL.

In phase 2 a Driver Manager operation may additionally select the fixed proxy
identity for an exclusive managed takeover. That node-scoped force-bind is
new Driver Manager work (the existing `DisableDriver` is global by URL and
unsuitable) and is specified in the Phase 2 section of this document.

#### 7.3 Direct selection during development

For hardware without a normal driver, an engineering product or explicit
developer action selects the proxy at initial bind. This is the phase 1 activation path. It uses the same policy, FIDL,
evidence, and session implementation that the embedded library (phase 2) reuses.

#### 7.4 Embedded library in-situ debugging (phase 2)

In-situ debugging -- embedding the driver-lab server into an active driver,
sharing mapped MMIO via VMO duplication, coordinating concurrency via quiesce
hooks, and tapping interrupts -- is specified in the Phase 2 section of this
document. The phase 1 wire contract serves both standalone and embedded
deployments with no major version change.

### 8. Resource model

#### 8.1 Resource kinds

The extensible resource model begins with:

- MMIO region;
- interrupt;
- GPIO protocol;
- SPI protocol;
- I2C protocol;
- serial protocol;
- clock protocol; and
- reset protocol.

Additional kinds require explicit capability descriptions, bounds, tests, and
threat-model review.

#### 8.2 Resource description

Each resource description contains:

- numeric ID scoped to the proxy instance;
- stable logical name when available;
- resource kind;
- provider identity/type;
- logical size where applicable;
- supported access widths or protocol methods;
- read/write classification;
- target limits;
- stable metadata used to compute a resource-description digest; and
- flags identifying missing or provisional metadata.

Descriptions never expose physical addresses or raw target handles.

#### 8.3 Resource digest

The digest is computed from canonical descriptions including:

- node identity criteria;
- provider kind;
- resource ordinal/name;
- logical size;
- operation widths;
- the immutable target-ceiling entries that apply to this resource, not the
  whole policy manifest; and
- relevant stable parent metadata.

Dynamic handle values, mapping addresses, boot IDs, and service-instance names
are excluded.

The digest allows a persistent host read grant to stop matching when resource
layout or policy changes. Scoping the ceiling input per resource ensures that
a policy change on one resource does not invalidate grants on unrelated
resources.

#### 8.4 Parent adapters

Use an injected `ResourceProvider` interface with implementations appropriate
to the target node:

```text
ResourceProvider
    DescribeResources()
    OpenMmio(id)
    OpenInterrupt(id)
    ConnectProtocol(id, kind)

PlatformResourceProvider
PciResourceProvider
Other reviewed bus-specific providers
FakeResourceProvider
```

Do not place bus-specific address translation in the FIDL server or policy
engine.

If a parent exposes a larger region than the proxy's logical resource, checked
translation must verify both the logical resource and actual backing mapping:

```text
backing_offset = logical_base + request_offset
```

All arithmetic is checked before access.

Resource providers communicate with the parent over zircon channel-transport
FIDL. The isolated driver-host requirement (goal 1) already forces this:
driver-transport (`fdf`) protocols require colocation with the parent.
Parents that offer resources only over driver transport are ineligible for
proxy access in V1; whether a colocated mode is ever introduced is a phase 2
open decision.

### 9. Target policy

#### 9.1 Policy layers

The proxy evaluates:

```text
actual resource bounds
    ∩ immutable target ceiling
    ∩ session allowlist
    ∩ session mode
    ∩ per-operation structural validation
```

Every layer must permit the operation.

#### 9.2 Immutable target ceiling

The ceiling comes from engineering assembly, board/bus metadata, or immutable
package content. Runtime configuration may narrow it but cannot widen it.

It specifies:

- permitted resource providers and resource kinds;
- maximum logical region sizes;
- access widths;
- hard-denied read offsets or ranges;
- whether unknown reads may be enabled by a session grant;
- exact writable offsets and masks;
- precondition/readback requirements;
- permitted protocol methods and maximum transaction sizes;
- interrupt observation permission;
- maximum operations, snapshot items, poll time, delay, and sequence duration;
- audit capacity; and
- cleanup rules.

#### 9.3 Unknown reads

For a resource whose ceiling permits operator-authorized exploration, an
otherwise unclassified aligned read may be allowed only when:

- the offset fits the logical and actual resource;
- it is not hard-denied;
- the exact resource, offset, width, and access class appear in the session
  allowlist; and
- all rate, count, and duration limits are satisfied.

This is an engineering risk decision. The proxy does not claim that an allowed
unknown read is side-effect free.

A session request to sweep a range is expanded into exact offsets before the
session opens. The proxy does not accept a wildcard "all offsets in this
region" allowlist rule.

#### 9.4 Writes

Writes are denied unless the immutable target ceiling contains an exact
writable-register entry. A host/session grant cannot invent a write entry.

Each entry specifies:

- offset and width;
- allowed mask;
- whether read-modify-write is valid;
- allowed precondition mask;
- whether a precondition is required;
- readback mask and semantics;
- barrier requirements;
- failure classification; and
- optional explicitly reviewed cleanup.

Write-one-to-clear, self-clearing, FIFO-like, or destructive-read registers
require a distinct operation semantic. Do not overload ordinary masked writes.

#### 9.5 Protocol-backed resources

For GPIO, SPI, I2C, serial, clock, reset, and future protocols, policy uses
method-specific bounds rather than pretending they are MMIO:

- method allowlist;
- transfer length;
- bus address or chip-select restrictions where applicable;
- configuration ranges;
- timeout/deadline;
- permitted state changes; and
- reset/cleanup behavior.

#### 9.6 Policy generation

When policy is expressed in JSON5 or another checked-in format, generate:

1. a target-native immutable table; and
2. a canonical manifest returned in `Describe`.

Generation rejects duplicate IDs/names, overflow, ambiguous overlap, invalid
masks, unreachable cleanup, nondeterministic ordering, and limits above FIDL
maxima.

Hash the canonical manifest bytes. Evidence records the digest.

### 10. Session allowlist

#### 10.1 Purpose

The session allowlist is derived from the canonical host plan after permission
resolution. It prevents a stale or buggy client from accessing anything beyond
the frozen run, even though it is not cryptographic proof of approval.

#### 10.2 Required identity

Opening a session includes:

- run and case IDs;
- plan digest;
- host tool version;
- expected boot ID;
- expected proxy generation;
- expected resource-description digest;
- expected target-policy digest;
- read-only or mutating mode;
- exact allowed operations or access rules; and
- bounded plan limits.

The proxy rejects a stale identity before access.

#### 10.3 Narrowing validation

Before opening the session, the proxy validates the entire allowlist against
the target ceiling. A rejected allowlist creates no session and performs no
hardware operation.

A read-only session can never issue a write even if a write appears in target
policy. A mutating session additionally requires an exclusive mutation lease.

### 11. Wire contract

The final FIDL syntax must match the selected Fuchsia revision. The semantic
contract below is normative. The V1 contract is implemented as the
`fuchsia.driver.lab` library with the `Proxy` (`Describe`,
`OpenSession`) and `Session` (`Read32`, `Snapshot`, `ReadAudit`) protocols.

#### 11.1 Limits

The protocol declares compile-time bounds for:

- resource names and device IDs;
- resource count;
- operation count;
- snapshot item count;
- protocol transaction size;
- audit page size;
- detail strings;
- session allowlist rules; and
- total encoded response size.

Runtime target limits may be smaller.

#### 11.2 Proxy description

`Describe` returns:

- protocol major/minor;
- proxy implementation version;
- proxy instance/generation;
- boot ID;
- node identity and topology generation;
- takeover identity when applicable (reserved; populated only in phase 2);
- target policy name/digest;
- canonical resource descriptions and digest;
- supported resource and operation kinds;
- runtime limits;
- audit capacity; and
- fault-isolation/session capabilities.

#### 11.3 Session creation

`OpenSession` accepts run context, session mode, identity expectations, and the
exact allowlist. It returns a session channel and session ID.

Read-only sessions may coexist when safe. Exactly one mutating session exists
per proxy instance. Target-local sequences are serialized initially, including
read-only sequences whose interleaving would make evidence ambiguous.

The allowlist is fixed for the session's lifetime in V1. Expanding consent
requires closing the session and reopening it with the expanded allowlist; a
narrowing-only `ExtendAllowlist` operation is a deferred open decision
(section 26).

#### 11.4 MMIO operations

The initial scalar MMIO methods are:

- `Read32(resource_id, offset)`;
- `Write32(resource_id, offset, value, write_mask, precondition, readback)`;
- `Snapshot(items)`; and
- `Poll32(resource_id, offset, expected, mask, interval, timeout)`.

Additional widths require protocol and policy revision. Offsets are bytes from
the start of a logical resource.

#### 11.5 Ordered sequence

`ExecuteSequence` accepts a bounded ordered list containing:

- resource-specific scalar operations;
- delay;
- memory barrier; and
- only those protocol transactions whose exact semantics are represented.

Every entry identifies resource kind, resource ID, and operation-specific
fields. Unknown operations produce `ZX_ERR_NOT_SUPPORTED`.

The sequence is not atomic. Whole-sequence structural/policy validation occurs
before the first hardware operation. Hardware preconditions are evaluated at
execution time.

#### 11.6 Resource-specific endpoints

Where practical, a session should open a typed endpoint for a standard
protocol-backed resource. The endpoint:

- preserves generated FIDL request/response types;
- is bound to the session identity;
- enforces the same target policy and allowlist;
- writes the same audit records; and
- observes the same deadlines and cancellation.

If an exact production protocol cannot represent necessary debug policy or
audit behavior, define a narrowly shaped adapter and document the difference.

#### 11.7 Interrupt observation

`WaitForInterrupt(resource_id, after_sequence, timeout)` is a bounded hanging
request. The proxy retains and acknowledges the interrupt locally.

The report includes:

- interrupt resource ID;
- event sequence;
- total count;
- target timestamp; and
- coalesced/overflow information.

The initial protocol observes interrupts only. It does not let the host change
polarity, routing, or masking.

#### 11.8 Audit

`ReadAudit(cursor, limit)` returns a bounded page and reports the oldest
available sequence so the host can detect gaps.

#### 11.9 Versioning

- Increment minor for optional compatible fields and safely rejectable flexible
  operations.
- Increment major for changed operation meaning, policy guarantees, session
  semantics, or evidence interpretation.
- Never reuse an ordinal with different meaning.
- The host requires an exact major version.
- Golden descriptions and compatibility matrices cover every supported major.

### 12. MMIO semantics

#### 12.1 Read validation

Immediately before every read:

1. verify session and proxy generation;
2. find the resource;
3. require supported width and alignment;
4. check `offset + width` for overflow;
5. require the access within logical size;
6. require the access within actual mapped size;
7. enforce target read ceiling/hard denies;
8. match the exact session allowlist;
9. enforce access-class count/rate/deadline limits; and
10. perform one volatile read.

Rejected accesses are audited without touching hardware.

#### 12.2 Masked writes

For an ordinary 32-bit masked write:

```text
policy_mask = immutable target mask
requested   = request.write_mask

require requested != 0
require (requested & ~policy_mask) == 0

if requested covers the full register width
   and no precondition is configured or requested:
    volatile_write32(request.value)
else:
    require read-modify-write valid for this policy entry
    before = volatile_read32()
    require precondition if configured
    after = (before & ~requested) | (request.value & requested)
    volatile_write32(after)

apply required barrier
read back configured stable bits if required
```

The before-read in the read-modify-write path is itself a hardware read with
potential side effects; it is audited, and it is permitted only where the
policy entry declares read-modify-write valid. Write-only registers must be
declared full-mask, non-read-modify-write, and without preconditions.

Reject rather than silently narrow a requested mask.

#### 12.3 Polls and delays

Polls use asynchronous timers on the driver dispatcher. They never busy-wait
and never block the dispatcher with sleep.

Dispatcher timers provide millisecond-class scheduling precision. V1 makes no
sub-millisecond guarantee for delays, polls, or inter-operation gaps; whether
ceiling-bounded short spin-delays are permitted for microsecond-class
sequencing is an open decision (section 26). Declared timing precision is
part of capability reporting.

The proxy enforces:

- maximum interval frequency;
- maximum timeout;
- maximum iteration count;
- sequence wall-clock deadline; and
- cancellation on session close or stop.

#### 12.4 Barriers

Use the architecture/platform MMIO primitives from the selected Fuchsia tree.
A barrier orders accesses; it does not prove device-internal completion.

#### 12.5 Snapshot

Validate every item before the first read. The snapshot is an ordered series of
reads, not an atomic hardware snapshot. Each value includes an individual
timestamp, status, and audit sequence.

### 13. Execution and dispatcher model

Start with one synchronized dispatcher for:

- FIDL requests;
- session state;
- execution state machines;
- timers;
- interrupt callbacks;
- audit ordering; and
- driver stop.

Use an asynchronous sequence state machine:

```text
ValidateWholeSequence
    |
    v
BeginOperation
    +-- immediate operation -> Record -> next
    +-- delay -> arm timer -> resume -> Record -> next
    +-- poll -> read -> matched/timeout/arm interval
    +-- protocol wait -> async completion -> Record -> next
```

A second target-local sequence receives `ZX_ERR_SHOULD_WAIT` until safe
concurrency semantics are designed.

On failure:

- stop when `stop_on_error` is true;
- continue only for explicitly recoverable errors when it is false;
- never continue after stop, peer closure, invalid backend state, or a
  policy-classified fatal error; and
- initially require `stop_on_error = true` for every mutating sequence.

### 14. Core implementation boundaries

Use narrow testable modules:

```text
ProxyDriver
    lifecycle, outgoing service, instance identity

ResourceProvider
    node/parent-specific acquisition

HardwareBackend
    MMIO, barriers, clock, and injected protocol operations

AccessPolicy
    actual bounds + target ceiling + session allowlist

Executor
    scalar operations and asynchronous sequences

SessionServer
    run context, allowlist, leases, cancellation

ProtocolResourceAdapter
    GPIO/SPI/I2C/serial/clock/reset endpoints

InterruptTracker
    async waits, counters, acknowledgment

AuditRing
    bounded ordered target evidence
```

FIDL conversion stays at service boundaries. Policy and executor tests run
without a component realm: the pure-logic modules form the
`lab_proxy_core` crate, which builds for both the target and the build
host so their tests run without an emulator.

### 15. Driver start

`Start` must:

1. read immutable engineering configuration;
2. load/generate the target ceiling;
3. connect to the node's parent-native resource provider;
4. enumerate offered resources;
5. validate policy against actual resources;
6. map permitted MMIO locally;
7. obtain and register permitted interrupts;
8. connect permitted protocol-backed resources;
9. derive boot/proxy/resource identities and digests;
10. create FIDL binding groups;
11. publish the proxy service through the Driver Framework route expected by
    the Driver Manager control plane; and
12. emit one structured ready event.

Start performs no experimental write. Avoid reads with possible side effects.
An identity read during start is allowed only when explicitly declared safe
and audited.

Start fails if a required resource is absent, undersized, ambiguous, or
incompatible with policy.

### 16. Sessions and leases

A session owns:

- run context;
- exact allowlist;
- read-only/mutating mode;
- session ID;
- pending timers/waits;
- sequence state; and
- audit cursor metadata.

On session channel close:

- cancel timers and waits;
- stop new operations;
- release the mutation lease;
- audit session closure;
- run only explicitly configured fail-safe cleanup; and
- finish within a bounded deadline.

Cleanup is fallible and audited. A resource with unknown safe cleanup has no
automatic cleanup and relies on normal driver rebind, target reset, or
out-of-band recovery.

### 17. Interrupt handling

For every interrupt callback:

1. capture target monotonic time;
2. increment count and sequence;
3. calculate coalescing information where available;
4. satisfy eligible waiters;
5. append an event/audit entry; and
6. acknowledge the interrupt on every path.

Use virtual interrupts in tests. An interrupt storm must not allocate
unbounded memory or starve stop/restoration.

### 18. Audit ring

The fixed-capacity ring assigns monotonically increasing 64-bit sequence
numbers. Wrap overwrites old entries but does not reset sequence.

The ring is in-memory: a driver-host crash loses undrained entries. Host
workflows must therefore drain audit incrementally during mutating plans
rather than only at run end.

Each entry contains:

- target boot and proxy generation;
- session and run context;
- resource identity;
- operation index and normalized operation;
- allowlist match/grant reference when provided;
- target policy decision;
- status;
- applicable before/after/value data;
- start/completion or event timestamp; and
- cleanup/failure classification.

Never audit pointer values, physical addresses, channel handles, or unbounded
caller strings.

The driver layer mirrors each appended entry into system tracing through an
append observer, keeping the core crate free of Fuchsia dependencies. Entries
are written to the `driver_lab` trace category, or `driver_lab:irq` for
interrupt taps, and successful writes also sample a per-resource counter.
Mirroring must be a no-op unless a trace session enables the category, must
not block, and must be disabled for embedded servers in builds without
`enable_driver_lab`. Trace events are best-effort; the audit ring remains the
authoritative record.

### 19. Stop

`PrepareStop` must:

1. reject new sessions;
2. stop accepting operations;
3. cancel sequence timers and protocol waits;
4. cancel interrupt waits;
5. finish in-flight callbacks;
6. close FIDL bindings with an epitaph;
7. run only bounded configured cleanup;
8. stop interrupt handlers;
9. release mappings/resources; and
10. complete stop within the Driver Manager stop deadline.

Every pending call terminates with a result, epitaph, cancellation, or bounded
host timeout.

### 20. Error model

Use typed application errors and Zircon status consistently:

- invalid/misaligned input;
- out of resource bounds;
- target ceiling denial;
- session allowlist denial;
- stale boot/proxy/resource identity;
- failed precondition;
- unsupported resource/method;
- timeout;
- cancellation;
- mutation lease contention;
- partial execution; and
- fatal backend state.

Machine-parsed information must not exist only in a human detail string.

Execution reports contain:

- sequence/batch ID;
- first and last audit sequence;
- result for every operation that began;
- stopped-on-error flag;
- partial-completion classification;
- fatal/nonfatal classification; and
- proxy health after execution.

### 21. Source layout

Exact paths may follow repository conventions:

```text
src/devices/driver-lab/
    BUILD.bazel
    BUILD.gn
    meta/
    fidl/
        BUILD.bazel
        BUILD.gn
        driver_lab.fidl          fuchsia.driver.lab wire contract; moves to
                                 an internal-category SDK library once the
                                 contract stabilizes
    src/
        lib.rs                   driver lifecycle and service publication
        driver_lab_lib.rs        embedded Rust library crate root
        embedded.rs              EmbeddedLabServer and DriverLabBuilder
        c_api.rs                 C FFI staticlib shim (driver_lab_c)
        server.rs                FIDL session server
        trace.rs                 audit-to-trace mirroring (driver_lab categories)
        core.rs                  host-testable core crate root
        access_policy.rs
        audit_ring.rs
        digest.rs
        executor.rs
        hardware_backend.rs
        session.rs
        resource_provider.rs           planned: parent adapters (P0/P1)
        protocol_resource_adapter.rs   planned (P4)
        interrupt_tracker.rs           planned (P5)
    cpp/
        include/lib/driver_lab/
            driver_lab.h         idiomatic C++20 wrapper and StateVmoBank
            driver_lab_c.h       pure C ABI declarations and global state helpers
        driver_lab.cc            C++20 wrapper and StateVmoBank implementation
        tests/
            driver_lab_cpp_test.cc   C++ embedded server and StateVmoBank unit tests
    policy/                            planned: schema and generation
    bind/

src/devices/bin/driver_manager/
    ... engineering takeover integration (phase 2) ...
```

The proxy is implemented in Rust on the DFv2 Rust driver bindings. Where the
Rust driver-framework, MMIO, or interrupt bindings lag their C++ equivalents
in the selected revision, the gap is a feasibility-spike finding to resolve
upstream, not a reason to switch languages. The wire contract and host
programming model remain language-neutral.

### 22. Build and configuration

Create separate targets for:

- FIDL libraries and generated bindings;
- policy schema/generator;
- pure policy/executor library;
- proxy driver/component/package;
- unit and integration tests;
- Driver Manager takeover integration (phase 2);
- host bindings and compatibility goldens;
- engineering assembly inclusion; and
- production-absence verification.

Structured configuration may reduce:

- enabled state;
- audit capacity;
- log verbosity;
- maximum operation count;
- maximum deadlines; and
- automatic restoration timeout (phase 2).

It may not widen resource or write policy.

### 23. Testing

#### 23.1 Policy tests

Cover:

- known/unknown resources;
- logical versus actual sizes;
- digest determinism;
- hard-denied reads;
- unknown read with and without exact allowlist;
- width/alignment;
- overflow;
- one-shot versus poll permission;
- target limit narrowing;
- read-only versus mutating session;
- exact write offset/mask;
- precondition/readback requirements;
- protocol method and transaction limits; and
- cleanup validation.

#### 23.2 Executor tests

Use fake MMIO, fake clocks, fake timers, and fake protocol resources. Cover:

- scalar read;
- snapshot prevalidation;
- masked write calculation;
- rejected mask performs no write;
- failed precondition;
- readback success/mismatch;
- immediate and delayed poll success;
- poll timeout;
- barriers and ordering;
- protocol transaction;
- stop-on-error partial result;
- cancellation;
- audit/result correspondence; and
- concurrent sequence rejection.

#### 23.3 Driver realm tests

Provide fake parent resources and test:

- platform-like and PCI-like providers;
- resource enumeration;
- missing/undersized resources;
- mapping and device cache policy;
- service publication;
- session opening;
- virtual interrupts;
- driver stop;
- pending request cancellation; and
- no resource handles returned to the host.

#### 23.4 Activation integration tests

With a synthetic unclaimed node:

1. register and bind the proxy through existing mechanisms;
2. verify identity, generation, and digest reporting;
3. execute a read-only plan;
4. stop the proxy; and
5. verify the node is again unclaimed.

Inject failure at every transition. Managed-takeover integration tests are
specified in phase 2.

#### 23.5 Interrupt tests

Verify count/sequence, retained events, future waits, timeout, coalescing,
multiple observers, close cancellation, stop cancellation, acknowledgment,
storm bounding, and stop/teardown responsiveness.

#### 23.6 Security-negative tests

Verify rejection of:

- physical address input;
- unknown resource;
- resource size boundary/overflow;
- unsupported width;
- hard-denied read;
- read absent from session allowlist;
- one-shot grant used for poll;
- write absent from immutable target policy;
- excessive write mask;
- missing precondition;
- oversized operation vectors or transactions;
- excessive timeout/delay;
- mutation from read-only session;
- second mutating session;
- stale identity/digest;
- arbitrary driver URL or protocol path;
- concurrent ownership with normal driver; and
- proxy/control artifacts in a production image.

#### 23.7 Fault injection

Cover:

| Fault | Required result |
|---|---|
| Peer closes before operation | no access; transport failure |
| Peer closes after write | partial/unknown result remains visible |
| Stop during delay/poll | bounded cancellation |
| Driver host crashes | proxy generation changes; session becomes stale |
| Target reboot | boot identity changes; no replay |
| Audit wrap | explicit gap reported |
| Interrupt storm | bounded memory and responsive stop |
| Protocol backend hangs | target/host deadline and recovery path |
| Cleanup fails | audited failure; no success claim |

### 24. Milestones

#### Milestone P0: feasibility

- validate proxy bind to an unclaimed node via existing registration and bind
  mechanisms;
- validate Rust DFv2 driver support for MMIO mapping, interrupt handling,
  synchronized-dispatcher timers, and service publication;
- classify candidate parent resource providers as channel-transport or
  driver-transport-only in the selected revision;
- validate the first parent resource provider;
- validate isolated host placement;
- define the engineering/production assembly boundary with platform assembly
  owners; and
- (phase-2 gate) review node-scoped force-bind, node persistence across
  normal-driver unbind, and restoration with the driver-framework team.

#### Milestone P1: read-only proxy

- `Describe`;
- immutable ceiling;
- session allowlist;
- named MMIO mapping;
- `Read32` and snapshot;
- audit; and
- driver realm tests.

Exit: only an exact allowed read inside an offered resource reaches fake MMIO.

#### Milestone P2: activation integration

- proxy registration, bind, and service routing on an unclaimed synthetic
  node;
- generation identity;
- bounded stop; and
- verified teardown back to an unclaimed node.

Exit: synthetic activation and teardown succeed and every injected transition
failure is visible. (Managed-takeover integration is the corresponding
phase 2 milestone.)

#### Milestone P3: bounded sequence and mutation

- delay, poll, and barrier;
- exact masked writes;
- precondition/readback;
- mutation lease;
- cleanup semantics;
- partial completion; and
- expanded fault tests.

Exit: no out-of-policy bit can be written and no failed sequence is called
atomic.

#### Milestone P4: protocol resources

- representative GPIO and SPI or I2C adapters;
- production-shaped typed endpoints;
- method-specific policy and audit;
- heterogeneous target-local sequence; and
- translation-fidelity documentation.

Exit: host Python can perform equivalent resource operations through direct and
proxy modes without flattening their semantics.

#### Milestone P5: interrupts and physical validation

- interrupt observation;
- virtual tests;
- selected hardware-safe read validation;
- selected reviewed mutation;
- driver-host failure drill; and
- out-of-band recovery drill.

Exit: hardware runs preserve evidence and return the target to a known
operational state.

### 25. Definition of done

The phase 1 proxy is ready when:

- production images verifiably do not offer it;
- it is present but inactive in selected engineering images;
- it binds without an arbitrary host-supplied driver URL;
- it binds only to nodes with no bound driver;
- it receives only resources offered to its node;
- no host API accepts a physical address or raw target resource handle;
- actual bounds, target ceiling, and session allowlist are all enforced;
- unknown reads require an exact session grant;
- writes require immutable exact target policy;
- operations and waits are bounded;
- target-local execution never blocks the dispatcher;
- partial completion is represented honestly;
- audit covers attempted and rejected operations;
- driver stop cancels all pending work;
- teardown verifiably returns the node to an unclaimed state;
- synthetic, security-negative, fault-injection, and selected physical tests
  pass;
- the wire contract remains usable from the companion host tooling
  (`//tools/driver-lab/SPEC.md`); and
- the wire contract carries the reserved takeover fields so phase 2 requires
  no major version change.

### 26. Open decisions

Before implementation, resolve:

1. The first parent resource-provider adapter.
2. How generic engineering read ceilings are supplied without unsafe global
   defaults.
3. Which offsets/ranges must be hard-denied even with operator consent.
4. The initial write-policy authoring and review process.
5. Whether standard FIDL protocols can be served unchanged while preserving
   session policy/audit.
6. The Rust driver-framework, MMIO, interrupt, and timer APIs available in
   the selected Fuchsia revision, and any binding gaps requiring upstream
   work.
7. Whether a narrowing-only `ExtendAllowlist` session operation replaces V1's
   reopen-per-grant behavior.
8. Whether ceiling-bounded short spin-delays are permitted for
   microsecond-class sequencing.

Takeover-related decisions (force-bind and restore APIs, node-class
persistence, restoration-state persistence, driver-host restart during
takeover, subtree teardown) live in the Phase 2 section of this document.

### 27. Primary references

- [Driver communication and services](https://fuchsia.dev/fuchsia-src/concepts/drivers/driver_communication)
- [Mapping device memory in a driver](https://fuchsia.dev/fuchsia-src/concepts/drivers/mapping-a-devices-memory-in-a-driver)
- [Using hardware resources in a driver](https://fuchsia.dev/fuchsia-src/development/drivers/tutorials/sdk_build_driver/hardware-resources)
- [Handling interrupts in a driver](https://fuchsia.dev/fuchsia-src/development/drivers/developer_guide/handle-interrupts-in-a-driver)
- [Driver runner and driver hosts](https://fuchsia.dev/fuchsia-src/concepts/components/v2/driver_runner)
- [Fuchsia registers driver](https://fuchsia.dev/fuchsia-src/development/drivers/driver_guides/registers/overview)
- [Structured configuration](https://fuchsia.dev/fuchsia-src/development/components/configuration/structured_config)
- [Build configuration and product assembly](https://fuchsia.dev/fuchsia-src/development/build/software_assembly/build_configuration)

## Phase 2: embedded driver library & in-situ debugging

### Implementation status

Changesets continue the global numbering from Phase 1:

- [x] CS24 `[driver-lab] Core library extraction & pre-mapped MMIO adapter`
- [x] CS25 `[driver-lab] Embedded Rust driver library with ServiceFs integration`
- [x] CS26 `[driver-lab] Cooperative locking, quiesce hooks, and interrupt tap support`
- [x] CS27 `[driver-lab] Assembly gating, CML debug shard, and production-absence verification`
- [x] CS28 `[driver-lab] Reference integration: synthetic platform driver in testing/`
- [x] CS29 `[driver-lab] Host tooling discovery of embedded debug endpoints and in-situ session workflow`
      (in //tools/driver-lab/SPEC.md).
- [x] CS30 `[driver-lab] In-situ live-target conformance test suite`
      (in //tools/driver-lab/SPEC.md).
- [x] CS34 `[driver-lab] Expose FIDL and debug shard to Bazel`
- [x] CS35 `[driver-lab] Native Bazel build for driver_lab_rust`
- [x] CS36 `[driver-lab] Add C FFI staticlib shim (driver_lab_c)`
- [x] CS37 `[driver-lab] Add C/C++ wrapper and StateVmoBank`
- [x] CS38 `[driver-lab] Add C++ embedded server unit tests`

Phase: 2 -- existing-driver in-situ inspection and live debugging via embedded library.
Extends: Phase 1 of this document. The wire contract (`fuchsia.driver.lab`), policy
models, audit ring, and bounded executor apply unchanged.

Companion specification: host tooling at `//tools/driver-lab/SPEC.md`.

### 1. Purpose

Phase 2 enables empirical hardware exploration and live debugging of existing,
bound drivers without stopping them or tearing down their child subtrees.

A driver incorporates the `driver_lab` library during compilation (in engineering
and debug builds), shares its mapped hardware resources with the embedded server,
and serves `fuchsia.driver.lab.Service` from its outgoing directory. Host tooling
connects directly to the driver's moniker, enabling register inspection, bounded
probing, and timing-sensitive sequences while the driver remains alive and active.

### 2. Architecture & library model

#### 2.1 Capability sharing via VMO duplication

The embedded library does not request root capabilities or acquire new resources
from the parent node:
- The driver duplicates its existing MMIO VMO handle (`zx::Rights::SAME_RIGHTS`).
- The library creates an independent local mapping (`MappedMmio`) within the same
  driver host process address space.
- The driver's internal data structures, register types, and private memory
  remain unperturbed.

#### 2.2 Concurrency and cooperative interlocking

Because both the driver's normal dispatch loops and the host's probe sequences
have access to hardware registers, unsynchronized mutations must be prevented:
- **Read operations**: Scalar reads and snapshots to known read-safe registers
  are permitted concurrently. Destructive registers (clear-on-read interrupt
  status, FIFO data registers) must be marked `hard_denied` in the target
  ceiling manifest unless explicitly authorized by an exact allowlist.
- **Mutating sequences and quiescing**: Opening a mutating session acquires an
  exclusive mutation lease. The library invokes an optional driver-registered
  synchronous `quiesce_hook(bool paused)` callback, allowing the driver's state
  machine or worker threads to pause background register polling while host
  mutations and sequences execute. If complex driver dispatchers require
  asynchronous coordination in the future, this interface may be extended to
  support an async stream/channel notification.

#### 2.3 Interrupt observation via event tapping

In Zircon, a `zx::interrupt` object can only be bound to a single dispatcher or
thread. The library therefore does not attempt to steal or bind the raw interrupt
handle.
- The driver instruments its existing interrupt service routine to ping the
  library: `lab.notify_interrupt(resource_id)`.
- The library increments the sequence counter, appends to the audit ring, and
  satisfies pending host `WaitForInterrupt` hanging gets.

### 3. Language roadmap

#### 3.1 Phase 2a: Rust driver library (`driver_lab_rust`)

Prioritized for immediate support across Fuchsia's DFv2 Rust driver stack in
both GN and Bazel builds.
- Links directly against `lab_proxy_core`.
- Native integration with `fdf_component::DriverContext`, `ServiceFs`, and
  `fuchsia_async::Scope` (with GN-only driver crates feature-gated under
  `fdf_driver` and a pure-`zx` root VMAR mapping fallback for native Bazel
  compilation).
- First reference integration: synthetic platform driver in `testing/` (CS28).

#### 3.2 Phase 2b: C/C++ embedded driver library (`driver_lab_c` and `driver_lab_cpp`)

Supports DFv2 C++ and hybrid C/C++ drivers across both GN and Bazel builds
by wrapping `driver_lab_rust` in a thin `extern "C"` FFI static library
(`driver_lab_c`, declared in `cpp/include/lib/driver_lab/driver_lab_c.h`) so all
SHA-256 resource/policy digest calculation, session allowlist enforcement, audit
ring buffering, and sequence execution remain single-sourced in Rust:
- `driver_lab_c` exposes opaque `DriverLabBuilderHandle` and
  `DriverLabServerHandle` pointers over a pure C ABI, runs a dedicated
  background `fuchsia_async::LocalExecutor` thread to serve incoming
  `fuchsia.driver.lab.Proxy` channels without blocking the C++ `fdf::Dispatcher`,
  and provides process-wide global state/knob helpers
  (`driver_lab_global_set_state_u32` and `driver_lab_global_get_knob_u32`) for
  legacy `.c` files.
- `driver_lab_cpp` (`cpp/include/lib/driver_lab/driver_lab.h` and
  `cpp/driver_lab.cc`) provides RAII C++20 wrappers in `namespace driver_lab`:
  `StateVmoBank` (shared-memory VMO bank for telemetry/state and host-tunable
  fault-injection knobs), `Builder`, and `EmbeddedServer` (integrating with
  `fdf::OutgoingDirectory` and `component::OutgoingDirectory`).
- Unit tests (`driver_lab_cpp_tests` in `cpp/tests/driver_lab_cpp_test.cc`)
  verify `Describe`, mutating `OpenSession`, `Read32`, `Write32` on
  `StateVmoBank` knobs, global C state/knob visibility, interrupt tapping,
  quiesce hooks, `ReadAudit`, and hard-denied range rejection over FIDL.

### 4. Production absence and security gating

The embedded debug library must never be active in production (`user`) builds:
1. **Compilation gating**: Gated by build argument (`enable_driver_lab = is_debug`).
   In release builds, library types compile to zero-sized no-ops and outgoing
   service handlers are omitted.
2. **Component manifest sharding**: Driver manifests import a common shard
   `//src/devices/driver-lab/meta/debug.shard.cml` (exported to both GN and
   Bazel targets as `//src/devices/driver-lab:meta/debug.shard.cml`) that
   exposes `fuchsia.driver.lab.Service`.
3. **Product assembly verification**: Platform assembly golden checks verify that
   no production package or bootfs driver exposes `fuchsia.driver.lab.Service`.

### 5. Open design decisions resolved

1. *Takeover unbind/force-bind APIs*: Obsoleted. No Driver Manager changes required.
2. *Subtree teardown and power framework*: Obsoleted. Drivers remain bound;
   subtrees and power topologies remain undisturbed.
3. *ARM64 clock/power gating panics*: Resolved. The running driver maintains its
   votes for power domains and clocks throughout the debug session.

## Phase 3: software state banks, runtime knobs, and diagnostic triggers

### Implementation status

Changesets continue the global numbering from Phase 2:

- [x] CS31 `[driver-lab] Software state banks, runtime knobs, and diagnostic triggers`
- [x] CS32 `[driver-lab] Host plan aliases and StateHandle API for state and knob experiments`
      (in //tools/driver-lab/SPEC.md).
- [x] CS33 `[driver-lab] Update driver-lab and autoda-fix skills for Mode A/B/C verification`
      (in //tools/driver-lab/SPEC.md).

Phase: 3 -- single-build in-situ software state observation, runtime knobs, and
diagnostic triggers.
Extends: Phases 1 and 2 of this document. `StateBank` implements `MmioBackend`
and `ResourceBackend`, requiring no breaking changes to `fuchsia.driver.lab`.

Companion specification: host tooling at `//tools/driver-lab/SPEC.md`.

### 1. Purpose

Phase 3 extends embedded in-situ debugging (`driver_lab_rust::embedded`) beyond
physical MMIO registers so developers and agents can reproduce, diagnose,
prototype fixes for, and verify software state-machine and concurrency bugs
without repeated `edit -> build -> flash/OTA -> reboot` cycles.

### 2. `StateBank` architecture and primitives

`lab_proxy_core::state_bank::StateBank` exposes a 32-bit word-addressed virtual
resource backed by 4-byte aligned offsets within a declared `logical_size`:

1. **Observable state slots (`define_state_slot(offset, initial) -> StateSlotHandle`)**:
   - Backed by `Arc<AtomicU32>` (`StateEntry::AtomicSlot`).
   - Read-only to the host by default (`offset` is omitted from
     `writable_offsets`).
   - The driver updates and reads the slot with zero lock contention via
     `StateSlotHandle` (`load`, `store`, `fetch_add`, `fetch_or`, `fetch_and`).
2. **Runtime knobs (`define_knob(offset, initial) -> StateSlotHandle`)**:
   - Backed by `Arc<AtomicU32>` (`StateEntry::AtomicSlot`) with `offset` added
     to `writable_offsets`.
   - Enables fault/race-window injection knobs (e.g., bounded microsecond delay
     at a suspect critical section) and candidate fix-toggle knobs (e.g.,
     enabling a lock or teardown guard at runtime to prove reproduction with
     `knob = 0` and resolution with `knob = 1` in the same boot).
3. **On-demand read probes (`define_read_probe(offset, probe)`)**:
   - Backed by `Arc<dyn Fn() -> Result<u32, BackendError> + Send + Sync>`
     (`StateEntry::ReadProbe`), evaluated synchronously when `read32` is called.
   - Read-only; host writes are rejected.
4. **Diagnostic triggers (`define_trigger(offset, trigger)`)**:
   - Backed by `StateEntry::Trigger { last_result: Arc<AtomicU32>, callback }`
     with `offset` added to `writable_offsets`.
   - Invokes `callback(value)` on `write32(offset, value)` under a mutating
     session lease, stores the returned `u32` status into `last_result` for
     immediate `readback` or subsequent `read32`/`poll32`, and returns
     `Err(BackendError::Fault)` if the callback fails.

### 3. Policy enforcement and wire contract stability

- `StateBank` implements `MmioBackend` (`read32`, `write32`, and `barrier` via
  `std::sync::atomic::fence(Ordering::SeqCst)`) and `ResourceBackend`.
- `LiveBackend::State(StateBank)` integrates with `DriverLabBuilder::with_state_bank(name, bank)`,
  which registers the bank in `ProvidedResources` with a `ResourceCeiling`
  populated from `bank.writable_offsets()`.
- All existing target policy invariants (`writable_registers`, `hard_denied`,
  session allowlist narrowing, mutating session lease, `quiesce_hook`, and
  `target-audit.jsonl` recording) apply automatically without changing
  `driver_lab.fidl`.

### 4. Reference sample driver integration

`lab_sample_driver` (`//src/devices/driver-lab/testing/src/sample_driver.rs`)
registers a `"state0"` `StateBank` alongside `"mmio0"` and `"irq0"`, exposing
`STATE_BUSY_FLAG` (`0x00`), `STATE_INVARIANT_VIOLATIONS` (`0x04`),
`KNOB_RACE_DELAY_US` (`0x08`), `KNOB_FIX_ENABLED` (`0x0C`), and
`TRIGGER_CONCURRENT_OP` (`0x10`), verified in `sample_driver_realm_test`.

