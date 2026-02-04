# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import base64
import json
import os
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path
from typing import Callable

import cartfs
import logger
import snapshotter


class WorkspaceError(Exception):
    """Base exception for Cartfs errors."""


class RepoSetupError(WorkspaceError):
    """Raised when there is an error setting up the repository."""


class NotInCogWorkspaceError(WorkspaceError):
    """Raised when the current directory is not within a Cog workspace."""


class CannotFindRepoNameError(WorkspaceError):
    """Raised when the repo name cannot be found."""


CARTFS_SYMLINK_NAME: str = "cartfs-dir"
COG_METADATA_FILE_NAME: str = ".cog.json"
LOCAL_JIRI_MANIFEST_CONTENT: str = """
<manifest>
  <imports>
    <localimport file="../integration/prebuilts"/>
    <localimport file="../integration/cobalt"/>
    <localimport file="manifests/prebuilts"/>
    <localimport file="manifests/third_party/all"/>
  </imports>
</manifest>
"""


class CogMetadata:
    """Represents the metadata stored in the .cog.json file."""

    def __init__(self, workspace_name: str, repo_name: str):
        """Initializes CogMetadata.

        Args:
            workspace_name: The name of the cog workspace.
            repo_name: The name of the repository within the workspace.
        """
        self.workspace_name = workspace_name
        self.repo_name = repo_name

    def to_dict(self) -> dict[str, str]:
        """Returns a dictionary representation of the metadata."""
        return {
            "workspace_name": self.workspace_name,
            "repo_name": self.repo_name,
        }

    @classmethod
    def from_file(cls, path: Path) -> "CogMetadata | None":
        """Loads metadata from a .cog.json file.

        Args:
            path: The full path to the .cog.json file.

        Returns:
            A CogMetadata instance if the file is valid, otherwise None.
        """
        if not path.exists():
            return None
        try:
            data = json.loads(path.read_text())
            return cls(
                workspace_name=data["workspace_name"],
                repo_name=data["repo_name"],
            )
        except (
            OSError,
            json.JSONDecodeError,
            KeyError,
        ) as e:
            logger.log_warn(f"Warning: Could not read or parse {path}: {e}")
            return None

    def write(self, directory: Path) -> None:
        """Writes the metadata to a JSON file in the given directory."""
        path = directory / COG_METADATA_FILE_NAME
        path.write_text(json.dumps(self.to_dict(), indent=4))


