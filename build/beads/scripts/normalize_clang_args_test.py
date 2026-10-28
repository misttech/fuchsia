# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import pathlib
import tempfile
import unittest

from build.beads.scripts import normalize_clang_args, path_normalizer


class MockPathNormalizer(path_normalizer.PathNormalizer):
    def normalize_path(self, path: str) -> str:
        if path.startswith("../../"):
            return f"{path[6:]}"
        return path


class NormalizeClangArgsTest(unittest.TestCase):
    def test_normalize_c_compile_flags(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "../../prebuilt/third_party/clang/linux-x64/bin/clang "
            "-MD -MF obj/foo/bar.d "
            "-I../../src/include "
            "-I ../../other/include "
            "-DDEBUG=1 -D_GNU_SOURCE "
            "-fcolor-diagnostics "
            "-c ../../src/foo.c "
            "-o obj/src/foo.o"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_C_COMPILE
        )
        self.assertIn("clang", normalized)
        self.assertIn("-Isrc/include", normalized)
        self.assertIn("-Iother/include", normalized)
        self.assertIn("-DDEBUG=1", normalized)
        self.assertIn("-D_GNU_SOURCE", normalized)
        self.assertIn("src/foo.c", normalized)

        # Output/dep/diagnostic flags should be omitted
        self.assertNotIn("-MD", normalized)
        self.assertNotIn("-fcolor-diagnostics", normalized)
        self.assertNotIn("obj/src/foo.o", normalized)

    def test_normalize_cpp_compile_flags(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "../../prebuilt/third_party/clang/linux-x64/bin/clang++ "
            "-std=c++20 "
            "-Wall -Wextra "
            "-O2 "
            "-isystem ../../prebuilt/include "
            "-c ../../src/main.cc "
            "-o obj/src/main.o"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_CPP_COMPILE
        )
        self.assertIn("clang++", normalized)
        self.assertIn("-std=c++20", normalized)
        self.assertIn("-Wall", normalized)
        self.assertIn("-Wextra", normalized)
        self.assertIn("-O2", normalized)
        self.assertIn("-isystemprebuilt/include", normalized)
        self.assertIn("src/main.cc", normalized)

    def test_normalize_assemble_flags(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "../../prebuilt/third_party/clang/linux-x64/bin/clang "
            "-D__ASSEMBLER__ "
            "-I../../src/arch/x64 "
            "-c ../../src/arch/x64/entry.S "
            "-o obj/src/arch/x64/entry.o"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_ASSEMBLE
        )
        self.assertIn("clang", normalized)
        self.assertIn("-D__ASSEMBLER__", normalized)
        self.assertIn("-Isrc/arch/x64", normalized)
        self.assertIn("src/arch/x64/entry.S", normalized)

    def test_normalize_cpp_link_flags(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "../../prebuilt/third_party/clang/linux-x64/bin/clang++ "
            "--driver-mode=g++ "
            "-Wl,--Map=out/bin/my_bin.map "
            "-Wl,--color-diagnostics "
            "-L../../prebuilt/lib "
            "-Wl,-L../../other/lib "
            "-lfoo -lbar "
            "obj/src/foo.o obj/src/bar.pic.o "
            "-o out/bin/my_bin"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_CPP_LINK
        )
        self.assertIn("clang++", normalized)
        self.assertIn("-Lprebuilt/lib", normalized)
        self.assertIn("-Lother/lib", normalized)
        self.assertIn("-lfoo", normalized)
        self.assertIn("-lbar", normalized)

        # Object files and map files should be omitted
        self.assertNotIn("obj/src/foo.o", normalized)
        self.assertNotIn("obj/src/bar.pic.o", normalized)
        self.assertNotIn("--driver-mode=g++", normalized)
        self.assertNotIn("-Wl,--color-diagnostics", normalized)

    def test_normalize_prefix_map_flags_with_gn_normalizer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fuchsia_dir = pathlib.Path(tmp) / "fuchsia"
            build_dir = fuchsia_dir / "out" / "default"
            fuchsia_dir.mkdir(parents=True)
            build_dir.mkdir(parents=True)

            gn_norm = path_normalizer.GnPathNormalizer(fuchsia_dir, build_dir)

            cmd_gn = (
                f"clang++ -ffile-prefix-map={fuchsia_dir}=../.. "
                f"-ffile-prefix-map={fuchsia_dir}/out=.. "
                f"-ffile-prefix-map={build_dir}=. "
                "-c ../../src/foo.cc -o obj/foo.o"
            )
            normalized_gn = normalize_clang_args.normalize_clang_cmd(
                cmd_gn,
                action_type=normalize_clang_args.ACTION_CPP_COMPILE,
                normalizer=gn_norm,
            )
            self.assertIn(
                "-ffile-prefix-map={SOURCE_ROOT}={SOURCE_ROOT}", normalized_gn
            )
            self.assertIn(
                "-ffile-prefix-map={OUT_ROOT}={OUT_ROOT}", normalized_gn
            )
            self.assertIn(
                "-ffile-prefix-map={BUILD_DIR}={BUILD_DIR}", normalized_gn
            )

            cmd_comp_dir = "clang++ -ffile-compilation-dir=. -c foo.cc -o foo.o"
            normalized_comp_dir = normalize_clang_args.normalize_clang_cmd(
                cmd_comp_dir,
                action_type=normalize_clang_args.ACTION_CPP_COMPILE,
                normalizer=gn_norm,
            )
            self.assertIn(
                "-ffile-compilation-dir={BUILD_DIR}", normalized_comp_dir
            )


if __name__ == "__main__":
    unittest.main()
