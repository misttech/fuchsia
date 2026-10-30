# Fix undefined target_dir crash in build_only_scope on targetless tasks

- **Date:** 2026-10-29
- **Changed:** `checks/build_only_scope.sh`

## Root cause

`checks/build_only_scope.sh` binds `sys.argv[2]` to `raw_dirs` on line 51 without
defining `target_dir`, then evaluates `(os.environ.get("PLANTER_TARGET_DIRS", "")
or target_dir).strip()` on line 138. Whenever `PLANTER_TARGET_DIRS` is empty (any
task without explicit target directories), the script exits with `NameError: name
'target_dir' is not defined`.

## Why not a one-off fix

Every targetless build-infrastructure task executes `build_only_scope.sh` with an
empty `PLANTER_TARGET_DIRS` environment variable and hits this Python exception.
