# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import base64
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

import logger

LOCAL_JIRI_MANIFEST_CONTENT = """
<manifest>
  <imports>
    <localimport file="manifests/third_party/all"/>
    <localimport file="manifests/prebuilts"/>
  </imports>
</manifest>
"""


class Prebuilts:
    """A class to manage prebuilts for a cog workspace."""

    def __init__(
        self,
        cartfs_directory: str,
        workspace_dir: str,
        workspace_name: str,
        repo_name: str,
    ):
        self.cartfs_directory = Path(cartfs_directory)
        self.workspace_dir = Path(workspace_dir)
        self.workspace_name = workspace_name
        self.repo_name = repo_name

    def create_symlink(self, target: Path, link_name: Path) -> None:
        """Creates a symlink from link_name to target.

        If a symlink already exists at link_name and points to target, this
        function does nothing.

        If a file, directory, or a different symlink exists at link_name, it will
        be removed and replaced with the new symlink.
        """
        if link_name.is_symlink() and link_name.readlink() == target:
            return

        # If the path exists but is not the desired symlink, remove it.
        if link_name.is_dir() and not link_name.is_symlink():
            shutil.rmtree(link_name)
        else:
            link_name.unlink(missing_ok=True)

        link_name.symlink_to(target)

    def _run_bootstrap_jiri_script(self) -> None:
        """Runs the bootstrap jiri script."""
        url = "https://fuchsia.googlesource.com/jiri/+/HEAD/scripts/bootstrap_jiri?format=TEXT"
        try:
            with urllib.request.urlopen(url) as response:
                encoded_script = response.read()
                decoded_script = base64.b64decode(encoded_script)
                subprocess.run(
                    ["bash", "-s", self.cartfs_directory],
                    input=decoded_script,
                    check=True,
                )
        except (urllib.error.URLError, subprocess.CalledProcessError) as e:
            logger.log_error(f"Failed to bootstrap jiri: {e}")
            sys.exit(1)

    def _write_jiri_manifest(self) -> None:
        """Writes the jiri manifest."""
        self._patch_file(
            filepath=".jiri_manifest",
            content=LOCAL_JIRI_MANIFEST_CONTENT,
            symlink=True,
        )

    def _write_jiri_config(self) -> None:
        """Initialize the jiri config."""
        logger.log_info("Initialize the jiri config.")
        subprocess.run(
            [
                ".jiri_root/bin/jiri",
                "init",
                "-analytics-opt=true",
                self.cartfs_directory,
            ],
            cwd=self.cartfs_directory,
            check=True,
        )

    def _create_jiri_snapshot(self) -> None:
        """Create snapshot."""
        logger.log_info("Create snapshot at .jiri_root/update_history/latest.")
        (self.cartfs_directory / ".jiri_root/update_history").mkdir(
            parents=True, exist_ok=True
        )
        subprocess.run(
            [
                ".jiri_root/bin/jiri",
                "snapshot",
                ".jiri_root/update_history/latest",
            ],
            cwd=self.cartfs_directory,
            check=True,
        )

    def is_jiri_bootstrapped(self) -> bool:
        """Checks if jiri is bootstrapped."""
        jiri_root = self.cartfs_directory / ".jiri_root"
        jiri_manifest = self.cartfs_directory / ".jiri_manifest"
        return jiri_root.is_dir() and jiri_manifest.exists()

    def bootstrap_jiri(self) -> None:
        """Bootstraps jiri if it is not already bootstrapped."""
        logger.log_info("Bootstrapping jiri.")
        self._run_bootstrap_jiri_script()
        self._write_jiri_manifest()

    def fetch_prebuilts(self) -> None:
        """Fetches prebuilts for the given repo."""
        logger.log_info(f"Fetching prebuilts for {self.repo_name}.")
        subprocess.run(
            [".jiri_root/bin/jiri", "fetch-packages"],
            cwd=self.cartfs_directory,
            check=True,
        )

    def cartfs_structure_initialization(self) -> None:
        """Create essential artifacts used by build."""
        # Create files
        self._patch_file(filepath="integration/MILESTONE", content="30")
        self._patch_file(
            filepath="build/cipd.gni",
            content="internal_access = false",
            symlink=True,
        )
        self._patch_file(
            filepath="build/info/jiri_generated/integration_commit_hash.txt",
            content="20560e50d0a87e8c0093b7ed21ebcaa46e64bb50",
            symlink=True,
        )
        self._patch_file(
            filepath="build/info/jiri_generated/integration_commit_stamp.txt",
            content="1762987703",
            symlink=True,
        )
        self._patch_file(
            filepath="build/info/jiri_generated/integration_daily_commit_hash.txt",
            content="843090d610fd85d7c7ffc4d1adf3abd01d367ae8",
            symlink=True,
        )
        self._patch_file(
            filepath="build/info/jiri_generated/integration_daily_commit_stamp.txt",
            content="1762905105",
            symlink=True,
        )

        # Copy manifests directory to CartFS.
        logger.log_info("Copy manifests directory to CartFS.")
        shutil.copytree(
            self.workspace_dir / self.repo_name / "manifests",
            self.cartfs_directory / "manifests",
            dirs_exist_ok=True,
        )

        self._write_jiri_config()
        self._create_jiri_snapshot()

        # Create directories
        (self.cartfs_directory / ".fx").mkdir(exist_ok=True)

        # Initialize git repository in the submodules
        submodules = [
            "third_party/mesa-migrating/src",
            "third_party/boringssl/src",
            "third_party/glslang/src",
            "third_party/go",
        ]
        for submodule in submodules:
            # This would create a .git/HEAD
            submodule_path = self.workspace_dir / self.repo_name / submodule
            if (submodule_path / ".git").exists():
                continue
            subprocess.run(
                ["git", "init", "-b", "main"],
                cwd=submodule_path,
                check=True,
            )
            # This would create a .git/index
            subprocess.run(
                ["git", "reset"],
                cwd=submodule_path,
                check=True,
            )

    def create_symlinks(self) -> None:
        """Creates symlinks for the prebuilts."""
        logger.log_info("Creating symlinks for the prebuilts.")
        # Link the paths in the repo to cartfs
        for path in [
            "prebuilt",
            ".jiri_root",
            ".cipd",
            ".fx",
            "integration",
        ]:
            repo_path = self.workspace_dir / self.repo_name / path
            cartfs_path = self.cartfs_directory / path
            logger.log_info(
                f"Creating symlink from {repo_path} to {cartfs_path}"
            )
            self.create_symlink(
                cartfs_path,
                repo_path,
            )

        # Link .jiri_root/bin/{fx, ffx, hermetic-env, fuchsia-vendored-python}
        # LINT.IfChange
        self.create_symlink(
            self.workspace_dir / self.repo_name / "scripts/fx",
            self.cartfs_directory / ".jiri_root/bin/fx",
        )
        self.create_symlink(
            self.workspace_dir
            / self.repo_name
            / "src/developer/ffx/scripts/ffx",
            self.cartfs_directory / ".jiri_root/bin/ffx",
        )
        self.create_symlink(
            self.workspace_dir / self.repo_name / "scripts/hermetic-env",
            self.cartfs_directory / ".jiri_root/bin/hermetic-env",
        )
        self.create_symlink(
            self.workspace_dir
            / self.repo_name
            / "scripts/fuchsia-vendored-python",
            self.cartfs_directory / ".jiri_root/bin/fuchsia-vendored-python",
        )
        # LINT.ThenChange(//scripts/devshell/lib/add_symlink_to_bin.sh)

        # Symlink in cog workspace specific GN arg overrides.
        (self.workspace_dir / self.repo_name / "local").mkdir(exist_ok=True)
        self.create_symlink(
            self.workspace_dir
            / self.repo_name
            / "scripts/cog/resources/args.gn",
            self.workspace_dir / self.repo_name / "local/args.gn",
        )

    def _patch_file(
        self, filepath: str, content: str, symlink: bool = False
    ) -> None:
        """Patches the file in cartFS."""
        logger.log_info(f"Patching the {filepath} file.")
        full_filepath = self.cartfs_directory / filepath
        if not full_filepath.exists():
            logger.log_info(
                f"File {full_filepath} does not exist. Creating it now."
            )
            full_filepath.parent.mkdir(parents=True, exist_ok=True)
            try:
                full_filepath.write_text(content)
            except Exception as e:
                logger.log_error(
                    f"An error occurred while writing the file: {e}"
                )
        else:
            logger.log_info(f"File {full_filepath} already exists.")

        # Symlink from workspace if workspace path is specified
        if symlink:
            self.create_symlink(
                full_filepath,
                self.workspace_dir / self.repo_name / filepath,
            )
