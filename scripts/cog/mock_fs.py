# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import shutil
import tempfile
from enum import Enum
from pathlib import Path
from types import TracebackType
from typing import Optional, Type


class FSType(Enum):
    CARTFS = 1
    COG = 2


class FileSystemTestHelper:
    def __init__(self) -> None:
        self.temp_dir = Path(tempfile.mkdtemp())
        self.cartfs_dir = self.temp_dir / "cartfs"
        self.cog_dir = self.temp_dir / "cog"
        self.cartfs_dir.mkdir()
        self.cog_dir.mkdir()

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

    def _get_dir(self, fs_type: FSType) -> Path:
        if fs_type == FSType.CARTFS:
            return self.cartfs_dir
        return self.cog_dir

    def full_path(self, path: str | Path, fs_type: FSType) -> Path:
        return self._get_dir(fs_type) / Path(path)

    def mkdir(self, path: str | Path, fs_type: FSType) -> Path:
        """Creates a directory in the specified file system."""
        full_path = self.full_path(path, fs_type)
        full_path.mkdir(exist_ok=True, parents=True)
        return full_path

    def write(self, path: str | Path, fs_type: FSType, content: str) -> None:
        full_path = self.full_path(path, fs_type)
        full_path.parent.mkdir(exist_ok=True)
        full_path.write_text(content)

    def read(self, path: str | Path, fs_type: FSType) -> str:
        full_path = self.full_path(path, fs_type)
        return full_path.read_text()

    def delete(self, path: str | Path, fs_type: FSType) -> None:
        full_path = self.full_path(path, fs_type)
        full_path.unlink()

    def symlink_from_cog_to_cartfs(
        self, link_name: str | Path, cartfs_dir: str | None = None
    ) -> None:
        if cartfs_dir:
            target = self.full_path(cartfs_dir, FSType.CARTFS)
        else:
            target = self.cartfs_dir
        link = self.full_path(link_name, FSType.COG)
        link.symlink_to(target)

    def symlink_from_cartfs_to_cog(
        self, link_name: str | Path, cog_dir: str | None = None
    ) -> None:
        if cog_dir:
            target = self.full_path(cog_dir, FSType.COG)
        else:
            target = self.cog_dir
        link = self.full_path(link_name, FSType.CARTFS)
        link.symlink_to(target)
