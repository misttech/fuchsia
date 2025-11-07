# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import shutil
import tempfile
from enum import Enum
from types import TracebackType
from typing import Optional, Type


class FSType(Enum):
    CARTFS = 1
    COG = 2


class FileSystemTestHelper:
    def __init__(self) -> None:
        self.temp_dir = tempfile.mkdtemp()
        self.cartfs_dir = os.path.join(self.temp_dir, "cartfs")
        self.cog_dir = os.path.join(self.temp_dir, "cog")
        os.makedirs(self.cartfs_dir)
        os.makedirs(self.cog_dir)

    def __enter__(self) -> "FileSystemTestHelper":
        return self

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_val: Optional[BaseException],
        exc_tb: Optional[TracebackType],
    ) -> None:
        self.cleanup()

    def cleanup(self) -> None:
        shutil.rmtree(self.temp_dir)

    def _get_dir(self, fs_type: FSType) -> str:
        if fs_type == FSType.CARTFS:
            return self.cartfs_dir
        return self.cog_dir

    def full_path(self, path: str, fs_type: FSType) -> str:
        return os.path.join(self._get_dir(fs_type), path.lstrip("/"))

    def mkdir(self, path: str, fs_type: FSType) -> str:
        """Creates a directory in the specified file system."""
        full_path = self.full_path(path, fs_type)
        os.makedirs(full_path, exist_ok=True)
        return full_path

    def write(self, path: str, fs_type: FSType, content: str) -> None:
        full_path = self.full_path(path, fs_type)
        os.makedirs(os.path.dirname(full_path), exist_ok=True)
        with open(full_path, "w") as f:
            f.write(content)

    def read(self, path: str, fs_type: FSType) -> str:
        full_path = self.full_path(path, fs_type)
        with open(full_path, "r") as f:
            return f.read()

    def delete(self, path: str, fs_type: FSType) -> None:
        full_path = self.full_path(path, fs_type)
        os.remove(full_path)

    def symlink_from_cog_to_cartfs(
        self, link_name: str, cartfs_dir: str | None = None
    ) -> None:
        if cartfs_dir:
            target = self.full_path(cartfs_dir, FSType.CARTFS)
        else:
            target = self.cartfs_dir
        link = self.full_path(link_name, FSType.COG)
        os.symlink(target, link)

    def symlink_from_cartfs_to_cog(
        self, link_name: str, cog_dir: str | None = None
    ) -> None:
        if cog_dir:
            target = self.full_path(cog_dir, FSType.COG)
        else:
            target = self.cog_dir
        link = self.full_path(link_name, FSType.CARTFS)
        os.symlink(target, link)
