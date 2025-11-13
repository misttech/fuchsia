# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os

from base import Benchmark


class SampleBenchmark(Benchmark):
    """A sample benchmark that creates a directory and writes a file."""

    def __init__(self) -> None:
        super().__init__(
            name="sample",
            description="Creates a directory and writes a file.",
            compare=["sample2", "other-sample"],
        )

    def run(self) -> None:
        """Runs the benchmark."""
        assert self.temp_dir, "temp_dir not set"
        dir_path = os.path.join(self.temp_dir, "sample_dir")
        os.makedirs(dir_path)
        file_path = os.path.join(dir_path, "sample_file.txt")
        with open(file_path, "w") as f:
            f.write("hello world")


class OtherSampleBenchmark(Benchmark):
    """A sample benchmark that creates a directory and writes a file."""

    def __init__(self) -> None:
        super().__init__(
            name="other-sample",
            description="Creates a directory and writes a file.",
        )

    def run(self) -> None:
        """Runs the benchmark."""
        assert False, "this benchmark always fails"
