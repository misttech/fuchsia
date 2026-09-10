#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for android_misc_info.py."""

import pathlib
import unittest

import android_misc_info
import test_utils


class AndroidMiscInfoTests(unittest.TestCase):
    def test_single_prop(self) -> None:
        self.assertEqual(
            android_misc_info.process_misc_info("avb_foo_args=--prop foo:bar"),
            {"foo": "bar"},
        )

    def test_multiple_props(self) -> None:
        self.assertEqual(
            android_misc_info.process_misc_info(
                "avb_foo_args=--prop foo:bar --prop abc:123"
            ),
            {"foo": "bar", "abc": "123"},
        )

    def test_skip_unknown_args(self) -> None:
        self.assertEqual(
            android_misc_info.process_misc_info(
                "avb_foo_args=--arg1 --prop foo:bar --arg2=0 --prop abc:123 --arg3"
            ),
            {"foo": "bar", "abc": "123"},
        )

    def test_special_characters(self) -> None:
        self.assertEqual(
            android_misc_info.process_misc_info(
                "avb_foo_args=--prop foo.bar.baz:ABC/123:000.000"
            ),
            {"foo.bar.baz": "ABC/123:000.000"},
        )

    def test_extract_props_missing_value_fails(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            android_misc_info.process_misc_info(
                "avb_foo_args=--prop invalid_arg_no_colon"
            )
        self.assertIn("Invalid property format", str(ctx.exception))

    def test_extract_props_double_quoting_fails(self) -> None:
        with self.assertRaises(NotImplementedError) as ctx:
            android_misc_info.process_misc_info(
                'avb_foo_args=--prop "quotes not supported"'
            )
        self.assertIn("Unsupported meta-char", str(ctx.exception))

    def test_extract_props_single_quoting_fails(self) -> None:
        with self.assertRaises(NotImplementedError) as ctx:
            android_misc_info.process_misc_info(
                "avb_foo_args=--prop 'quotes not supported'"
            )
        self.assertIn("Unsupported meta-char", str(ctx.exception))

    def test_extract_props_escape_fails(self) -> None:
        with self.assertRaises(NotImplementedError) as ctx:
            android_misc_info.process_misc_info(
                "avb_foo_args=--prop escape\\character"
            )
        self.assertIn("Unsupported meta-char", str(ctx.exception))

    def test_multiple_commandlines(self) -> None:
        self.assertEqual(
            android_misc_info.process_misc_info(
                "avb_foo_args=--prop foo.version:123\n"
                "avb_bar_args=--prop bar.version:456"
            ),
            {"foo.version": "123", "bar.version": "456"},
        )

    def test_multiple_commandlines_property_overlap_fails(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            android_misc_info.process_misc_info(
                "avb_foo_args=--prop same_prop:123\n"
                "avb_bar_args=--prop same_prop:456"
            )
        self.assertIn("Duplicate properties", str(ctx.exception))

    def test_load_aosp_cf_misc_info_properties(self) -> None:
        """Tests a real misc_info.txt from an Android build."""
        content = test_utils.load_test_data(
            pathlib.Path("android_misc_info")
            / "misc_info.aosp_cf_arm64_only_phone-userdebug.16102939.txt"
        ).decode("utf-8")

        expected_props = {
            "com.android.build.boot.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.boot.os_version": "17",
            "com.android.build.boot.security_patch": "2026-06-05",
            "com.android.build.init_boot.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.init_boot.os_version": "17",
            "com.android.build.init_boot.security_patch": "2026-06-05",
            "com.android.build.odm.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.odm.os_version": "17",
            "com.android.build.odm_dlkm.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.odm_dlkm.os_version": "17",
            "com.android.build.product.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.product.os_version": "17",
            "com.android.build.product.security_patch": "2026-06-05",
            "com.android.build.recovery.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.system.fingerprint": (
                "Android/generic_system/generic:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.system.os_version": "17",
            "com.android.build.system.security_patch": "2026-06-05",
            "com.android.build.system_dlkm.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.system_dlkm.os_version": "17",
            "com.android.build.system_ext.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.system_ext.os_version": "17",
            "com.android.build.system_ext.security_patch": "2026-06-05",
            "com.android.build.vendor.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.vendor.os_version": "17",
            "com.android.build.vendor.security_patch": "2026-06-05",
            "com.android.build.vendor_boot.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.vendor_dlkm.fingerprint": (
                "generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/CP2A.260605.016/16102939:userdebug/test-keys"
            ),
            "com.android.build.vendor_dlkm.os_version": "17",
        }
        self.assertEqual(
            android_misc_info.process_misc_info(content), expected_props
        )


if __name__ == "__main__":
    unittest.main()
