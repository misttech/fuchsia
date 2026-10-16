#!/usr/bin/env fuchsia-vendored-python

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Verifies GN and Bazel build commands across multiple Fuchsia build configurations.

Each build configuration is generated from a fint_params .textproto file into its
own subdirectory (e.g. out/default/beads_verify_<config_name>/args.gn), initialized
via //build/regenerator.py (with FX_NO_ENV_SYMLINK=1), and checked with
verify_build_commands.py.
"""

import argparse
import ast
import concurrent.futures
import dataclasses
import glob
import json
import os
import pathlib
import subprocess
import sys
import threading
import time
import typing as T

_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

sys.path.insert(0, str(_FUCHSIA_DIR / "build/bazel/scripts"))
import build_utils

sys.path.insert(0, str(_FUCHSIA_DIR / "build/beads/scripts"))
import compare_utils

sys.path.insert(0, str(_FUCHSIA_DIR / "build/beads/verifications"))
import verify_build_commands

DEFAULT_MANIFEST_RELPATH = pathlib.Path(
    "build/beads/verifications/targets_to_compare.json"
)

# Candidate directories searched for fint_params .textproto files.
# Note: `turquoise` is an internal repository that is not present in public
# Fuchsia checkouts; `resolve_fint_params_specs()` checks for directory
# existence (`is_dir()`) before searching each path.
FINT_PARAMS_SEARCH_DIRS = (
    pathlib.Path("integration/infra/config/generated/fuchsia/fint_params"),
    pathlib.Path("integration/infra/config/generated/turquoise/fint_params"),
)

# Built-in configuration presets (relative to global.ci under fuchsia fint_params).
PRESET_MODES: tuple[str, ...] = (
    # x64: debug, balanced, release
    "core.x64-debug",
    "core.x64-balanced",
    "core.x64-release",
    # arm64: debug, balanced, release
    "core.arm64-debug",
    "core.arm64-balanced",
    "core.arm64-release",
    # riscv64: debug, balanced, release
    "minimal.riscv64-debug",
    "minimal.riscv64-balanced",
    "minimal.riscv64-release",
)

PRESET_SANITIZERS: tuple[str, ...] = (
    "core.x64-asan",
    "core.arm64-asan",
    "core.arm64-hwasan",
    "bringup.riscv64-asan",
)

PRESET_LTO: tuple[str, ...] = (
    "core.x64-lto",
    "core.x64-thinlto",
    "core.arm64-lto",
    "core.arm64-thinlto",
    "minimal.riscv64-lto",
    "minimal.riscv64-thinlto",
)

PRESETS: dict[str, tuple[str, ...]] = {
    "modes": PRESET_MODES,
    "sanitizers": PRESET_SANITIZERS,
    "lto": PRESET_LTO,
    "default": PRESET_MODES + PRESET_SANITIZERS + PRESET_LTO,
}

_COMPILATION_MODE_MAP: dict[str, str] = {
    "COMPILATION_MODE_DEBUG": "debug",
    "COMPILATION_MODE_BALANCED": "balanced",
    "COMPILATION_MODE_RELEASE": "release",
    "COMPILATION_MODE_SANITIZER": "sanitizer",
}

_TARGET_ARCH_MAP: dict[str, str] = {
    "ARM64": "arm64",
    "X64": "x64",
    "RISCV64": "riscv64",
}

# GN argument variable names in fint_params `gn_args` that only control
# package/test target universes, RBE execution, or infra test metadata and
# should be omitted from minimal flag-verification build directories.
_IGNORED_GN_ARG_VARS: frozenset[str] = frozenset(
    {
        "allowed_test_device_types",
        "base_package_labels",
        "basic_env_names",
        "bazel_rbe_download_outputs",
        "bazel_rbe_enable",
        "build_event_service",
        "build_event_service_ninja",
        "build_only_labels",
        "build_should_trace_actions",
        "cache_package_labels",
        "cxx_rbe_check",
        "cxx_rbe_enable",
        "developer_test_labels",
        "disable_boot_tests",
        "e2e_test_labels",
        "enable_bazel_remote_services",
        "extra_kernel_test_images",
        "fidl_api_coverage",
        "generate_cuckoo_tests",
        "hangcheck_timeout_ms",
        "hermetic_test_package_labels",
        "host_labels",
        "host_test_labels",
        "link_rbe_check",
        "link_rbe_enable",
        "link_rbe_exec_strategy",
        "rbe_mode",
        "rust_rbe_check",
        "rust_rbe_enable",
        "test_package_labels",
        "universe_package_labels",
        "use_test_pilot",
    }
)

EXAMPLE_TARGETS_GROUP = "//build/beads/verifications:example_targets"


def _to_gn_value(val: T.Any) -> str:
    """Format a Python value as a GN literal, mirroring `toGNValue` in //tools/integration/fint/set.go.

    Note: fint_params `variants` entries may be raw GN scope strings (e.g.
    `{variant="asan-fuzzer" target_type=["fuzzed_executable"]}`) that must be
    emitted unquoted, whereas regular strings are double-quoted.
    """
    if isinstance(val, bool):
        return "true" if val else "false"
    if isinstance(val, int):
        return str(val)
    if isinstance(val, str):
        stripped = val.strip()
        # Scope or comma-separated scopes in select_variant are written raw.
        if (
            stripped.startswith("{")
            and stripped.endswith("}")
            and "=" in stripped
        ):
            return stripped
        escaped = val.replace("\\", "\\\\").replace('"', '\\"')
        return f'"{escaped}"'
    if isinstance(val, (list, tuple)):
        items = ", ".join(_to_gn_value(x) for x in val)
        return f"[{items}]"
    if isinstance(val, dict):
        entries = " ".join(f"{k} = {_to_gn_value(v)}" for k, v in val.items())
        return f"{{ {entries} }}"
    raise TypeError(f"Unsupported GN value type: {type(val)}")


def _extract_gn_arg_var_name(gn_arg_line: str) -> str:
    """Extract the LHS variable name from a GN assignment or += statement."""
    s = gn_arg_line.strip()
    for op in ("+=", "="):
        if op in s:
            lhs, _, _ = s.partition(op)
            return lhs.strip()
    return ""


def _should_keep_gn_arg(gn_arg_line: str) -> bool:
    """Return True if a raw gn_args line from fint_params should be kept."""
    s = gn_arg_line.strip()
    if not s:
        return False
    if s.startswith("import("):
        return True
    var_name = _extract_gn_arg_var_name(s)
    if var_name in _IGNORED_GN_ARG_VARS:
        return False
    return True


@dataclasses.dataclass
class FintParamsConfig:
    """Parsed subset of a fint.proto.Static textproto relevant to compiler/linker flags."""

    name: str
    source_path: pathlib.Path | None = None
    board: str = ""
    product: str = ""
    compilation_mode: str = ""
    target_cpu: str = ""
    variants: list[str] = dataclasses.field(default_factory=list)
    gn_args: list[str] = dataclasses.field(default_factory=list)

    def generate_args_gn(
        self,
        extra_build_only_labels: T.Sequence[str] = (),
    ) -> str:
        """Generate minimal args.gn content for this configuration."""
        imports: list[str] = []
        vars_map: dict[str, T.Any] = {}
        assign_lines: list[str] = []
        append_lines: list[str] = []

        if self.board:
            imports.append(f'import("//{self.board}")')
            board_name = pathlib.Path(self.board).stem
            vars_map["build_info_board"] = board_name

        if self.product:
            imports.append(f'import("//{self.product}")')
            product_name = pathlib.Path(self.product).stem
            vars_map["build_info_product"] = product_name

        if self.compilation_mode:
            vars_map["compilation_mode"] = self.compilation_mode

        if self.target_cpu:
            vars_map["target_cpu"] = self.target_cpu

        if self.variants:
            vars_map["select_variant"] = list(self.variants)

        # Skip running Bazel test queries during workspace regeneration.
        vars_map["export_bazel_tests"] = False

        for raw_arg in self.gn_args:
            if not _should_keep_gn_arg(raw_arg):
                continue
            stripped = raw_arg.strip()
            if stripped.startswith("import("):
                imports.append(stripped)
            elif "+=" in stripped:
                append_lines.append(stripped)
            else:
                assign_lines.append(stripped)

        build_only_targets = [EXAMPLE_TARGETS_GROUP]
        for label in extra_build_only_labels:
            if label not in build_only_targets:
                build_only_targets.append(label)
        vars_map["build_only_labels"] = build_only_targets

        lines: list[str] = []
        if self.source_path:
            lines.append(f"# Generated from {self.source_path}")
        else:
            lines.append(f"# Generated for {self.name}")

        if imports:
            lines.append("")
            lines.extend(sorted(set(imports)))

        lines.append("")
        lines.append("# Basic args:")
        for k in sorted(vars_map.keys()):
            lines.append(f"{k} = {_to_gn_value(vars_map[k])}")
        for line in sorted(assign_lines):
            lines.append(line)

        if append_lines:
            lines.append("")
            lines.append("# Target lists:")
            for line in append_lines:
                lines.append(line)

        return "\n".join(lines) + "\n"

    def host_config_signature(
        self,
    ) -> tuple[str, tuple[str, ...], tuple[str, ...]]:
        """Return a tuple identifying the host toolchain configuration for this config."""
        host_variants = tuple(sorted(v for v in self.variants if "host" in v))
        kept_gn_args = tuple(
            sorted(
                raw_arg.strip()
                for raw_arg in self.gn_args
                if _should_keep_gn_arg(raw_arg)
            )
        )
        return (self.compilation_mode, host_variants, kept_gn_args)


def parse_fint_params_textproto(
    content: str,
    name: str = "",
    source_path: pathlib.Path | None = None,
) -> FintParamsConfig:
    """Parse a fint_params .textproto file into a FintParamsConfig.

    Note: This lightweight parser only handles top-level scalar and repeated
    fields (`board`, `product`, `compilation_mode`, `target_arch`, `variants`,
    and `gn_args`) from `fint.proto.Static`. It does not parse nested protobuf
    messages; if nested fields are needed in the future, this should be
    replaced with a full textproto parser.
    """
    if not name and source_path is not None:
        name = source_path.stem

    config = FintParamsConfig(name=name, source_path=source_path)

    for raw_line in content.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        key, sep, val = line.partition(":")
        if not sep:
            continue
        key = key.strip()
        val = val.strip()

        if key == "board":
            config.board = ast.literal_eval(val)
        elif key == "product":
            config.product = ast.literal_eval(val)
        elif key == "compilation_mode":
            if val not in _COMPILATION_MODE_MAP:
                raise ValueError(
                    f"Unknown compilation_mode '{val}' in {source_path or name}"
                )
            config.compilation_mode = _COMPILATION_MODE_MAP[val]
        elif key == "target_arch":
            if val not in _TARGET_ARCH_MAP:
                raise ValueError(
                    f"Unknown target_arch '{val}' in {source_path or name}"
                )
            config.target_cpu = _TARGET_ARCH_MAP[val]
        elif key == "variants":
            config.variants.append(ast.literal_eval(val))
        elif key == "gn_args":
            config.gn_args.append(ast.literal_eval(val))

    return config


def extract_extra_build_only_labels(
    target_queries: T.Sequence[compare_utils.CompareCommandsQuery],
) -> list[str]:
    """Extract GN labels from queries that are not covered by //build/beads/verifications:example_targets."""
    extra_labels: list[str] = []
    for query in target_queries:
        if not query.gn:
            continue
        base_label, sep, tc_part = query.gn.partition("(")
        # Strip .actual subtarget suffix used by rustc_*.gni since .actual has private visibility.
        if base_label.endswith(".actual"):
            base_label = base_label[: -len(".actual")]

        if base_label.startswith(
            (
                "//build/beads/verifications/cc:",
                "//build/beads/verifications/rust:",
            )
        ):
            continue

        reconstructed = f"{base_label}({tc_part}" if sep else base_label
        if reconstructed not in extra_labels:
            extra_labels.append(reconstructed)

    return extra_labels


