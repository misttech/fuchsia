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
    <localimport file="../integration/flower"/>
  </imports>
</manifest>
"""


class Prebuilts:
    """A class to manage prebuilts for a cog workspace."""

    def __init__(
        self,
        cartfs_directory: Path,
        workspace_dir: Path,
        workspace_name: str,
        repo_name: str,
    ):
        self.cartfs_directory = cartfs_directory
        self.cartfs_fuchsia_dir = self.cartfs_directory / "fuchsia"
        self.workspace_dir = workspace_dir
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

        if not link_name.parent.is_dir():
            link_name.parent.mkdir(parents=True, exist_ok=True)

        link_name.symlink_to(target)

    def _run_bootstrap_jiri_script(self) -> None:
        """Runs the bootstrap jiri script."""
        url = "https://fuchsia.googlesource.com/jiri/+/HEAD/scripts/bootstrap_jiri?format=TEXT"
        try:
            with urllib.request.urlopen(url) as response:
                encoded_script = response.read()
                decoded_script = base64.b64decode(encoded_script)
                subprocess.run(
                    ["bash", "-s", self.cartfs_fuchsia_dir],
                    input=decoded_script,
                    check=True,
                )
        except (urllib.error.URLError, subprocess.CalledProcessError) as e:
            logger.log_error(f"Failed to bootstrap jiri: {e}")
            sys.exit(1)

    def _write_jiri_manifest(self) -> None:
        """Writes the jiri manifest."""
        self._patch_file(
            filepath="fuchsia/.jiri_manifest",
            content=LOCAL_JIRI_MANIFEST_CONTENT,
        )

    def _write_jiri_config(self) -> None:
        """Initialize the jiri config."""
        logger.log_info("Initialize the jiri config.")
        subprocess.run(
            [
                ".jiri_root/bin/jiri",
                "init",
                "-analytics-opt=true",
                self.cartfs_fuchsia_dir,
            ],
            cwd=self.cartfs_fuchsia_dir,
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
        self._write_jiri_config()

    def fetch_prebuilts(self) -> None:
        """Fetches prebuilts for the given repo."""
        logger.log_info(f"Fetching prebuilts for {self.repo_name}.")
        cartfs_fuchsia_dir = self.cartfs_fuchsia_dir
        if (cartfs_fuchsia_dir / ".git").exists():
            subprocess.run(
                ["git", "reset", "--hard"],
                cwd=cartfs_fuchsia_dir,
                check=True,
            )

        subprocess.run(
            [".jiri_root/bin/jiri", "update"],
            cwd=cartfs_fuchsia_dir,
            check=True,
        )

    def create_integration_repository(self) -> None:
        """Creates the integration repository."""
        logger.log_info("Setup the integration repository.")
        integration_directory = self.cartfs_directory / "integration"

        # Setup the integration repository
        if integration_directory.exists():
            shutil.rmtree(integration_directory, ignore_errors=True)

        subprocess.run(
            [
                "git",
                "clone",
                "https://fuchsia.googlesource.com/integration",
                "--depth",
                "100",
            ],
            cwd=self.cartfs_directory,
            check=True,
        )

        # Determine the fuchsia repo commit hash.
        fuchsia_repo_states = (
            subprocess.run(
                [
                    "git",
                    "citc",
                    "api.get-repo-states",
                    "fuchsia",
                ],
                check=True,
                capture_output=True,
            )
            .stdout.decode("utf-8")
            .strip()
            .split("\n")
        )
        for state in fuchsia_repo_states:
            if "base_commit_hash" in state:
                fuchsia_repo_commit_hash = (
                    state.split(":")[1].strip().replace('"', "")
                )
                break

        logger.log_info(f"fuchsia_repo_commit_hash: {fuchsia_repo_commit_hash}")

        # We use the first 7 characters of the fuchsia repo to look up in
        # integration repo's commit message
        commit_hash_prefix = fuchsia_repo_commit_hash[:7]
        logger.log_info(f"commit_hash_prefix: {commit_hash_prefix}")

        integration_repo_commit_hash = (
            subprocess.run(
                [
                    "git",
                    "log",
                    "--grep",
                    commit_hash_prefix,
                    "--format=%H",
                ],
                cwd=integration_directory,
                check=True,
                capture_output=True,
            )
            .stdout.decode("utf-8")
            .strip()
            .split("\n")[-1]
        )
        logger.log_info(
            f"integration_repo_commit_hash: {integration_repo_commit_hash}"
        )

        if not integration_repo_commit_hash:
            logger.log_info(
                "Fuchsia commit is not rolled to integration repo yet. We will"
                "use the latest integration repo commit hash."
            )
            return

        # checkout the integration repo based on the fuchsia repo commit hash
        subprocess.run(
            [
                "git",
                "reset",
                "--hard",
                integration_repo_commit_hash,
            ],
            cwd=integration_directory,
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def create_symlinks(self) -> None:
        """Creates symlinks for the prebuilts."""
        logger.log_info("Creating symlinks for the prebuilts.")
        # Link the paths in the repo to cartfs
        (self.cartfs_fuchsia_dir / ".jiri_root" / "bin").mkdir(
            exist_ok=True, parents=True
        )
        (self.cartfs_fuchsia_dir / ".fx" / "config").mkdir(
            exist_ok=True, parents=True
        )
        for path in [
            "prebuilt",
            "build/cipd.gni",
            ".jiri_root/bin/ffx",
            ".jiri_root/bin/fuchsia-vendored-python",
            ".jiri_root/bin/hermetic-env",
            ".git",
            ".fx",
            ".fx-build-dir",
            "integration",
            "out",
        ]:
            repo_path = self.workspace_dir / self.repo_name / path
            cartfs_path = self.cartfs_fuchsia_dir / path
            logger.log_info(
                f"Creating symlink from {repo_path} to {cartfs_path}"
            )
            self.create_symlink(
                cartfs_path,
                repo_path,
            )

        # Symlink in the fx script.
        self.create_symlink(
            self.workspace_dir / self.repo_name / "scripts/cog/resources/fx",
            self.workspace_dir / self.repo_name / ".jiri_root/bin/fx",
        )

        # Symlink in CartFS specific GN arg overrides.
        self.create_symlink(
            self.cartfs_fuchsia_dir / "scripts/cog/resources/args.gn",
            self.cartfs_fuchsia_dir / "local/args.gn",
        )

        # Invoke git status in the fuchsia directory in the background. This
        # will make the future fx format-code command faster.
        subprocess.Popen(
            [
                "git",
                "status",
            ],
            cwd=self.workspace_dir / self.repo_name,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )

    def _patch_file(self, filepath: str, content: str) -> None:
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
