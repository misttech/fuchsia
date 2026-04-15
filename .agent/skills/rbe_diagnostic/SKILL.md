---
name: rbe_diagnostic
description: Diagnose remote build (RBE) issues, such as remote-only failures, remote vs. local artifact differences.
---

# RBE Diagnostic

## When to use this skill

Use this skill when a user reports a build failure related to Remote Build Execution (RBE). This includes:
*   "File not found" errors during remote compilation.
*   Unexpected remote cache hits or misses.
*   Suspected non-determinism (e.g., identical commands producing different results).
*   State leaks across parallel or sequential actions in the same reproxy session.
*   Local vs. remote build yield different artifacts.

## Persona

Assume the role of a specialized build engineer. You are methodical and data-driven, relying on reproxy logs, action digests, and remote input root inspections to pinpoint the exact failure mechanism in the RBE infrastructure or wrapper scripts.

## Diagnostics Workflow

### 1. Locate Reproxy Logs

Reproxy logs are essential for understanding what happened during a build.
*   Check `out/_build_logs/<config>/build.<timestamp>.<id>/reproxy_logs/`.
    *   `<config>` is your build directory's basename, which can be found by `fx use` or looking at `.fx-build-dir`.
    *   `<id>` is a random suffix.
    *   Find the most recent `build.*` directory that corresponds to the failed build.
*   Look for `reproxy.INFO`, `scandeps_server.INFO`, and `.rrpl` (reclient reproxy proto log) files.
*   If `FX_BUILD_RBE_STATS=1` was set, the log directory is printed in the build summary. It is generally useful to set this environment variable while re-building and debugging.

### 2. Fetching Infra Build RBE Logs

Infra builds in buildbucket are identified by a number, e.g. go/bbid/NUMBER.
If the failure occurred in an infrastructure build, you can fetch the RBE logs (specifically the `.rrpl` files) to a temporary directory using:

```bash
./build/rbe/bb_fetch_rbe_cas.sh --verbose --bbid <BBID>
```

*   `<BBID>` is the Buildbucket ID of the failed infra build ('b' prefix is ok).
*   **Prerequisites**: Requires the `bb` (Buildbucket) and `cas` tools (found under `prebuilt/tools/buildbucket/` and `prebuilt/tools/cas/`). If authentication is needed, ask the user to run `bb auth-login` and `cas login` as agents cannot perform interactive authentication.

### 3. Analyze Action and Command Digests

The `.rrpl` logs contain remote action details.
*   **CommandDigest**: Represents the command line and environment. If two actions have the same CommandDigest, they are considered "the same command".
*   **ActionDigest**: Represents the full execution context, including the Input Root (all files uploaded).
*   **Comparison**: If two actions have the same CommandDigest but unexpectedly different ActionDigests, it means their input roots differ (e.g., different headers, different toolchain versions, or different environment variables). Investigate the Input Root differences to find the cause of cache misses or collisions.

### 4. Inspect Remote Input Roots

Use `remotetool.sh` to see exactly what files were sent to the remote worker.

```bash
./build/rbe/remotetool.sh --operation show_action --digest <ActionDigest>
```

*   **Prerequisites**: If authentication is needed, ask the user to run `gcloud auth application-default login`.
*   **Canonicalization**: Pay close attention to paths under `set_by_reclient/a/` (if canonicalization is enabled, which is the default for C++). This maps absolute paths to a generic working directory.
*   Compare the input roots of near-identical actions to find differences.
    These differences are crucial to investigation.

### 5. Reproduction Techniques

*   **Build action repro**: `fx build -- -v -n <target>` is one way to get the command for a given ninja target, which may include the remote action wrappers.
    *   For ninja actions that write long command-line arguments to response files (`.rsp`), also pass `-d keeprsp` to prevent ninja from deleting them. You can find the `.rsp` file paths in the ninja command output.
*   **Reproxy management**: `./build/rbe/fuchsia-reproxy-wrap.sh -- <your_command>` starts and stops the `reproxy` tool around any command, which is useful for reproducing single build actions. Note that manual invocations of this script will produce logs in `out/_unknown/.reproxy_logs/`.
*   **Sequential Repro**: If you suspect order-dependent behavior or race-conditions, try to reduce the scenario to a small number of sequential build actions and permute their order. One way to experiment with action ordering is to write them to a temporary script, and run them in a single `reproxy` session using `./build/rbe/fuchsia-reproxy-wrap.sh -- bash <your_script>.sh`.
*   **Fresh State**: Attempt reproduction with a clean disk cache (e.g., by changing the log directory or temporary RBE environment).

### 6. Common Root Causes to Check

*   **Missing Remote Inputs**: Calculation of remote inputs is different for each language/tool.
    *   C++: Uses a lightweight `build/rbe/reclient_cxx.sh` (mostly forward directly to `rewrapper`, using its built-in InputProcessor), unless additional features are needed from `cxx_remote_wrapper.py`.
    *   linking: Uses `build/rbe/cxx_link_remote_wrapper.py`.
    *   Rust: Uses `build/rbe/rustc_remote_wrapper.py`.
*   **Workaround**: Use `--remote-inputs` in GN to force-upload specific files as remote inputs if the scanner fails to detect them.

## Reporting

When diagnosing an issue, always provide:
1.  The specific **Action Digest** and **Command Digest**.
2.  The relevant snippets from `remotetool` showing the path mismatch or missing file.
3.  A deterministic reproduction script if possible.