def resolve_fint_params_specs(
    fuchsia_dir: pathlib.Path,
    specs: T.Sequence[str],
) -> list[pathlib.Path]:
    """Resolve CLI --fint-params specs (paths, globs, or builder names) to .textproto paths."""
    resolved: list[pathlib.Path] = []
    seen: set[pathlib.Path] = set()

    def _add(p: pathlib.Path) -> None:
        rp = p.resolve()
        if rp not in seen:
            seen.add(rp)
            resolved.append(p)

    for spec in specs:
        # 1. Check direct path or glob relative to CWD or fuchsia_dir.
        for base in (pathlib.Path(), fuchsia_dir):
            candidate = base / spec
            if candidate.is_file():
                _add(candidate)
                break
            matches = sorted(glob.glob(str(candidate), recursive=True))
            if matches:
                for m in matches:
                    mp = pathlib.Path(m)
                    if mp.is_file():
                        _add(mp)
                break
        else:
            # 2. Search under FINT_PARAMS_SEARCH_DIRS by name or <bucket>/<name>.
            spec_name = (
                spec if spec.endswith(".textproto") else f"{spec}.textproto"
            )
            found = False
            # Prefer global.ci first if no bucket is specified. Only search
            # directories that exist in the current checkout (e.g. `turquoise`
            # is absent in public Fuchsia checkouts).
            existing_search_roots = [
                fuchsia_dir / rel_dir
                for rel_dir in FINT_PARAMS_SEARCH_DIRS
                if (fuchsia_dir / rel_dir).is_dir()
            ]
            for search_root in existing_search_roots:
                global_ci_candidate = search_root / "global.ci" / spec_name
                if global_ci_candidate.is_file():
                    _add(global_ci_candidate)
                    found = True
                    break
                direct_candidate = search_root / spec_name
                if direct_candidate.is_file():
                    _add(direct_candidate)
                    found = True
                    break
                path_matches = sorted(search_root.glob(f"*/{spec_name}"))
                if path_matches:
                    for mp in path_matches:
                        if mp.is_file():
                            _add(mp)
                    found = True
                    break

            if not found:
                raise FileNotFoundError(
                    f"Could not find fint_params file matching '{spec}'"
                )

    return resolved


