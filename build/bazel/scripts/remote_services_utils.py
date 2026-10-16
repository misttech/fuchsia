# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Functions to generate a remote_services.bazelrc file from a template and RBE config files.
"""

import os
import sys
import typing as T
from pathlib import Path

# Dynamically resolve and insert the Fuchsia root directory into sys.path
# to support standalone script package imports (e.g. from build.rbe).
_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent.parent
if str(_FUCHSIA_DIR) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_DIR))

from build.rbe import rbe_profile, rbe_settings
from build.resultstore import rs_profile

# The list of configuration files, relative to the FUCHSIA_DIR.
RBE_CONFIG_FILES = [
    "build/rbe/fuchsia-rewrapper.cfg",
    "build/rbe/fuchsia-reproxy.cfg",
]

# Location of template file.
RBE_TEMPLATE_FILE = "build/bazel/templates/template.remote_services.bazelrc"


def generate_rbe_config(
    fuchsia_dir: Path,
    config_files: T.Sequence[str] = RBE_CONFIG_FILES,
    environ: T.Mapping[str, str] | None = None,
    rbe_enabled: bool = True,
    rs_enabled: bool = True,
) -> tuple[dict[str, str], set[Path]]:
    """Read RBE and ResultStore configs and active profiles into a dictionary.

    Args:
        fuchsia_dir: Fuchsia source directory.
        config_files: An optional list of config file path strings, relative to
           fuchsia_dir.
        environ: Optional environment dictionary overrides.
        rbe_enabled: Whether RBE is enabled in this build context.
        rs_enabled: Whether ResultStore is enabled in this build context.
    Returns:
        A (config_dict, input_files) pair, where config_dict is a dictionary
        mapping configuration keys to values, and input_files is a set of
        input file Paths read by the function.
    """
    if environ is None:
        environ = dict(os.environ)

    input_files: set[Path] = set()
    config: dict[str, str] = {}
    for f in config_files:
        cfg_file = fuchsia_dir / f
        input_files.add(cfg_file)
        if cfg_file.is_file():
            for line in cfg_file.read_text().splitlines():
                stripped = line.strip()
                if not stripped or stripped.startswith("#"):
                    # skip empty and comment lines
                    continue
                key, sep, value = stripped.partition("=")
                if sep == "=":
                    config[key] = value

    # Resolve and load active profiles cleanly (transparently handling Option C bypass)
    rbe_prof = rbe_profile.load(fuchsia_dir, environ)
    rs_prof = rs_profile.load(fuchsia_dir, environ)

    # Track only the active on-disk configuration/profile files in the build system when enabled
    if rbe_enabled:
        rbe_paths = rbe_profile.resolve_paths(fuchsia_dir, environ)
        for path in rbe_paths.active_inputs:
            if path.is_file():
                input_files.add(path)
    if rs_enabled:
        rs_paths = rs_profile.resolve_paths(fuchsia_dir, environ)
        for path in rs_paths.active_inputs:
            if path.is_file():
                input_files.add(path)

    config["rbe_instance"] = rbe_prof.rbe_instance
    config["container_image"] = rbe_prof.container_image
    config["rs_instance"] = rs_prof.rs_instance
    config["rs_project"] = rs_prof.rs_project
    config["cas_instance"] = rs_prof.cas_instance
    config["results_url"] = rs_prof.results_url

    return (config, input_files)


def generate_rbe_template_substitutions(
    config_dict: dict[str, str], remote_download_outputs: str
) -> dict[str, str]:
    """Generate formatting arguments to expand the remote services template.

    Args:
        config_dict: The value returned by generate_rbe_config
        remote_download_outputs: Value for remote_download_outputs key.
    Returns:
        A dictionary used to expand template.remote_services.bazelrc.
    """
    # Expected format: comma-separated "key=value" strings.
    platform_values = config_dict.get("platform", "").split(",")
    platform_vars = {}
    for pv in platform_values:
        if not pv:
            continue
        k, _, v = pv.partition("=")
        platform_vars[k] = v

    return {
        "remote_download_outputs": remote_download_outputs,
        "remote_instance_name": config_dict["rbe_instance"],
        "rs_instance": config_dict["rs_instance"],
        "rs_project": config_dict["rs_project"],
        "cas_instance": config_dict["cas_instance"],
        "results_url": config_dict["results_url"],
        "container_image": config_dict.get("container_image")
        or platform_vars.get("container-image", ""),
    }


def generate_remote_services_bazelrc(
    fuchsia_dir: Path,
    build_dir: Path,
    output_path: Path,
    download_outputs: str,
    environ: T.Mapping[str, str] | None = None,
    settings: rbe_settings.RbeSettings | None = None,
) -> set[Path]:
    """Generate the remote_services.bazelrc file.

    Args:
        fuchsia_dir: Fuchsia source directory.
        build_dir: Fuchsia build directory where rbe_settings.json resides.
        output_path: Output file location.
        download_outputs: Value for "remote_download_outputs".
        environ: Optional environment dictionary to query for overrides.
        settings: Optional RbeSettings instance for dependency injection in tests.

    Returns:
        A set of Path values for the input files read by this function.
    """
    if settings is None:
        try:
            settings = rbe_settings.load(build_dir)
        except ValueError as e:
            raise ValueError(
                f"Failed to generate remote services bazelrc because RBE settings "
                f"could not be loaded from build directory {build_dir}: {e}. "
                "Please make sure you have successfully run 'fx set' or 'gn gen' to "
                "populate the build directory."
            )

    rbe_enabled = settings.rbe_enabled
    rs_enabled = settings.rs_enabled

    rbe_config, rbe_inputs = generate_rbe_config(
        fuchsia_dir,
        environ=environ,
        rbe_enabled=rbe_enabled,
        rs_enabled=rs_enabled,
    )
    rbe_substitutions = generate_rbe_template_substitutions(
        rbe_config, download_outputs
    )

    rbe_template_path = fuchsia_dir / RBE_TEMPLATE_FILE
    rbe_inputs.add(rbe_template_path)
    rbe_template = rbe_template_path.read_text()

    remote_services_bazelrc = rbe_template.format(**rbe_substitutions)
    output_path.write_text(remote_services_bazelrc)

    return rbe_inputs