class Workspace:
    """A class to encapsulate a Cog workspace and an associated Cartfs workspace."""

    def __init__(
        self,
        workspace_dir: Path,
        repo_name: str,
        workspace_name: str,
        cartfs_directory: Path | None,
        cartfs_instance: cartfs.Cartfs,
    ):
        """Initializes a Workspace instance.

        Note: This constructor should not be called directly. Instead, use the
        `create` class method to create an instance.
        """
        self.workspace_dir = workspace_dir
        self.repo_name = repo_name
        self.workspace_name = workspace_name
        self.cartfs_directory = cartfs_directory
        self.cartfs_instance = cartfs_instance
        self.cartfs_mount_point = cartfs_instance.mount_point

    @staticmethod
    def _find_cog_workspace_directory(
        start_dir: Path,
    ) -> Path | None:
        """Finds the root cog workspace directory by traversing up from a start path.

        This function looks for a directory which contains a .citc directory.

        Args:
            start_dir: The directory to start searching upwards from.
            cog_mount_point: The base path for cog workspaces.

        Returns:
            The absolute path to the workspace directory if found, otherwise None.
        """
        for ancestor in [start_dir] + list(start_dir.parents):
            if (ancestor / ".citc").is_dir():
                return ancestor
        return None

    @classmethod
    def create(
        cls,
        use_local_mock_cartfs: bool = False,
        repo_root: Path | None = None,
    ) -> "Workspace":
        """Creates a Workspace instance after verifying its state.

        Raises:
            NotInCogWorkspaceError: If the current directory is not within a Cog workspace.
                cannot be found.
            CannotFindRepoNameError: If the repo name cannot be found.
            CartfsError: If cartfs is not installed or running.

        Returns:
            A new Workspace instance.
        """
        current_dir = Path(repo_root) if repo_root else Path.cwd()

        workspace_dir = cls._find_cog_workspace_directory(current_dir)
        if not workspace_dir:
            raise NotInCogWorkspaceError(
                f"Current directory is not within a Cog workspace: {current_dir}"
            )

        # Note: this will raise a CartfsError if cartfs is not installed or
        # running.
        cartfs_instance = cartfs.Cartfs.create(use_local_mock_cartfs)

        repo_name = cls._get_repo_name_from_path(workspace_dir, current_dir)
        if not repo_name:
            raise CannotFindRepoNameError(
                "Could not find repo name from the path."
            )

        workspace_name = workspace_dir.name

        cartfs_directory = cls.get_linked_cartfs_workspace_directory(
            workspace_dir, repo_name
        )

        return cls(
            workspace_dir,
            repo_name,
            workspace_name,
            cartfs_directory,
            cartfs_instance,
        )

    @staticmethod
    def _get_repo_name_from_path(workspace_dir: Path, path: Path) -> str | None:
        """Finds the repo name from a given path.

        The repo name is the first path element that is common between the
        workspace directory and the given path, after the workspace directory.

        Args:
            path: The path to find the repo name from.

        Returns:
            The repo name if found, None otherwise.
        """
        if not path.is_relative_to(workspace_dir):
            return None

        relative_path = path.relative_to(workspace_dir)

        if relative_path == Path("."):
            return None

        return relative_path.parts[0]

    @staticmethod
    def get_linked_cartfs_workspace_directory(
        workspace_dir: Path, repo_name: str
    ) -> Path | None:
        """Gets the linked cartfs directory for a specific repo in a cog workspace.

        A workspace is considered linked if a symlink named `cartfs-dir` exists
        inside the specified repository directory, pointing to a valid cartfs
        directory. This target cartfs directory must contain a `.cog.json` file
        with a matching `repo_name` and `workspace_name`.

        Args:
            workspace_dir: The absolute path to the cog workspace.
            repo_name: The name of the repository within the workspace.

        Returns:
            The absolute path to the linked cartfs directory if found and valid,
            otherwise None.
        """
        repo_dir = workspace_dir / repo_name
        symlink_path = repo_dir / CARTFS_SYMLINK_NAME
        if not symlink_path.is_symlink():
            logger.log_info(f"symlink_path is not a link: {symlink_path}")
            return None

        target_path = symlink_path.readlink()
        if not target_path.is_absolute():
            # Handles relative symlinks. The target is relative to the directory
            # containing the symlink.
            target_path = repo_dir / target_path

        if not target_path.is_dir():
            return None

        metadata = CogMetadata.from_file(target_path / COG_METADATA_FILE_NAME)
        if not metadata:
            return None

        if (
            metadata.repo_name != repo_name
            or metadata.workspace_name != workspace_dir.name
        ):
            return None

        return target_path

    def snapshot_from_previous_instance(
        self,
        snapshot_function: Callable[
            [Path, Path, Path], None
        ] = snapshotter.snapshot_workspace,
    ) -> Path | None:
        """Snapshots the workspace from the most recent cartfs directory."""
        previous_cartfs_instance_rel_path = self._find_previous_instance()
        if not previous_cartfs_instance_rel_path:
            return None

        suggested_directory_name = (
            self.cartfs_instance.suggest_cartfs_directory_name(
                self.workspace_name
            )
        )
        try:
            snapshot_function(
                previous_cartfs_instance_rel_path,
                suggested_directory_name,
                self.cartfs_instance.mount_point,
            )
        except ValueError as e:
            logger.log_error(f"Error during snapshotting: {e}")
            return None

        return self.cartfs_instance.mount_point / suggested_directory_name

    def create_empty_cartfs_workspace_directory(self) -> Path:
        """Creates a new, empty directory in the cartfs mount for this workspace.

        This method generates a unique directory name based on the workspace name,
        creates the directory, and writes a `.cog.json` metadata file into it.

        Returns:
            The absolute path to the newly created cartfs directory.
        """
        suggested_directory_name = (
            self.cartfs_instance.suggest_cartfs_directory_name(
                self.workspace_name
            )
        )
        cartfs_directory = (
            self.cartfs_instance.mount_point / suggested_directory_name
        )
        # It is ok to use exist_ok here because the suggested directory name
        # is generated by cartfs, and there should not be a directory with
        # the same name in the cartfs mount point.
        cartfs_directory.mkdir(exist_ok=True)

        # Write the metadata file in cartfs
        metadata = CogMetadata(
            workspace_name=self.workspace_name, repo_name=self.repo_name
        )
        metadata.write(cartfs_directory)

        return cartfs_directory

    def link_to_cartfs(self, cartfs_directory: Path) -> None:
        """Links the cog workspace to a cartfs directory.

        This creates a symlink named `cartfs-dir` inside the repository
        directory of the cog workspace. This symlink points to the specified
        cartfs directory, establishing the link between them. If a symlink
        already exists, it will be replaced.

        Additionally, it writes a `.cog.json` metadata file into the cartfs
        directory.

        Args:
            cartfs_directory: The absolute path to the target cartfs directory.
        """

        symlink_path = self.workspace_dir / self.repo_name / CARTFS_SYMLINK_NAME

        # Create an absolute symlink from the repo directory to the cartfs
        # workspace directory. If a symlink already exists, remove it first.
        if symlink_path.is_symlink():
            symlink_path.unlink()
        symlink_path.symlink_to(cartfs_directory)

        metadata = CogMetadata(
            workspace_name=self.workspace_name, repo_name=self.repo_name
        )
        metadata.write(cartfs_directory)

        self.cartfs_directory = cartfs_directory

    def _find_previous_instance(self) -> Path | None:
        """Finds the most recent cartfs directory for the same repo.

        This method iterates through all directories in the cartfs mount point,
        looking for directories that are linked to a workspace with the same repo
        name as the current one. It then returns the path to the one with the
        most recent modification time.

        Returns:
            The path, relative to the cartfs mount point, to the newest
            directory found, or None if no instances are found.
        """
        mount_point = Path(self.cartfs_instance.mount_point)
        if not mount_point or not mount_point.is_dir():
            return None

        candidates = set()
        for entry in mount_point.iterdir():
            if not entry.is_dir():
                continue

            metadata = CogMetadata.from_file(entry / COG_METADATA_FILE_NAME)
            if not metadata:
                continue

            repo_name = metadata.repo_name

            # Check if it's for the same repo.
            if repo_name != self.repo_name:
                continue

            candidates.add(entry)

        newest_candidate = None
        newest_mtime = -1.0
        for candidate in candidates:
            try:
                mtime = candidate.stat().st_mtime
                if mtime > newest_mtime:
                    newest_mtime = mtime
                    newest_candidate = candidate
            except FileNotFoundError:
                # The directory was deleted between listing and stat-ing.
                continue
        return (
            newest_candidate.relative_to(mount_point)
            if newest_candidate
            else None
        )

    def initialize_cartfs_directory(self) -> None:
        """Initializes the cartfs directory for this workspace."""
        if not self.cartfs_directory:
            raise RepoSetupError("No cartfs directory found.")

        self.cartfs_fuchsia_dir = self.cartfs_directory / "fuchsia"
        if not self._is_jiri_bootstrapped():
            self._bootstrap_jiri()

        integration_hash = self._create_integration_repository()
        self._fetch_prebuilts(integration_hash)
        self._create_symlinks()

    def _create_symlink(self, target: Path, link_name: Path) -> None:
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

        logger.log_info(f"Creating symlink from {link_name} to {target}")
        link_name.symlink_to(target)

    def _run_bootstrap_jiri_script(self) -> None:
        """Runs the bootstrap jiri script."""
        url = "https://fuchsia.googlesource.com/jiri/+/HEAD/scripts/bootstrap_jiri?format=TEXT"
        try:
            with urllib.request.urlopen(url) as response:
                encoded_script = response.read()
                decoded_script = base64.b64decode(encoded_script)
                subprocess.run(
                    ["bash", "-s", self.cartfs_mount_point],
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
        (self.cartfs_fuchsia_dir / ".jiri_root" / "bin").mkdir(
            exist_ok=True, parents=True
        )
        self._create_symlink(
            self.cartfs_mount_point / ".jiri_root" / "bin" / "jiri",
            self.cartfs_fuchsia_dir / ".jiri_root" / "bin" / "jiri",
        )
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

    def _is_jiri_bootstrapped(self) -> bool:
        """Checks if jiri is bootstrapped."""
        return (
            self.cartfs_mount_point / ".jiri_root" / "bin" / "jiri"
        ).exists()

    def _bootstrap_jiri(self) -> None:
        """Bootstraps jiri if it is not already bootstrapped."""
        logger.log_info("Bootstrapping jiri.")
        self._run_bootstrap_jiri_script()

    def _fetch_prebuilts(self, current_integration_hash: str) -> None:
        """Fetches prebuilts for the given repo."""
        if not self.cartfs_directory:
            raise RepoSetupError("No cartfs directory found.")

        integration_hash_file = (
            self.cartfs_directory / ".integration_commit_hash"
        )
        if integration_hash_file.exists():
            former_integration_hash = integration_hash_file.read_text().strip()
            if former_integration_hash == current_integration_hash:
                logger.log_info(
                    f"Integration repo not changed, skip fetching prebuilts."
                )
                return

        logger.log_info(f"Fetching prebuilts for {self.repo_name}.")
        cartfs_fuchsia_dir = self.cartfs_fuchsia_dir
        if (cartfs_fuchsia_dir / ".git").exists():
            subprocess.run(
                ["git", "add", "."],
                cwd=cartfs_fuchsia_dir,
                check=True,
            )
            subprocess.run(
                ["git", "reset", "--hard"],
                cwd=cartfs_fuchsia_dir,
                check=True,
                stdout=subprocess.DEVNULL,
            )

        # Run jiri update and fetch-packages in parallel to speed up the
        # process.
        update_process = subprocess.Popen(
            [".jiri_root/bin/jiri", "update", "--fetch-packages=false"],
            cwd=cartfs_fuchsia_dir,
        )
        fetch_process = subprocess.Popen(
            [".jiri_root/bin/jiri", "fetch-packages"],
            cwd=cartfs_fuchsia_dir,
        )
        update_process.wait()
        fetch_process.wait()

        # Record integration repo commit hash in the fuchsia repo.
        integration_hash_file.write_text(current_integration_hash)

    def _create_integration_repository(self) -> str:
        """Creates the integration repository."""
        logger.log_info("Setup the integration repository.")
        if not self.cartfs_directory:
            raise RepoSetupError("No cartfs directory found.")

        integration_directory = self.cartfs_directory / "integration"

        # Setup the integration repository
        if integration_directory.exists():
            if not (integration_directory / ".git").exists():
                shutil.rmtree(integration_directory, ignore_errors=True)
            else:
                # Pull latest changes from the remote
                subprocess.run(
                    [
                        "git",
                        "pull",
                        "origin",
                        "main",
                        "--depth=100",
                    ],
                    cwd=integration_directory,
                    check=True,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )

        # Clone the integration repository
        if not integration_directory.exists():
            subprocess.run(
                [
                    "git",
                    "clone",
                    "https://fuchsia.googlesource.com/integration",
                    "--depth=100",
                ],
                cwd=self.cartfs_directory,
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
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
                cwd=self.workspace_dir / self.repo_name,
                check=True,
                capture_output=True,
            )
            .stdout.decode("utf-8")
            .strip()
            .split("\n")
        )
        fuchsia_repo_commit_hash = None
        for state in fuchsia_repo_states:
            if "base_commit_hash" in state:
                fuchsia_repo_commit_hash = (
                    state.split(":")[1].strip().replace('"', "")
                )
                break
        if not fuchsia_repo_commit_hash:
            logger.log_error("Failed to get fuchsia repo commit hash.")
            raise RepoSetupError("Failed to get fuchsia repo commit hash.")

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
                "Fuchsia commit is not rolled to integration repo yet. We will "
                "use the latest integration repo commit hash."
            )
        else:
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

        # clone fuchsia repository and reset it to the commit hash
        logger.log_info("Setup the fuchsia repository.")
        if not self.cartfs_fuchsia_dir.exists():
            subprocess.run(
                [
                    "git",
                    "clone",
                    "https://fuchsia.googlesource.com/fuchsia",
                    f"--revision={fuchsia_repo_commit_hash}",
                    "--depth=100",
                ],
                cwd=self.cartfs_directory,
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            subprocess.run(
                [
                    "git",
                    "fetch",
                    "origin",
                    "main:refs/remotes/origin/main",
                    "--depth=100",
                ],
                cwd=self.cartfs_fuchsia_dir,
                check=True,
            )
        else:
            subprocess.run(
                ["git", "reset", "--hard"],
                cwd=self.cartfs_fuchsia_dir,
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            subprocess.run(
                [
                    "git",
                    "fetch",
                    "origin",
                    "main",
                    "--depth=100",
                ],
                cwd=self.cartfs_fuchsia_dir,
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            logger.log_info(
                f"Resetting fuchsia repository to {fuchsia_repo_commit_hash}."
            )
            subprocess.run(
                [
                    "git",
                    "reset",
                    "--hard",
                    fuchsia_repo_commit_hash,
                ],
                cwd=self.cartfs_fuchsia_dir,
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )

        # Couple of places in the build expect to find JIRI_HEAD for fuchsia
        # repository. In a normal checkout by jiri, the JIRI_HEAD is created
        # automatically.
        # https://fuchsia.googlesource.com/jiri/+/refs/heads/main/project/project.go#689
        # For our cartfs checkout, we need to create it manually.
        shutil.copyfile(
            self.cartfs_fuchsia_dir / ".git" / "HEAD",
            self.cartfs_fuchsia_dir / ".git" / "JIRI_HEAD",
        )

        self._write_jiri_manifest()
        self._write_jiri_config()

        return integration_repo_commit_hash

    def _create_symlinks(self) -> None:
        """Creates symlinks for the prebuilts."""
        # Link the paths in the repo to cartfs
        (self.cartfs_fuchsia_dir / ".fx" / "config").mkdir(
            exist_ok=True, parents=True
        )
        for path in [
            "prebuilt",
            "build/cipd.gni",
            ".jiri_manifest",
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
            self._create_symlink(
                cartfs_path,
                repo_path,
            )

        # Symlink in the fx script.
        self._create_symlink(
            self.workspace_dir / self.repo_name / "scripts/cog/resources/fx",
            self.workspace_dir / self.repo_name / ".jiri_root/bin/fx",
        )

        # Symlink in CartFS specific GN arg overrides.
        self._create_symlink(
            self.cartfs_fuchsia_dir / "scripts/cog/resources/args.gn",
            self.cartfs_fuchsia_dir / "local/args.gn",
        )

        if not self.cartfs_directory:
            raise RepoSetupError("No cartfs directory found.")

        # Symlink in CartFS specific integration directory.
        self._create_symlink(
            self.cartfs_directory / "integration",
            self.cartfs_fuchsia_dir / "integration",
        )

        # Manually execute jiri hooks. The hooks are defined in
        # https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/manifests/platform#14
        # and are automatically executed by jiri during `jiri update`. Since we
        # are not using `jiri update`, we need to execute them manually.
        hooks = [
            "scripts/devshell/lib/add_symlink_to_bin.sh",
            "sdk/ctf/build/internal/create_ctf_releases_gni.sh",
            "build/info/create_jiri_hook_files.sh",
            "tools/build/scripts/generate_prebuilt_versions.sh",
            "tools/build/scripts/extract_protobuf_py3_wheel.sh",
        ]
        # Set FUCHSIA_DIR environment variable to the cartfs fuchsia directory.
        # This is needed for the hooks to work correctly.
        my_env = os.environ.copy()
        my_env["FUCHSIA_DIR"] = str(self.cartfs_fuchsia_dir)
        for hook in hooks:
            logger.log_info(f"Running hook: {hook}")
            subprocess.run(
                [
                    hook,
                ],
                cwd=self.cartfs_fuchsia_dir,
                check=True,
                stdout=subprocess.DEVNULL,
                env=my_env,
            )

        # Invoke git status in the fuchsia directory in the background. This
        # will make the future `fx format-code` command faster.
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
        if not self.cartfs_directory:
            raise RepoSetupError("No cartfs directory found.")

        logger.log_info(f"Patching the {filepath} file.")
        full_filepath = self.cartfs_directory / filepath
        if full_filepath.exists():
            logger.log_info(f"File {full_filepath} already exists.")
            return

        full_filepath.parent.mkdir(parents=True, exist_ok=True)
        try:
            full_filepath.write_text(content)
        except Exception as e:
            logger.log_error(f"An error occurred while writing the file: {e}")