def deduplicate_configs(
    configs: T.Sequence[FintParamsConfig],
    extra_build_only_labels: T.Sequence[str] = (),
) -> tuple[list[FintParamsConfig], dict[str, list[str]]]:
    """Deduplicate configs that produce identical args.gn content (ignoring header comment).

    Returns:
        A pair (unique_configs, aliases_map) where aliases_map maps each kept
        config's name to the list of all config names with identical args.gn.
    """
    unique_configs: list[FintParamsConfig] = []
    seen_by_body: dict[str, str] = {}
    aliases_map: dict[str, list[str]] = {}

    for cfg in configs:
        args_gn = cfg.generate_args_gn(extra_build_only_labels)
        # Strip the leading "# Generated ..." comment line before comparing.
        body_lines = [
            line
            for line in args_gn.splitlines()
            if not line.startswith("# Generated ")
        ]
        body_key = "\n".join(body_lines)
        if body_key in seen_by_body:
            primary_name = seen_by_body[body_key]
            aliases_map[primary_name].append(cfg.name)
        else:
            seen_by_body[body_key] = cfg.name
            aliases_map[cfg.name] = [cfg.name]
            unique_configs.append(cfg)

    return unique_configs, aliases_map


def select_queries_for_configs(
    configs: T.Sequence[FintParamsConfig],
    target_queries: T.Sequence[compare_utils.CompareCommandsQuery],
    dedup_host_queries: bool = True,
) -> list[list[compare_utils.CompareCommandsQuery]]:
    """Return the list of CompareCommandsQuery to run for each config.

    When `dedup_host_queries` is True, `--config=host` queries are only
    included in the first configuration for each unique host signature
    (compilation_mode, host_variants, kept_gn_args), avoiding redundant
    `bazel aquery/cquery --config=host` invocations across target CPUs
    and target-only variants (like LTO/ThinLTO).
    """
    if not dedup_host_queries:
        return [list(target_queries) for _ in configs]

    non_host_queries = [
        q
        for q in target_queries
        if q.bazel_config != compare_utils.BAZEL_CONFIG_HOST
    ]
    seen_host_sigs: set[tuple[str, tuple[str, ...], tuple[str, ...]]] = set()
    per_config_queries: list[list[compare_utils.CompareCommandsQuery]] = []
    for cfg in configs:
        sig = cfg.host_config_signature()
        if sig not in seen_host_sigs:
            seen_host_sigs.add(sig)
            per_config_queries.append(list(target_queries))
        else:
            per_config_queries.append(list(non_host_queries))
    return per_config_queries


