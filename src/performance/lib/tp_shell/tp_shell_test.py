# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import importlib.resources
import os
import time
import unittest
import unittest.mock

from tp_shell import FuchsiaPlatformDelegate, PerfettoTraceProcessor


class TpShellTest(unittest.TestCase):
    def test_single_query_and_cleanup(self) -> None:
        """Tests that a single query works and the background process is cleaned up."""
        source = importlib.resources.files("test_data").joinpath(
            "perfetto_golden.fxt"
        )
        with importlib.resources.as_file(source) as trace_path:
            with PerfettoTraceProcessor(str(trace_path)) as tp:
                # Run a query.
                results = tp.run_query("SELECT count(*) as cnt FROM slice")
                self.assertEqual(len(results), 1)
                self.assertGreater(results[0]["cnt"], 0)

                tables = tp.get_tables()
                self.assertIn("slice", tables)

                # Check that process is alive inside the context.
                self.assertTrue(tp._finalizer.alive)

            # Check that process is cleaned up after exiting context.
            self.assertFalse(tp._finalizer.alive)
            with self.assertRaises(RuntimeError):
                tp.run_query("SELECT count(*) as cnt FROM slice")

    def test_multiple_queries_are_fast(self) -> None:
        """Tests that running multiple queries is fast, proving a persistent connection."""
        source = importlib.resources.files("test_data").joinpath(
            "perfetto_golden.fxt"
        )
        with importlib.resources.as_file(source) as trace_path:
            with PerfettoTraceProcessor(str(trace_path)) as tp:
                # Warmup query
                tp.run_query("SELECT count(*) as cnt FROM slice")

                # Measure 3 queries
                start_time = time.time()
                for _ in range(3):
                    tp.run_query("SELECT count(*) as cnt FROM slice")
                elapsed = time.time() - start_time

                # Spawning a new trace_processor_shell subprocess and loading the
                # perfetto_golden.fxt and performning one query would take under 500ms.
                # In this test we are doing 4 queries, and the last 3 should be more or less "free"
                # since they do not re-parse and load the trace file.
                self.assertLess(
                    elapsed,
                    0.5,
                    f"Expected queries to be fast, took {elapsed:.3f}s",
                )

    def test_duration_events_over_50ms(self) -> None:
        """Tests extracting duration events > 50ms."""
        source = importlib.resources.files("test_data").joinpath(
            "perfetto_golden.fxt"
        )
        with importlib.resources.as_file(source) as trace_path:
            with PerfettoTraceProcessor(str(trace_path)) as tp:
                # 50ms = 50,000,000 ns
                results = tp.run_query(
                    "SELECT name, dur FROM slice WHERE dur > 50000000 ORDER BY ts"
                )
                self.assertEqual(len(results), 20)
                event_names = [r["name"] for r in results]
                expected_names = ["example_duration"] * 20
                self.assertEqual(event_names, expected_names)

    def test_query_rows_types_and_columns(self) -> None:
        """Tests that query_rows returns typed tuples and column names lazily."""
        source = importlib.resources.files("test_data").joinpath(
            "perfetto_golden.fxt"
        )
        with importlib.resources.as_file(source) as trace_path:
            with PerfettoTraceProcessor(str(trace_path)) as tp:
                cols, rows = tp.query_rows(
                    "SELECT 42 AS int_col, 3.14 AS float_col, 'hello' AS str_col, NULL AS null_col"
                )
                self.assertEqual(
                    cols, ("int_col", "float_col", "str_col", "null_col")
                )
                row_list = list(rows)
                self.assertEqual(len(row_list), 1)
                first_row = row_list[0]
                self.assertEqual(first_row, (42, 3.14, "hello", None))
                self.assertIsInstance(first_row[0], int)
                self.assertIsInstance(first_row[1], float)
                self.assertIsInstance(first_row[2], str)
                self.assertIsNone(first_row[3])

    @unittest.mock.patch("urllib.request.urlopen")
    def test_http_uri_resolver_timeout(
        self, mock_urlopen: unittest.mock.MagicMock
    ) -> None:
        """Tests that HttpUriResolver.resolve() calls urlopen with a timeout."""
        from tp_shell.tp_utils import HttpUriResolver

        mock_response = unittest.mock.MagicMock()
        mock_response.read.side_effect = [b"data", b""]
        mock_urlopen.return_value = mock_response

        resolver = HttpUriResolver("http://example.com/trace.fxt")
        resolver.resolve()

        mock_urlopen.assert_called_once()
        _, kwargs = mock_urlopen.call_args
        self.assertEqual(kwargs.get("timeout"), 120)

    @unittest.mock.patch("urllib.request.urlopen")
    @unittest.mock.patch("tp_shell.tp_utils.resolve_trace_url")
    def test_http_uri_resolver_permalink(
        self,
        mock_resolve_trace_url: unittest.mock.MagicMock,
        mock_urlopen: unittest.mock.MagicMock,
    ) -> None:
        """Tests that HttpUriResolver resolves perfetto.dev permalinks."""
        from tp_shell.tp_utils import HttpUriResolver

        mock_resolve_trace_url.return_value = (
            "http://example.com/resolved_trace.fxt"
        )

        mock_response = unittest.mock.MagicMock()
        mock_response.read.side_effect = [b"data", b""]
        mock_urlopen.return_value = mock_response

        resolver = HttpUriResolver("https://ui.perfetto.dev/#!/?s=123456789")
        resolver.resolve()

        mock_resolve_trace_url.assert_called_once_with(
            "https://ui.perfetto.dev/#!/?s=123456789"
        )
        mock_urlopen.assert_called_once_with(
            "http://example.com/resolved_trace.fxt",
            context=unittest.mock.ANY,
            timeout=120,
        )

    @unittest.mock.patch("urllib.request.urlopen")
    @unittest.mock.patch("tp_shell.tp_utils.resolve_trace_url")
    def test_trace_caching_and_reuse(
        self,
        mock_resolve_trace_url: unittest.mock.MagicMock,
        mock_urlopen: unittest.mock.MagicMock,
    ) -> None:
        """Tests downloading, caching, and cache-hit reuse of trace URLs."""
        mock_resolve_trace_url.return_value = (
            "http://example.com/resolved_permalink.fxt"
        )
        mock_response = unittest.mock.MagicMock()
        mock_response.__enter__.return_value = mock_response
        mock_response.read.side_effect = [b"test_trace_data", b""]
        mock_urlopen.return_value = mock_response

        url = "https://ui.perfetto.dev/#!/?s=cache_test_permalink_123"
        registry = FuchsiaPlatformDelegate("").default_resolver_registry()
        cached_path = PerfettoTraceProcessor._resolve_and_cache_url(
            url, registry
        )

        try:
            self.assertTrue(os.path.exists(cached_path))
            with open(cached_path, "rb") as f:
                self.assertEqual(f.read(), b"test_trace_data")
            self.assertEqual(mock_urlopen.call_count, 1)

            # Second call should hit the cache and not invoke urlopen again
            cached_path_2 = PerfettoTraceProcessor._resolve_and_cache_url(
                url, registry
            )
            self.assertEqual(cached_path, cached_path_2)
            self.assertEqual(mock_urlopen.call_count, 1)

            # force_refresh=True (--no-cache) must re-download and overwrite the cache file
            mock_response_refresh = unittest.mock.MagicMock()
            mock_response_refresh.__enter__.return_value = mock_response_refresh
            mock_response_refresh.read.side_effect = [
                b"refreshed_trace_data",
                b"",
            ]
            mock_urlopen.return_value = mock_response_refresh

            cached_path_3 = PerfettoTraceProcessor._resolve_and_cache_url(
                url, registry, force_refresh=True
            )
            self.assertEqual(cached_path, cached_path_3)
            self.assertEqual(mock_urlopen.call_count, 2)
            with open(cached_path_3, "rb") as f:
                self.assertEqual(f.read(), b"refreshed_trace_data")
        finally:
            if os.path.exists(cached_path):
                os.remove(cached_path)

    @unittest.mock.patch("tp_shell.tp_utils._get_gcp_access_token")
    @unittest.mock.patch("urllib.request.urlopen")
    def test_fuchsia_trace_viewer_short_hash_and_cache_normalization(
        self,
        mock_urlopen: unittest.mock.MagicMock,
        mock_token: unittest.mock.MagicMock,
    ) -> None:
        """Tests short-hash resolution on fuchsia-trace-viewer.corp.goog and deep-link cache sharing."""
        mock_token.return_value = "fake-oauth-token"
        full_object = "sha256/deadbeef12344b549841a371781e4e1e6f290edda50cf29dc60f07360083948f.fxt"

        list_resp = unittest.mock.MagicMock()
        list_resp.__enter__.return_value = list_resp
        list_resp.read.return_value = (
            f'{{"items": [{{"name": "{full_object}"}}]}}'.encode("utf-8")
        )

        media_resp = unittest.mock.MagicMock()
        media_resp.headers = {"Content-Type": "application/octet-stream"}
        media_resp.read.side_effect = [b"\x10\x00\x04\x46\x78\x54\x16\x00", b""]

        mock_urlopen.side_effect = [list_resp, media_resp]

        registry = FuchsiaPlatformDelegate("").default_resolver_registry()
        deep_link_url = "https://fuchsia-trace-viewer.corp.goog/trace/deadbeef1234?ts=5632100000&dur=48000000&visStart=5000000000&visEnd=6000000000"
        bare_url = "https://fuchsia-trace-viewer.corp.goog/trace/deadbeef1234"
        api_url = (
            "https://fuchsia-trace-viewer.corp.goog/api/trace/deadbeef1234"
        )

        cached_path = PerfettoTraceProcessor._resolve_and_cache_url(
            deep_link_url, registry, force_refresh=True
        )
        try:
            self.assertTrue(os.path.exists(cached_path))
            self.assertEqual(mock_urlopen.call_count, 2)

            # Verify bare_url and api_url hit the exact same cache file without network calls
            self.assertEqual(
                PerfettoTraceProcessor._resolve_and_cache_url(
                    bare_url, registry
                ),
                cached_path,
            )
            self.assertEqual(
                PerfettoTraceProcessor._resolve_and_cache_url(
                    api_url, registry
                ),
                cached_path,
            )
            self.assertEqual(mock_urlopen.call_count, 2)
        finally:
            if os.path.exists(cached_path):
                os.remove(cached_path)

    @unittest.mock.patch("tp_shell.tp_utils._get_gcp_access_token")
    @unittest.mock.patch("urllib.request.urlopen")
    def test_fuchsia_trace_viewer_not_found_and_ambiguous(
        self,
        mock_urlopen: unittest.mock.MagicMock,
        mock_token: unittest.mock.MagicMock,
    ) -> None:
        """Tests FileNotFoundError on 0 matches and ValueError on ambiguous prefix matches."""
        from tp_shell.tp_utils import HttpsUriResolver

        mock_token.return_value = "fake-oauth-token"

        empty_resp = unittest.mock.MagicMock()
        empty_resp.__enter__.return_value = empty_resp
        empty_resp.read.return_value = b"{}"

        ambiguous_resp = unittest.mock.MagicMock()
        ambiguous_resp.__enter__.return_value = ambiguous_resp
        ambiguous_resp.read.return_value = b'{"items": [{"name": "sha256/abcdef11.fxt"}, {"name": "sha256/abcdef22.fxt"}]}'

        mock_urlopen.side_effect = [empty_resp, ambiguous_resp]

        resolver = HttpsUriResolver(
            "https://fuchsia-trace-viewer.corp.goog/trace/abcdef"
        )
        with self.assertRaises(FileNotFoundError):
            resolver.resolve()

        with self.assertRaises(ValueError):
            resolver.resolve()

    @unittest.mock.patch("tp_shell.tp_utils._get_gcp_access_token")
    @unittest.mock.patch("urllib.request.urlopen")
    def test_fuchsia_trace_viewer_resultdb_resolution(
        self,
        mock_urlopen: unittest.mock.MagicMock,
        mock_token: unittest.mock.MagicMock,
    ) -> None:
        """Tests /trace/rdb/... resolution with percent-encoded slashes and XSSI prefix."""
        mock_token.return_value = "fake-oauth-token"

        prpc_resp = unittest.mock.MagicMock()
        prpc_resp.__enter__.return_value = prpc_resp
        prpc_resp.read.return_value = (
            b")]}'\n"
            b'{"artifacts": [{"name": "invocations/inv-1/tests/t/results/r/artifacts/trace.fxt", '
            b'"fetchUrl": "https://storage.googleapis.com/signed-rdb-trace.fxt"}]}'
        )

        media_resp = unittest.mock.MagicMock()
        media_resp.headers = {"Content-Type": "application/octet-stream"}
        media_resp.read.side_effect = [b"fxt_binary_bytes", b""]

        mock_urlopen.side_effect = [prpc_resp, media_resp]

        rdb_url = (
            "https://fuchsia-trace-viewer.corp.goog/trace/rdb/"
            "build-87654321/fuchsia-pkg%3A%2F%2Ffuchsia.com%2Ftest%23meta%2Fcomp.cm/trace.fxt"
        )
        registry = FuchsiaPlatformDelegate("").default_resolver_registry()
        results = registry.resolve(rdb_url)
        self.assertEqual(len(results), 1)
        self.assertEqual(b"".join(results[0].generator), b"fxt_binary_bytes")

    @unittest.mock.patch("tp_shell.tp_utils._get_gcp_access_token")
    @unittest.mock.patch("urllib.request.urlopen")
    def test_gs_uri_resolver(
        self,
        mock_urlopen: unittest.mock.MagicMock,
        mock_token: unittest.mock.MagicMock,
    ) -> None:
        """Tests direct gs://<bucket>/<object> URI resolution."""
        mock_token.return_value = "fake-oauth-token"

        media_resp = unittest.mock.MagicMock()
        media_resp.headers = {"Content-Type": "application/octet-stream"}
        media_resp.read.side_effect = [b"gs_trace_bytes", b""]
        mock_urlopen.return_value = media_resp

        registry = FuchsiaPlatformDelegate("").default_resolver_registry()
        results = registry.resolve(
            "gs://fuchsia-trace-viewer-traces/sha256/fc1de629.fxt"
        )
        self.assertEqual(len(results), 1)
        self.assertEqual(b"".join(results[0].generator), b"gs_trace_bytes")

    @unittest.mock.patch("urllib.request.urlopen")
    def test_html_rejection_and_poisoned_cache_self_healing(
        self, mock_urlopen: unittest.mock.MagicMock
    ) -> None:
        """Tests that SSO HTML responses are rejected and poisoned cache files self-heal."""
        sso_html = (
            b'<!--googleoff: all-->\n<html lang="en"><head>'
            b"<title>fuchsia-trace-viewer.corp.goog - Google Single Sign On</title></head></html>"
        )

        # 1. Verify an HTML response from urlopen is rejected and not cached
        html_resp = unittest.mock.MagicMock()
        html_resp.headers = {"Content-Type": "text/html; charset=utf-8"}
        html_resp.read.side_effect = [sso_html, b""]
        mock_urlopen.return_value = html_resp

        url = "https://example.com/unauthenticated_trace.fxt"
        registry = FuchsiaPlatformDelegate("").default_resolver_registry()
        with self.assertRaises(ValueError):
            PerfettoTraceProcessor._resolve_and_cache_url(url, registry)

        # 2. Pre-populate a poisoned HTML cache file and verify _resolve_and_cache_url evicts and heals it
        import hashlib
        import tempfile

        cache_key = hashlib.sha256(url.encode("utf-8")).hexdigest()
        cache_dir = os.path.join(tempfile.gettempdir(), "perf_analyze_cache")
        os.makedirs(cache_dir, exist_ok=True)
        poisoned_file = os.path.join(cache_dir, f"{cache_key}.fxt")
        with open(poisoned_file, "wb") as f:
            f.write(sso_html)

        valid_resp = unittest.mock.MagicMock()
        valid_resp.headers = {"Content-Type": "application/octet-stream"}
        valid_resp.read.side_effect = [b"healed_binary_trace_data", b""]
        mock_urlopen.return_value = valid_resp

        try:
            healed_path = PerfettoTraceProcessor._resolve_and_cache_url(
                url, registry
            )
            self.assertEqual(healed_path, poisoned_file)
            with open(healed_path, "rb") as f:
                self.assertEqual(f.read(), b"healed_binary_trace_data")
        finally:
            if os.path.exists(poisoned_file):
                os.remove(poisoned_file)


if __name__ == "__main__":
    unittest.main()
