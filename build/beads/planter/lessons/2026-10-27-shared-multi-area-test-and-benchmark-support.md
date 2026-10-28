# Shared multi area test and benchmark support visibility scoping

- **Learned from:** cq failure on CL Ia6f0b4b605689c56c510536d8d23070166b12a63, patchset 1 (task `src-performance-lib-fuchsiaperf`)
- **Date:** 2026-10-27
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

find_rdeps.sh and visibility_audit.sh only classified packages with 'testing/'
path segments or explicit testonly = True attributes as shared test-support
libraries eligible for ['//:__subpackages__'] visibility, missing non-testonly
benchmark and test-runner support libraries consumed exclusively by test,
benchmark, stress-test, and test-runner packages across multiple subsystem
areas. In addition, visibility_audit.sh did not flag narrowing an unrestricted
pre-migration GN shared multi-area test/benchmark library to <= 15 leaf caller
packages, and sub-repository worktrees whose local HEAD lags JIRI_HEAD hid newly
added cross-tree benchmark callers from static rdep discovery.

## Why not a one-off fix

Hardcoding visibility for a single benchmark library directory would leave all
other shared benchmark, stress-test, and test-runner support libraries
vulnerable to over-narrow leaf package visibility lists that break cross-tree
and out-of-graph benchmark callers in CQ with 'Dependency not allowed'.
Identifying shared test/benchmark support targets by their multi-area
test/benchmark caller profile and enforcing ['//:__subpackages__'] when
pre-migration GN visibility was unrestricted prevents this class of CQ failure
across the entire repository.
