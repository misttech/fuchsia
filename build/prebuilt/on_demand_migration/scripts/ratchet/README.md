# Direct Prebuilt Reference Ratchet (`ratchet.py`)

`ratchet.py` implements the static presubmit check
(`check_no_new_direct_prebuilts` registered in `//scripts/shac/prebuilts.star`)
preventing new raw `//prebuilt` accesses from being added outside approved
on-demand fetching mechanisms during the lazy CIPD migration (`b/566304584`,
`b/567197573`).

## Architecture & Language Families (`languages.py`)

`ratchet.py` delegates per-language file matching, syntax scanning, and
remediation suffixes to `LANGUAGE_FAMILIES` in `languages.py`. Across checked
source, build, and script files, it detects unmigrated `//prebuilt/...`,
relative `prebuilt/...`, and `${PREBUILT_...}` accesses while stripping
language-specific comments and allowing:

* **GN (`*.gn`, `*.gni`):** Targets declared inside `cipd_prebuilt(...) { ... }`
  blocks and staged downstream consumer paths starting with
  `${cipd_prebuilt_root}/prebuilt/...`.
* **Bazel (`BUILD.bazel`, `*.bzl`):** Targets declared inside
  `cipd_prebuilt_file(...)` or `cipd_prebuilt_directory(...)` rule invocations.
* **Devshell scripts (`tools/devshell/**`, `scripts/devshell/**` shell scripts
  and `.fx` files):** Structured header comments (`#### EXECUTABLE=...` and
  `#### PREBUILTS=...`).
* **Shell scripts (`*.sh`, `*.bash`, and extensionless scripts):** Ignores `#`
  comments and staged `${cipd_prebuilt_root}/prebuilt/...` paths.
* **Hash-comment languages (`*.py`, `*.star`, `*.cmake`, `CMakeLists.txt`):**
  Strips trailing `#` comments outside string literals.
* **C-family languages (`*.c`, `*.cc`, `*.cpp`, `*.h`, `*.hpp`, `*.rs`, `*.go`,
  `*.dart`):** Strips `//` and `/* ... */` comments outside string literals
  (including `#include "prebuilt/..."`, Rust
  `include_bytes!(".../prebuilt/...")`, and Go prebuilt paths).

Each file's observed match count is compared against the repository's
checked-in baseline allowlist
(`build/prebuilt/on_demand_migration/shac/direct_prebuilt_allowlist.json`). Any
file exceeding its recorded count produces an actionable remediation error
tailored to that file's language family.

## Usage

### Regenerate Allowlists (`fuchsia.git` and `vendor/google`)

After migrating targets or removing legacy direct references, update the
checked-in baseline allowlists across both repositories from the Fuchsia
checkout root:

```bash
./build/prebuilt/on_demand_migration/scripts/ratchet/ratchet.py \
  --update-allowlists
```

### Inspect Specific Files Manually

```bash
./build/prebuilt/on_demand_migration/scripts/ratchet/ratchet.py src/foo/BUILD.gn
```

### Run Unit Tests & SHAC Checks

```bash
fx build //build/prebuilt:tests
prebuilt/tools/shac/shac test scripts/shac/tests/prebuilts_test.star
fx host-tool shac check --only check_no_new_direct_prebuilts --all
```