def is_build_dir_up_to_date(
    fuchsia_dir: pathlib.Path,
    config_build_dir: pathlib.Path,
    expected_args_gn: str,
) -> bool:
    """Return True if config_build_dir already has matching args.gn and generated files."""
    args_gn_path = config_build_dir / "args.gn"
    if not args_gn_path.exists():
        return False
    if args_gn_path.read_text() != expected_args_gn:
        return False

    launcher = build_utils.find_bazel_launcher_path(
        fuchsia_dir, config_build_dir
    )
    if not launcher:
        return False

    required_files = (
        config_build_dir / "build.ninja",
        config_build_dir / "ninja_outputs.json",
        config_build_dir / verify_build_commands.BEADS_TOOLCHAINS_FILENAME,
    )
    return all(p.exists() for p in required_files)


def run_regenerator(
    fuchsia_dir: pathlib.Path,
    config_build_dir: pathlib.Path,
    verbose: bool = False,
) -> None:
    """Run //build/regenerator.py on config_build_dir with FX_NO_ENV_SYMLINK=1."""
    regenerator_script = fuchsia_dir / "build/regenerator.py"
    cmd = [
        sys.executable,
        str(regenerator_script),
        "--fuchsia-dir",
        str(fuchsia_dir),
        "--fuchsia-build-dir",
        str(config_build_dir),
        "--no-export-rust-project",
        "--no-gn-ide-json",
        "--no-gn-check",
        "--no-bazel-warm-up",
    ]
    if not verbose:
        cmd.append("--quiet")

    env = os.environ.copy()
    env["FX_NO_ENV_SYMLINK"] = "1"

    subprocess.run(
        cmd,
        cwd=fuchsia_dir,
        env=env,
        check=True,
        capture_output=not verbose,
        text=True,
    )


