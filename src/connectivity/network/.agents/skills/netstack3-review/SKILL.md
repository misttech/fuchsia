---
name: netstack3-review
description: >
  Code review rubric for Netstack3 and related Fuchsia networking code. Use
  when reviewing changes under src/connectivity/network/ or src/lib/network/.
---

# Netstack3 Code Review

Review changes to Netstack3 and related networking code as an expert on the
Fuchsia networking stack. Ensure that the code is high-quality, performant,
and idiomatic.

This skill is the single source of truth for Netstack3 AI code review. It is
used by:

-   Local `/review`, via `src/connectivity/network/AGENTIC_REVIEW` and
    `src/lib/network/AGENTIC_REVIEW`.
-   The Gerrit CL review agent, via the `fuchsia-netstack3-review` entry in
    `.agents/code-review/agent_configs.textproto`.

When changing which paths this skill applies to, update both configurations.

## Scope of review

Prioritize:

-   **Netstack3**: `src/connectivity/network/netstack3/...`
-   **Shared networking libraries**: `src/connectivity/network/lib/...` and
    `src/lib/network/...`
-   **Networking tests**: `src/connectivity/network/tests/...`

## Analysis process

1.  **Analyze**: Check **only added or modified lines** of the diff against
    the Netstack3 concepts and documentation below, focusing on performance,
    modularity, and idiom.
2.  **Verify**: Before reporting, read the surrounding code and double-check
    that the change actually violates a specific Netstack design pattern or
    idiom.
3.  **Report**: Report each verified issue as a separate finding.
4.  **Finish**: If no issues are found after checking the entire change,
    report exactly: "No Netstack3 issues found."

## Reporting guidelines

1.  **One issue, one comment**: Each finding must target a single, atomic
    issue.
2.  **Always cite the reference**: Each finding **must** include a link or
    clear reference to the specific section of the Netstack documentation that
    defines the rule or pattern, using the format:
    `Issue details. (Source: [Page Title](URL))`
3.  **Explain briefly**: Briefly explain the deviation and suggest a specific
    correction.
4.  **Be deterministic, conservative, and accurate**:
    -   **Line precision**: Use the exact line number where the issue occurs.
    -   **No duplicates**: Do not report multiple findings for the same rule in
        the same file.

## Key Netstack3 concepts

-   **Core vs. Bindings separation**: Core must be free of Fuchsia-specific
    dependencies. See
    [CORE_BINDINGS.md](../../../netstack3/docs/CORE_BINDINGS.md).
-   **Avoid deadlocks**: Code that performs locking must take care to avoid
    deadlocks. In Netstack3 this is generally handled by the lock ordering
    paradigm. See the "Lock ordering" section of
    [TENETS_AND_DESIGN_DECISIONS.md](../../../netstack3/docs/TENETS_AND_DESIGN_DECISIONS.md).
-   **Avoid panics**: Non-test code must use appropriate error handling
    paradigms to avoid panicking unless the error is truly unrecoverable.

## Reference documentation

Use the following documentation to ground findings:

-   Netstack3 design docs in `src/connectivity/network/netstack3/docs/`:
    [CONTROL_FLOW.md](../../../netstack3/docs/CONTROL_FLOW.md),
    [CORE_BINDINGS.md](../../../netstack3/docs/CORE_BINDINGS.md),
    [DEMULTIPLEXING_PRINCIPLE.md](../../../netstack3/docs/DEMULTIPLEXING_PRINCIPLE.md),
    [DETAILS.md](../../../netstack3/docs/DETAILS.md),
    [DUAL_STACK_SOCKETS.md](../../../netstack3/docs/DUAL_STACK_SOCKETS.md),
    [HOST_MODEL.md](../../../netstack3/docs/HOST_MODEL.md),
    [IMPROVEMENTS.md](../../../netstack3/docs/IMPROVEMENTS.md),
    [IP_TYPES.md](../../../netstack3/docs/IP_TYPES.md),
    [PARSING_SERIALIZATION.md](../../../netstack3/docs/PARSING_SERIALIZATION.md),
    [POSIX_COMPATIBILITY.md](../../../netstack3/docs/POSIX_COMPATIBILITY.md),
    [PUB_CRATE.md](../../../netstack3/docs/PUB_CRATE.md),
    [STATIC_TYPING.md](../../../netstack3/docs/STATIC_TYPING.md), and
    [TENETS_AND_DESIGN_DECISIONS.md](../../../netstack3/docs/TENETS_AND_DESIGN_DECISIONS.md).
-   Contributor guides in `docs/contribute/contributing-to-netstack/`:
    [README.md](../../../../../../docs/contribute/contributing-to-netstack/README.md),
    [netstack3.md](../../../../../../docs/contribute/contributing-to-netstack/netstack3.md),
    and
    [rust-patterns.md](../../../../../../docs/contribute/contributing-to-netstack/rust-patterns.md).

## URL mapping for citations

When citing documentation, map in-tree paths to public URLs, for example:

-   `docs/contribute/contributing-to-netstack/rust-patterns.md` maps to
    `https://fuchsia.dev/fuchsia-src/contribute/contributing-to-netstack/rust-patterns`
-   `src/connectivity/network/netstack3/docs/CONTROL_FLOW.md` maps to
    `https://cs.opensource.google/fuchsia/fuchsia/+/main:src/connectivity/network/netstack3/docs/CONTROL_FLOW.md`
