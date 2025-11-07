# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import base64
import os
import shutil
import subprocess
import sys
import urllib.request

LOCAL_JIRI_MANIFEST_CONTENT = """
<manifest>
  <imports>
    <localimport file="manifests/platform"/>
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
        self.cartfs_directory = cartfs_directory
        self.workspace_dir = workspace_dir
        self.workspace_name = workspace_name
        self.repo_name = repo_name

    def create_symlink(self, target: str, link_name: str) -> None:
        """Creates a symlink from link_name to target.

        If a symlink already exists at link_name and points to target, this
        function does nothing.

        If a file, directory, or a different symlink exists at link_name, it will
        be removed and replaced with the new symlink.
        """
        if os.path.lexists(link_name):
            if os.path.islink(link_name) and os.readlink(link_name) == target:
                return

            # If the path exists but is not the desired symlink, remove it.
            if os.path.isdir(link_name) and not os.path.islink(link_name):
                shutil.rmtree(link_name)
            else:
                os.remove(link_name)

        os.symlink(target, link_name)

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
            print(f"Failed to bootstrap jiri: {e}")
            sys.exit(1)

    def _write_jiri_manifest(self) -> None:
        """Writes the jiri manifest."""
        print("Writing jiri manifest.")
        jiri_manifest = os.path.join(self.cartfs_directory, ".jiri_manifest")
        try:
            with open(jiri_manifest, "w") as f:
                f.write(LOCAL_JIRI_MANIFEST_CONTENT)
        except Exception as e:
            print(f"An error occurred while writing jiri manifest file: {e}")

        print("Copy manifests directory to CartFS.")
        shutil.copytree(
            os.path.join(self.workspace_dir, self.repo_name, "manifests"),
            os.path.join(self.cartfs_directory, "manifests"),
            dirs_exist_ok=True,
        )

    def is_jiri_bootstrapped(self) -> bool:
        """Checks if jiri is bootstrapped."""
        jiri_root = os.path.join(self.cartfs_directory, ".jiri_root")
        jiri_manifest = os.path.join(self.cartfs_directory, ".jiri_manifest")
        return os.path.isdir(jiri_root) and os.path.exists(jiri_manifest)

    def bootstrap_jiri(self) -> None:
        """Bootstraps jiri if it is not already bootstrapped."""
        print("Bootstrapping jiri.")
        self._run_bootstrap_jiri_script()
        self._write_jiri_manifest()

    def fetch_prebuilts(self) -> None:
        """Fetches prebuilts for the given repo."""
        self._patch_build_cipd_gni_file()
        print(f"Fetching prebuilts for {self.repo_name}.")
        subprocess.run(
            [".jiri_root/bin/jiri", "fetch-packages"],
            cwd=self.cartfs_directory,
            check=True,
        )

    def create_symlinks(self) -> None:
        """Creates symlinks for the prebuilts."""
        print("Creating symlinks for the prebuilts.")
        # Link the paths in the repo to cartfs
        for path in ["prebuilt", ".jiri_root", ".jiri_manifest", ".cipd"]:
            repo_path = os.path.join(self.workspace_dir, self.repo_name, path)
            cartfs_path = os.path.join(self.cartfs_directory, path)
            print(f"Creating symlink from {repo_path} to {cartfs_path}")
            self.create_symlink(
                cartfs_path,
                repo_path,
            )

    def _patch_build_cipd_gni_file(self) -> None:
        """Patches the build/cipd.gni file to set internal_access to false."""
        print("Patching the build/cipd.gni file.")
        cipd_gni_file = os.path.join(self.cartfs_directory, "build", "cipd.gni")
        if not os.path.exists(cipd_gni_file):
            print(f"File {cipd_gni_file} does not exist. Creating it now.")
            parent_dir = os.path.dirname(cipd_gni_file)
            os.makedirs(parent_dir, exist_ok=True)
            try:
                with open(cipd_gni_file, "w") as f:
                    f.write("internal_access = false")
            except Exception as e:
                print(f"An error occurred while writing the file: {e}")
        else:
            print(f"File {cipd_gni_file} already exists.")

        # Now symlink the workspace cipd.gni to the cartfs cipd.gni
        self.create_symlink(
            cipd_gni_file,
            os.path.join(
                self.workspace_dir, self.repo_name, "build", "cipd.gni"
            ),
        )