def shutdown_bazel_daemon(
    fuchsia_dir: pathlib.Path, config_build_dir: pathlib.Path
) -> None:
    """Shut down the per-build-dir Bazel server daemon to release JVM memory."""
    launcher = build_utils.find_bazel_launcher_path(
        fuchsia_dir, config_build_dir
    )
    if not launcher:
        return
    subprocess.run(
        [str(launcher), "shutdown"],
        cwd=config_build_dir,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


@dataclasses.dataclass
class ConfigVerificationResult:
    """Verification outcome for a single build configuration."""

    config: FintParamsConfig
    build_dir: pathlib.Path
    duration_sec: float
    regenerated: bool
    regen_error: str = ""
    verification: verify_build_commands.VerificationRunResult | None = None

    @property
    def passed(self) -> bool:
        return (
            not self.regen_error
            and self.verification is not None
            and self.verification.all_success
        )


def merge_deduped_host_results(
    results: list[ConfigVerificationResult],
) -> None:
    """Propagate `--config=host` comparison results to configs that shared the same host signature."""
    host_results_by_sig: dict[
        tuple[str, tuple[str, ...], tuple[str, ...]],
        list[compare_utils.CompareCommandsResult],
    ] = {}
    failed_config_by_sig: dict[
        tuple[str, tuple[str, ...], tuple[str, ...]],
        str,
    ] = {}
    for res in results:
        sig = res.config.host_config_signature()
        if res.verification is None:
            if sig not in failed_config_by_sig:
                failed_config_by_sig[sig] = res.config.name
            continue
        has_host = any(
            r.query.bazel_config == compare_utils.BAZEL_CONFIG_HOST
            for r in res.verification.results
        )
        if has_host and sig not in host_results_by_sig:
            host_results_by_sig[sig] = [
                r
                for r in res.verification.results
                if r.query.bazel_config == compare_utils.BAZEL_CONFIG_HOST
            ]

    for res in results:
        if res.verification is None:
            continue
        sig = res.config.host_config_signature()
        has_host = any(
            r.query.bazel_config == compare_utils.BAZEL_CONFIG_HOST
            for r in res.verification.results
        )
        if not has_host:
            if sig in host_results_by_sig:
                merged_cmp_results = (
                    list(host_results_by_sig[sig]) + res.verification.results
                )
                res.verification = (
                    verify_build_commands.summarize_comparison_results(
                        res.build_dir, merged_cmp_results
                    )
                )
                per_config_report = (
                    res.build_dir / "verify_build_commands.report.txt"
                )
                if res.build_dir.is_dir():
                    per_config_report.write_text(res.verification.report_text)
            elif sig in failed_config_by_sig:
                err_msg = (
                    f"Host queries (--config=host) were not verified because "
                    f"'{failed_config_by_sig[sig]}' failed during regeneration."
                )
                if err_msg not in res.verification.errors:
                    res.verification.errors.append(err_msg)
                res.verification.all_success = False


def _rel_to_fuchsia_dir(
    path: pathlib.Path, fuchsia_dir: pathlib.Path | None
) -> pathlib.Path:
    if fuchsia_dir is not None:
        try:
            return path.resolve().relative_to(fuchsia_dir.resolve())
        except ValueError:
            pass
    return path


def format_summary_report(
    results: T.Sequence[ConfigVerificationResult],
    aliases_map: dict[str, list[str]],
    fuchsia_dir: pathlib.Path | None = None,
) -> str:
    """Generate a combined human-readable report across all verified build configurations."""
    lines: list[str] = []
    total = len(results)
    passed = sum(1 for r in results if r.passed)
    failed = total - passed

    lines.append("# Multi-Config Build Command Verification Summary")
    lines.append("")
    lines.append(
        f"Total configurations tested: {total} ({passed} passed, {failed} failed)"
    )
    lines.append("")
    lines.append(
        "| Config | Arch | Mode | Variants | Status | Targets w/ Diffs | Unexpected Diffs | Time (s) | Build Dir |"
    )
    lines.append(
        "| :--- | :--- | :--- | :--- | :--- | :---: | :---: | :---: | :--- |"
    )

    for res in results:
        cfg = res.config
        variants_str = ", ".join(cfg.variants) if cfg.variants else "none"
        if res.regen_error:
            status = "REGEN_ERROR"
            diffs_str = "-"
            unexp_str = "-"
        elif res.verification is not None:
            status = "PASS" if res.verification.all_success else "FAIL"
            diffs_str = str(res.verification.differences_count)
            unexp_str = str(res.verification.unexpected_differences_count)
        else:
            status = "GENERATED_ONLY"
            diffs_str = "-"
            unexp_str = "-"

        rel_build_dir = _rel_to_fuchsia_dir(res.build_dir, fuchsia_dir)
        lines.append(
            f"| `{cfg.name}` | `{cfg.target_cpu}` | `{cfg.compilation_mode}` "
            f"| `{variants_str}` | **{status}** | {diffs_str} | {unexp_str} "
            f"| {res.duration_sec:.1f}s | `{rel_build_dir}` |"
        )

    for res in results:
        cfg = res.config
        aliases = aliases_map.get(cfg.name, [cfg.name])
        alias_note = (
            f" (also covers: {', '.join(aliases[1:])})"
            if len(aliases) > 1
            else ""
        )
        if res.regen_error:
            lines.append("")
            lines.append(f"## {cfg.name}{alias_note} - REGEN_ERROR")
            lines.append("")
            lines.append(res.regen_error)
        elif res.verification and (
            res.verification.errors or res.verification.report_text
        ):
            lines.append("")
            lines.append(f"## {cfg.name}{alias_note}")
            for err in res.verification.errors:
                lines.append("")
                lines.append(f"ERROR: {err}")
            if res.verification.report_text:
                lines.append(res.verification.report_text)

    return "\n".join(lines) + "\n"


def main(argv: T.Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    build_utils.BuildPaths.add_parser_arguments(parser)

    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=None,
        help=f"Path to the manifest file (JSON). Defaults to //{DEFAULT_MANIFEST_RELPATH}.",
    )
    parser.add_argument(
        "--preset",
        choices=tuple(PRESETS.keys()),
        default="default",
        help=(
            "Built-in configuration preset to use when --fint-params is not specified "
            "(default: 'default', which includes all 3 architectures in debug, balanced, "
            "and release modes + sanitizers + lto/thinlto)."
        ),
    )
    parser.add_argument(
        "--fint-params",
        nargs="+",
        action="extend",
        default=[],
        help=(
            "One or more fint_params .textproto paths, glob patterns, or builder names "
            "(e.g. 'core.x64-release' or 'integration/infra/config/generated/fuchsia/fint_params/global.ci/*.textproto'). "
            "Overrides --preset when specified."
        ),
    )
    parser.add_argument(
        "--arch",
        nargs="+",
        action="extend",
        choices=("x64", "arm64", "riscv64"),
        default=[],
        help="Filter selected configurations by target architecture (x64, arm64, riscv64).",
    )
    parser.add_argument(
        "--compilation-mode",
        nargs="+",
        action="extend",
        choices=("debug", "balanced", "release", "sanitizer"),
        default=[],
        help="Filter selected configurations by compilation mode.",
    )
    parser.add_argument(
        "--variant",
        nargs="+",
        action="extend",
        default=[],
        help="Filter selected configurations by variant substring (e.g. 'asan', 'hwasan', 'lto', 'thinlto', or 'none').",
    )
    parser.add_argument(
        "--dir-prefix",
        default="beads_verify_",
        help="Subdirectory prefix under the base build directory (default: 'beads_verify_').",
    )
    parser.add_argument(
        "--dedup-args",
        "--deduplicate",
        dest="dedup_args",
        action="store_true",
        default=False,
        help="Deduplicate configurations that generate identical args.gn files.",
    )
    parser.add_argument(
        "--no-dedup-host-queries",
        action="store_true",
        default=False,
        help="Run --config=host queries on every configuration instead of once per unique host config.",
    )
    parser.add_argument(
        "--jobs",
        "-j",
        type=int,
        default=min(4, os.cpu_count() or 1),
        help="Number of build configurations to verify in parallel (default: min(4, cpu_count)).",
    )
    parser.add_argument(
        "--skip-variants",
        action="store_true",
        default=False,
        help="Skip configurations that specify select_variant (variants).",
    )
    parser.add_argument(
        "--force-regen",
        action="store_true",
        default=False,
        help="Force running //build/regenerator.py even if args.gn and build.ninja are up-to-date.",
    )
    parser.add_argument(
        "--dry-run",
        "-n",
        action="store_true",
        default=False,
        help="List the resolved build configurations without writing files or running verification.",
    )
    parser.add_argument(
        "--generate-only",
        action="store_true",
        default=False,
        help="Only generate per-config args.gn files without running regenerator.py or verification.",
    )
    parser.add_argument(
        "--keep-bazel-daemons",
        action="store_true",
        default=False,
        help="Do not run `bazel shutdown` after verifying each configuration.",
    )
    parser.add_argument(
        "--read_response_files",
        action="store_true",
        default=False,
        help="Read response files directly from the Bazel execroot instead of querying Starlark providers.",
    )
    parser.add_argument(
        "--report",
        type=pathlib.Path,
        default=None,
        help="Write aggregated summary report to this path (defaults to <build_dir>/beads_verify_summary.report.txt).",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        default=False,
        help="Print verbose output.",
    )

    args = parser.parse_args(argv)

    fuchsia_dir = (
        args.fuchsia_dir.resolve()
        if args.fuchsia_dir
        else build_utils.find_fuchsia_dir()
    )
    if args.build_dir:
        base_build_dir = args.build_dir.resolve()
    else:
        base_build_dir = build_utils.find_fx_build_dir(fuchsia_dir) or (
            fuchsia_dir / "out/default"
        )

    manifest_path = (
        args.manifest.resolve()
        if args.manifest
        else (fuchsia_dir / DEFAULT_MANIFEST_RELPATH)
    )
    with open(manifest_path) as f:
        targets_data = json.load(f)

    target_queries = [
        compare_utils.CompareCommandsQuery.from_dict(t) for t in targets_data
    ]
    extra_build_only_labels = extract_extra_build_only_labels(target_queries)

    specs = args.fint_params if args.fint_params else PRESETS[args.preset]
    textproto_paths = resolve_fint_params_specs(fuchsia_dir, specs)

    configs: list[FintParamsConfig] = []
    seen_names: set[str] = set()
    for path in textproto_paths:
        rel_source = (
            path.resolve().relative_to(fuchsia_dir)
            if path.resolve().is_relative_to(fuchsia_dir)
            else path
        )
        cfg_name = path.stem
        if cfg_name in seen_names:
            cfg_name = f"{path.parent.name}_{path.stem}"
        seen_names.add(cfg_name)

        cfg = parse_fint_params_textproto(
            path.read_text(),
            name=cfg_name,
            source_path=rel_source,
        )
        if args.skip_variants and cfg.variants:
            if args.verbose:
                print(
                    f"Skipping variant config: {cfg.name} ({cfg.variants})",
                    file=sys.stderr,
                )
            continue
        if args.arch and cfg.target_cpu not in args.arch:
            continue
        if (
            args.compilation_mode
            and cfg.compilation_mode not in args.compilation_mode
        ):
            continue
        if args.variant:
            cfg_variants = cfg.variants if cfg.variants else ["none"]
            cfg_variant_tokens = {
                token
                for variant in cfg_variants
                for token in (variant, *variant.split("-"))
            }
            if not any(v in cfg_variant_tokens for v in args.variant):
                continue
        configs.append(cfg)

    if not configs:
        print("No build configurations selected.")
        return 0

    if args.dedup_args:
        configs, aliases_map = deduplicate_configs(
            configs, extra_build_only_labels
        )
    else:
        aliases_map = {c.name: [c.name] for c in configs}

    def _debug(msg: T.Any) -> None:
        if args.verbose:
            print(f"DEBUG: {msg}", file=sys.stderr)

    results: list[ConfigVerificationResult] = []
    total_configs = len(configs)

    if args.dry_run:
        for idx, cfg in enumerate(configs, 1):
            config_build_dir = _rel_to_fuchsia_dir(
                base_build_dir / f"{args.dir_prefix}{cfg.name}", fuchsia_dir
            )
            alias_list = aliases_map.get(cfg.name, [cfg.name])
            alias_note = (
                f" [aliases: {', '.join(alias_list[1:])}]"
                if len(alias_list) > 1
                else ""
            )
            print(
                f"[{idx}/{total_configs}] {cfg.name}{alias_note} "
                f"(arch={cfg.target_cpu}, mode={cfg.compilation_mode}, "
                f"variants={cfg.variants or ['none']}) -> {config_build_dir} "
                f"[source: {cfg.source_path}]"
            )
        return 0

    non_host_queries = [
        q
        for q in target_queries
        if q.bazel_config != compare_utils.BAZEL_CONFIG_HOST
    ]
    has_host_queries = len(non_host_queries) < len(target_queries)
    dedup_host = not args.no_dedup_host_queries and has_host_queries

    print_lock = threading.Lock()
    host_dedup_lock = threading.Lock()
    claimed_host_sigs: set[tuple[str, tuple[str, ...], tuple[str, ...]]] = set()
    host_results_by_sig: dict[
        tuple[str, tuple[str, ...], tuple[str, ...]],
        list[compare_utils.CompareCommandsResult],
    ] = {}
    pending_by_sig: dict[
        tuple[str, tuple[str, ...], tuple[str, ...]],
        list[tuple[int, ConfigVerificationResult]],
    ] = {}

    def _finalize_config_result(
        idx: int, res: ConfigVerificationResult
    ) -> None:
        assert res.verification is not None
        ver_result = res.verification
        per_config_report = res.build_dir / "verify_build_commands.report.txt"
        per_config_report.write_text(ver_result.report_text)

        status_str = "PASS" if ver_result.all_success else "FAIL"
        with print_lock:
            print(
                f"  [{idx}/{total_configs}] {res.config.name} -> {status_str} ({res.duration_sec:.1f}s, "
                f"{ver_result.differences_count} targets w/ diffs, "
                f"{ver_result.unexpected_differences_count} unexpected diffs, "
                f"{len(ver_result.errors)} query errors) "
                f"[report: {_rel_to_fuchsia_dir(per_config_report, fuchsia_dir)}]",
                flush=True,
            )

    def _process_config(
        idx: int,
        cfg: FintParamsConfig,
    ) -> ConfigVerificationResult:
        config_build_dir = base_build_dir / f"{args.dir_prefix}{cfg.name}"
        config_build_dir.mkdir(parents=True, exist_ok=True)
        rel_config_build_dir = _rel_to_fuchsia_dir(
            config_build_dir, fuchsia_dir
        )

        args_gn_content = cfg.generate_args_gn(extra_build_only_labels)
        args_gn_path = config_build_dir / "args.gn"

        start_time = time.monotonic()
        regenerated = False

        if args.generate_only:
            args_gn_path.write_text(args_gn_content)
            elapsed = time.monotonic() - start_time
            with print_lock:
                print(
                    f"[{idx}/{total_configs}] Generated {args_gn_path}",
                    flush=True,
                )
            return ConfigVerificationResult(
                config=cfg,
                build_dir=config_build_dir,
                duration_sec=elapsed,
                regenerated=False,
            )

        needs_regen = args.force_regen or not is_build_dir_up_to_date(
            fuchsia_dir, config_build_dir, args_gn_content
        )
        if (
            not args_gn_path.exists()
            or args_gn_path.read_text() != args_gn_content
        ):
            args_gn_path.write_text(args_gn_content)

        with print_lock:
            print(
                f"[{idx}/{total_configs}] Verifying {cfg.name} "
                f"(arch={cfg.target_cpu}, mode={cfg.compilation_mode}, "
                f"variants={cfg.variants or ['none']}) in {rel_config_build_dir} ...",
                flush=True,
            )

        if needs_regen:
            try:
                run_regenerator(
                    fuchsia_dir, config_build_dir, verbose=args.verbose
                )
                regenerated = True
            except subprocess.CalledProcessError as e:
                elapsed = time.monotonic() - start_time
                err_msg = (
                    f"regenerator.py failed with exit code {e.returncode}:\n"
                    f"STDOUT:\n{e.stdout or ''}\nSTDERR:\n{e.stderr or ''}"
                )
                with print_lock:
                    print(
                        f"  [{idx}/{total_configs}] {cfg.name} -> REGEN_ERROR ({elapsed:.1f}s)\n{err_msg}",
                        file=sys.stderr,
                        flush=True,
                    )
                return ConfigVerificationResult(
                    config=cfg,
                    build_dir=config_build_dir,
                    duration_sec=elapsed,
                    regenerated=True,
                    regen_error=err_msg,
                )

        sig = cfg.host_config_signature()
        is_host_primary = False
        needs_host_merge = False
        if dedup_host:
            with host_dedup_lock:
                if sig not in claimed_host_sigs:
                    claimed_host_sigs.add(sig)
                    is_host_primary = True
                    cfg_queries = list(target_queries)
                else:
                    needs_host_merge = True
                    cfg_queries = list(non_host_queries)
        else:
            cfg_queries = list(target_queries)

        try:
            ver_result = (
                verify_build_commands.verify_build_commands_for_build_dir(
                    fuchsia_dir=fuchsia_dir,
                    build_dir=config_build_dir,
                    target_queries=cfg_queries,
                    read_response_files=args.read_response_files,
                    debug_fn=_debug,
                )
            )
        finally:
            if not args.keep_bazel_daemons:
                shutdown_bazel_daemon(fuchsia_dir, config_build_dir)

        elapsed = time.monotonic() - start_time
        res = ConfigVerificationResult(
            config=cfg,
            build_dir=config_build_dir,
            duration_sec=elapsed,
            regenerated=regenerated,
            verification=ver_result,
        )

        if is_host_primary:
            _finalize_config_result(idx, res)
            with host_dedup_lock:
                host_results_for_sig = [
                    r
                    for r in ver_result.results
                    if r.query.bazel_config == compare_utils.BAZEL_CONFIG_HOST
                ]
                host_results_by_sig[sig] = host_results_for_sig
                pending_secondaries = pending_by_sig.pop(sig, [])
            for sec_idx, sec_res in pending_secondaries:
                assert sec_res.verification is not None
                sec_res.verification = (
                    verify_build_commands.summarize_comparison_results(
                        sec_res.build_dir,
                        list(host_results_for_sig)
                        + sec_res.verification.results,
                    )
                )
                _finalize_config_result(sec_idx, sec_res)
        elif needs_host_merge:
            with host_dedup_lock:
                if sig in host_results_by_sig:
                    host_results_for_sig = host_results_by_sig[sig]
                else:
                    host_results_for_sig = None
                    pending_by_sig.setdefault(sig, []).append((idx, res))
            if host_results_for_sig is not None:
                res.verification = (
                    verify_build_commands.summarize_comparison_results(
                        res.build_dir,
                        list(host_results_for_sig) + ver_result.results,
                    )
                )
                _finalize_config_result(idx, res)
        else:
            _finalize_config_result(idx, res)

        return res

    max_workers = (
        1 if args.generate_only else min(max(1, args.jobs), total_configs)
    )
    if max_workers == 1:
        results = [
            _process_config(idx, cfg) for idx, cfg in enumerate(configs, 1)
        ]
    else:
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=max_workers
        ) as executor:
            futures = [
                executor.submit(_process_config, idx, cfg)
                for idx, cfg in enumerate(configs, 1)
            ]
            results = [f.result() for f in futures]

    if args.generate_only:
        return 0

    if not args.no_dedup_host_queries:
        merge_deduped_host_results(results)
        for pending_list in pending_by_sig.values():
            for sec_idx, sec_res in pending_list:
                _finalize_config_result(sec_idx, sec_res)
        pending_by_sig.clear()

    summary_text = format_summary_report(
        results, aliases_map, fuchsia_dir=fuchsia_dir
    )
    summary_report_path = (
        args.report.resolve()
        if args.report
        else (base_build_dir / "beads_verify_summary.report.txt")
    )
    summary_report_path.parent.mkdir(parents=True, exist_ok=True)
    summary_report_path.write_text(summary_text)

    print(f"\nSummary report written to: {summary_report_path}")
    all_passed = all(r.passed for r in results)
    return 0 if all_passed else 1


if __name__ == "__main__":
    sys.exit(main())
