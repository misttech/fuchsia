# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Trace Processor wrapper utilities for Perfetto trace analysis."""

import hashlib
import importlib.resources
import json
import logging
import os
import re
import ssl
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import weakref
from collections.abc import Iterator
from types import TracebackType
from typing import Any, Self

import perfetto.trace_processor.api
from perfetto.tools.download_trace import resolve_trace_url
from perfetto.trace_processor.api import TraceProcessor as PerfettoTP
from perfetto.trace_processor.api import TraceProcessorConfig
from perfetto.trace_processor.platform import PlatformDelegate
from perfetto.trace_uri_resolver import util as resolver_util
from perfetto.trace_uri_resolver.path import PathUriResolver
from perfetto.trace_uri_resolver.registry import ResolverRegistry
from perfetto.trace_uri_resolver.resolver import TraceUriResolver

_LOGGER = logging.getLogger(__name__)

DEFAULT_GCS_BUCKET = "fuchsia-trace-viewer-traces"
DEFAULT_VIEWER_URL = "https://fuchsia-trace-viewer.corp.goog"
DEFAULT_VIEWER_HOSTS = frozenset(
    {
        "fuchsia-trace-viewer.corp.goog",
        "fuchsia-trace-viewer-dev.corp.goog",
        "fuchsia-trace-viewer.corp.google.com",
    }
)
RESULTDB_PRPC_ENDPOINT = (
    "https://results.api.luci.app/prpc/luci.resultdb.v1.ResultDB/QueryArtifacts"
)
_UI_VIEWPORT_PARAMS = frozenset(
    {"ts", "dur", "visStart", "visEnd", "query", "table", "origin"}
)
_HTML_SIGNATURES = (b"<!doctype html", b"<html", b"<!--googleoff:")
_HEX_RE = re.compile(r"^[0-9a-fA-F]+$")
_FFX_CONFIG_CACHE: dict[str, str | None] = {}


def _is_html_bytes(data: bytes) -> bool:
    """Returns True if the byte prefix begins with an HTML document or SSO login signature.

    Complexity: O(1) time and space (inspects at most the first 256 bytes).
    """
    stripped = data[:256].lstrip().lower()
    return any(stripped.startswith(sig) for sig in _HTML_SIGNATURES)


def _is_cached_file_valid_trace(file_path: str) -> bool:
    """Checks that a cached trace file is non-empty and not a cached HTML/SSO login page.

    Complexity: O(1) time and space (reads at most 256 bytes).
    """
    if not os.path.exists(file_path) or os.path.getsize(file_path) <= 0:
        return False
    try:
        with open(file_path, "rb") as f:
            header = f.read(256)
        return len(header) > 0 and not _is_html_bytes(header)
    except OSError:
        return False


def _query_ffx_config(key: str) -> str | None:
    """Queries `ffx config get <key>` once per process and memoizes the result."""
    if key in _FFX_CONFIG_CACHE:
        return _FFX_CONFIG_CACHE[key]

    value: str | None = None
    try:
        proc = subprocess.run(
            ["ffx", "config", "get", key],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        if proc.returncode == 0:
            cleaned = proc.stdout.strip().strip('"').strip()
            if cleaned and cleaned.lower() != "none":
                value = cleaned
    except (OSError, subprocess.SubprocessError) as e:
        _LOGGER.debug("ffx config get %s failed: %s", key, e)

    _FFX_CONFIG_CACHE[key] = value
    return value


def _resolve_gcs_bucket(
    query_params: dict[str, list[str]] | None = None,
) -> str:
    """Resolves the target GCS bucket name from query params, env var, ffx config, or default."""
    if query_params and "bucket" in query_params:
        for val in query_params["bucket"]:
            if val.strip():
                return val.strip()

    env_bucket = os.environ.get("TRACE_BUCKET_NAME", "").strip()
    if env_bucket:
        return env_bucket

    cfg_bucket = _query_ffx_config("trace.gcs_bucket")
    if cfg_bucket:
        return cfg_bucket

    return DEFAULT_GCS_BUCKET


def _is_fuchsia_trace_viewer_url(
    uri: str, check_ffx_config: bool = False
) -> bool:
    """Returns True if `uri` targets a Fuchsia Trace Viewer `/trace/...` or `/api/trace/...` endpoint."""
    parsed = urllib.parse.urlsplit(uri)
    host = (parsed.hostname or "").lower()
    path = parsed.path or ""
    if not (path.startswith("/trace/") or path.startswith("/api/trace/")):
        return False

    if (
        host in DEFAULT_VIEWER_HOSTS
        or host.endswith(".corp.goog")
        or host.endswith(".corp.google.com")
    ):
        return True

    env_viewer = os.environ.get("TRACE_VIEWER_URL", "").strip()
    if env_viewer:
        env_host = (urllib.parse.urlsplit(env_viewer).hostname or "").lower()
        if env_host and host == env_host:
            return True

    if check_ffx_config:
        cfg_viewer = _query_ffx_config("trace.viewer_url")
        if cfg_viewer:
            cfg_host = (
                urllib.parse.urlsplit(cfg_viewer).hostname or ""
            ).lower()
            if cfg_host and host == cfg_host:
                return True

    return False


def _normalize_cache_url(url: str) -> str:
    """Normalizes trace URLs before computing cache keys so deep links share the same cache file.

    Strips Perfetto UI viewport query parameters (`ts`, `dur`, `visStart`, `visEnd`, `query`,
    `table`, `origin`) and fragments, and canonicalizes `/api/trace/...` to `/trace/...`.

    Complexity: O(U) where U is the URL string length.
    """
    if not _is_fuchsia_trace_viewer_url(url, check_ffx_config=False):
        return url

    parsed = urllib.parse.urlsplit(url)
    path = parsed.path
    if path.startswith("/api/trace/"):
        path = "/trace/" + path[len("/api/trace/") :]
    if len(path) > len("/trace/") and path.endswith("/"):
        path = path.rstrip("/")

    query_pairs = urllib.parse.parse_qsl(parsed.query, keep_blank_values=True)
    filtered_pairs = [
        (k, v) for k, v in query_pairs if k not in _UI_VIEWPORT_PARAMS
    ]
    normalized_query = urllib.parse.urlencode(filtered_pairs)
    return urllib.parse.urlunsplit(
        (
            parsed.scheme.lower(),
            parsed.netloc.lower(),
            path,
            normalized_query,
            "",
        )
    )


def _get_gcp_access_token() -> str:
    """Obtains an OAuth2 access token via `gcloud` (ADC first, then standard gcloud auth)."""
    commands = [
        ["gcloud", "auth", "application-default", "print-access-token"],
        ["gcloud", "auth", "print-access-token"],
    ]
    errors: list[str] = []
    for cmd in commands:
        try:
            proc = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            if proc.returncode == 0 and proc.stdout.strip():
                return proc.stdout.strip()
            errors.append(f"{' '.join(cmd)}: {proc.stderr.strip()}")
        except (OSError, subprocess.SubprocessError) as e:
            errors.append(f"{' '.join(cmd)}: {e}")

    raise RuntimeError(
        "Failed to obtain GCP access token for trace download. Please authenticate by running:\n"
        "  gcloud auth application-default login\n"
        "  gcloud auth login\n"
        f"Details: {'; '.join(errors)}"
    )


def _build_gcs_media_request(
    bucket: str, object_name: str, token: str
) -> urllib.request.Request:
    """Builds an authenticated GCS JSON API media download request."""
    encoded_bucket = urllib.parse.quote(bucket, safe="")
    encoded_object = urllib.parse.quote(object_name.lstrip("/"), safe="")
    media_url = f"https://storage.googleapis.com/storage/v1/b/{encoded_bucket}/o/{encoded_object}?alt=media"
    return urllib.request.Request(
        media_url,
        headers={"Authorization": f"Bearer {token}"},
    )


def _resolve_gcs_trace_object(
    bucket: str, trace_id: str, token: str, ssl_context: ssl.SSLContext
) -> str:
    """Resolves a Fuchsia Trace Viewer trace ID (short hex prefix, full hash, or path) to a GCS object name.

    Mirrors `storage.ResolveTraceID` in `fuchsia_trace_viewer`, querying at most 2 items
    (`maxResults=2&fields=items(name)`) for O(1) prefix resolution and collision detection.
    """
    clean_id = trace_id.strip("/")
    if clean_id.lower().endswith(".fxt"):
        clean_id = clean_id[:-4]
    if clean_id.lower().startswith("sha256/"):
        clean_id = clean_id[7:]

    is_hex = bool(_HEX_RE.match(clean_id))
    if is_hex:
        clean_id = clean_id.lower()

    if is_hex and 6 <= len(clean_id) < 64:
        prefix = f"sha256/{clean_id}"
        encoded_bucket = urllib.parse.quote(bucket, safe="")
        encoded_prefix = urllib.parse.quote(prefix, safe="")
        list_url = (
            f"https://storage.googleapis.com/storage/v1/b/{encoded_bucket}/o"
            f"?prefix={encoded_prefix}&maxResults=2&fields=items(name)"
        )
        req = urllib.request.Request(
            list_url, headers={"Authorization": f"Bearer {token}"}
        )
        try:
            with urllib.request.urlopen(
                req, context=ssl_context, timeout=30
            ) as resp:
                payload = json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            if e.code in (401, 403):
                raise RuntimeError(
                    f"GCS access denied (HTTP {e.code}) for bucket 'gs://{bucket}'. "
                    "Ensure you have access and run `gcloud auth application-default login`."
                ) from e
            raise ValueError(
                f"Failed to list GCS prefix '{prefix}' in bucket '{bucket}' (HTTP {e.code})"
            ) from e

        items = payload.get("items") or []
        if not items:
            raise FileNotFoundError(
                f"Trace '{trace_id}' (prefix '{prefix}') not found in gs://{bucket}"
            )
        if len(items) > 1:
            names = ", ".join(item.get("name", "") for item in items)
            raise ValueError(
                f"Ambiguous trace prefix '{trace_id}' matched multiple objects in gs://{bucket}: {names}"
            )
        resolved_name = items[0].get("name", "")
        if not resolved_name:
            raise FileNotFoundError(
                f"Trace '{trace_id}' resolved to empty object name in gs://{bucket}"
            )
        return str(resolved_name)

    if is_hex and len(clean_id) == 64:
        return f"sha256/{clean_id}.fxt"

    return trace_id.lstrip("/")


def _resolve_resultdb_trace_request(
    escaped_rdb_tail: str, token: str, ssl_context: ssl.SSLContext
) -> urllib.request.Request:
    """Resolves a `/trace/rdb/{invocation_id}/{escaped_test_id}/{artifact_path...}` route via LUCI ResultDB pRPC."""
    parts = escaped_rdb_tail.strip("/").split("/", 2)
    if len(parts) != 3 or not all(parts):
        raise ValueError(
            f"Invalid ResultDB trace path '/trace/rdb/{escaped_rdb_tail}': "
            "expected /trace/rdb/<invocation_id>/<escaped_test_id>/<artifact_path>"
        )

    raw_inv_id, escaped_test_id, raw_artifact_path = parts
    invocation_id = urllib.parse.unquote(raw_inv_id)
    if invocation_id.startswith("invocations/"):
        invocation_id = invocation_id[len("invocations/") :]
    test_id = urllib.parse.unquote(escaped_test_id)
    artifact_path = "/".join(
        urllib.parse.unquote(seg) for seg in raw_artifact_path.split("/")
    )

    body = json.dumps(
        {
            "invocations": [f"invocations/{invocation_id}"],
            "predicate": {
                "testResultPredicate": {
                    "testIdRegexp": re.escape(test_id),
                },
                "artifactIdRegexp": re.escape(artifact_path),
            },
            "pageSize": 1,
        }
    ).encode("utf-8")

    req = urllib.request.Request(
        RESULTDB_PRPC_ENDPOINT,
        data=body,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(
            req, context=ssl_context, timeout=30
        ) as resp:
            raw_text = resp.read().decode("utf-8")
    except urllib.error.HTTPError as e:
        if e.code in (401, 403):
            raise RuntimeError(
                f"ResultDB access denied (HTTP {e.code}) for invocation '{invocation_id}'. "
                "Please run `gcloud auth application-default login`."
            ) from e
        raise ValueError(
            f"Failed to query ResultDB artifact for invocation '{invocation_id}' (HTTP {e.code})"
        ) from e

    if raw_text.startswith(")]}'"):
        raw_text = raw_text[4:].lstrip("\r\n")
    data = json.loads(raw_text)
    artifacts = data.get("artifacts") or []
    if not artifacts:
        raise FileNotFoundError(
            f"ResultDB artifact '{artifact_path}' not found for invocation '{invocation_id}', test '{test_id}'"
        )
    fetch_url = artifacts[0].get("fetchUrl", "")
    if not fetch_url:
        raise ValueError(
            f"ResultDB artifact '{artifact_path}' is missing a signed fetchUrl"
        )
    return urllib.request.Request(fetch_url)


def resolve_fuchsia_trace_viewer_request(
    uri: str, ssl_context: ssl.SSLContext
) -> urllib.request.Request:
    """Resolves a Fuchsia Trace Viewer URL (`/trace/...` or `/api/trace/...`) to an authenticated download Request."""
    parsed = urllib.parse.urlsplit(uri)
    path = parsed.path or ""
    if path.startswith("/api/trace/"):
        tail = path[len("/api/trace/") :]
    elif path.startswith("/trace/"):
        tail = path[len("/trace/") :]
    else:
        raise ValueError(f"Unsupported Fuchsia Trace Viewer path in URL: {uri}")

    tail = tail.strip("/")
    if not tail:
        raise ValueError(
            f"Missing trace identifier in Fuchsia Trace Viewer URL: {uri}"
        )

    token = _get_gcp_access_token()
    if tail.startswith("rdb/"):
        return _resolve_resultdb_trace_request(
            tail[len("rdb/") :], token, ssl_context
        )

    query_params = urllib.parse.parse_qs(parsed.query)
    bucket = _resolve_gcs_bucket(query_params)
    unquoted_id = urllib.parse.unquote(tail)
    object_name = _resolve_gcs_trace_object(
        bucket, unquoted_id, token, ssl_context
    )
    return _build_gcs_media_request(bucket, object_name, token)


def _validate_and_stream_response(
    response: Any, source_url: str
) -> Iterator[bytes]:
    """Validates that an HTTP response is not an HTML/SSO login page and streams its bytes."""
    headers = getattr(response, "headers", None)
    if headers is not None and hasattr(headers, "get"):
        content_type = headers.get("Content-Type", "")
        if isinstance(content_type, str) and content_type.lower().startswith(
            "text/html"
        ):
            response.close()
            raise ValueError(
                f"URL '{source_url}' returned an HTML page (Content-Type: {content_type}) "
                "instead of a binary trace file (possible Corp SSO login redirect)."
            )

    gen = resolver_util.read_generator(response)
    first_chunk = next(gen, b"")
    if _is_html_bytes(first_chunk):
        response.close()
        raise ValueError(
            f"URL '{source_url}' returned an HTML document instead of a binary trace file "
            "(possible Corp SSO login redirect)."
        )

    def _stream() -> Iterator[bytes]:
        if first_chunk:
            yield first_chunk
        yield from gen

    return _stream()


class GsUriResolver(TraceUriResolver):
    """URI Resolver that streams trace data directly from `gs://<bucket>/<object>` URIs."""

    PREFIX = "gs"

    def __init__(self, uri: str) -> None:
        self.uri = uri

    @classmethod
    # TODO(https://fxbug.dev/571891548): decorate with typing.override when able.
    def from_trace_uri(cls, uri: str) -> Self:
        return cls(uri)

    # TODO(https://fxbug.dev/571891548): decorate with typing.override when able.
    def resolve(self) -> list[TraceUriResolver.Result]:
        parsed = urllib.parse.urlsplit(self.uri)
        bucket = parsed.netloc.strip()
        object_name = urllib.parse.unquote(parsed.path.lstrip("/"))
        if not bucket or not object_name:
            raise ValueError(
                f"Invalid GCS URI '{self.uri}': expected gs://<bucket>/<object_path>"
            )
        context = ssl._create_unverified_context()
        token = _get_gcp_access_token()
        req = _build_gcs_media_request(bucket, object_name, token)
        response = urllib.request.urlopen(req, context=context, timeout=120)
        return [
            TraceUriResolver.Result(
                trace=_validate_and_stream_response(response, self.uri),
                metadata={"_url": req.full_url},
            )
        ]


class HttpUriResolver(TraceUriResolver):
    """URI Resolver that streams trace data from HTTP/HTTPS endpoints and permalinks."""

    PREFIX = "http"

    def __init__(self, uri: str) -> None:
        self.uri = uri

    @classmethod
    # TODO(https://fxbug.dev/571891548): decorate with typing.override when able.
    def from_trace_uri(cls, uri: str) -> Self:
        return cls(uri)

    # TODO(https://fxbug.dev/571891548): decorate with typing.override when able.
    def resolve(self) -> list[TraceUriResolver.Result]:
        context = ssl._create_unverified_context()
        if "perfetto.dev" in self.uri:
            req_or_url: str | urllib.request.Request = resolve_trace_url(
                self.uri
            )
            resolved_url = req_or_url
        elif _is_fuchsia_trace_viewer_url(self.uri, check_ffx_config=True):
            req_or_url = resolve_fuchsia_trace_viewer_request(self.uri, context)
            resolved_url = req_or_url.full_url
        else:
            req_or_url = self.uri
            resolved_url = self.uri

        # Add a default timeout as a backstop to avoid permanently blocking when loading
        # the trace file.
        response = urllib.request.urlopen(
            req_or_url, context=context, timeout=120
        )
        return [
            TraceUriResolver.Result(
                trace=_validate_and_stream_response(response, self.uri),
                metadata={"_url": resolved_url},
            )
        ]


class HttpsUriResolver(HttpUriResolver):
    PREFIX = "https"


class FuchsiaPlatformDelegate(PlatformDelegate):
    """PlatformDelegate that points directly to the host's prebuilt trace_processor_shell binary.

    This delegate is used to override the default PlatformDelegate in the
    third_party Perfetto Python SDK, ensuring that it uses our prebuilt, in-tree
    version of trace_processor_shell instead of trying to download or resolve it
    from the network or default paths.
    """

    host_tp_shell_path: str

    def __init__(self, host_tp_shell_path: str) -> None:
        super().__init__()
        self.host_tp_shell_path = host_tp_shell_path

    # TODO(https://fxbug.dev/571891548): decorate with typing.override when able.
    def get_shell_path(
        self, bin_path: str | None = None, fetch_latest: bool = False
    ) -> str:
        return self.host_tp_shell_path

    # TODO(https://fxbug.dev/571891548): decorate with typing.override when able.
    def get_resource(self, file: str) -> bytes:
        return (
            importlib.resources.files("perfetto.trace_processor")
            .joinpath(file)
            .read_bytes()
        )

    # TODO(https://fxbug.dev/571891548): decorate with typing.override when able.
    def default_resolver_registry(self) -> ResolverRegistry:
        return ResolverRegistry(
            resolvers=[
                PathUriResolver,
                GsUriResolver,
                HttpUriResolver,
                HttpsUriResolver,
            ]
        )


class PerfettoTraceProcessor:
    """A wrapper around Perfetto's official Python TraceProcessor API.

    This class provides a way to interact with Perfetto's TraceProcessor API,
    handling the setup and teardown of the trace_processor_shell backend.
    The trace file is parsed only once, and backend process is torn down
    when the processor is closed.

    Attributes:
        trace_path: Path to the trace file to ingest.
        tp_shell_path: Path to the trace_processor_shell binary.
        debug: If True, prints SQL queries.
    """

    trace_path: str
    tp_shell_path: str
    debug: bool
    _tp: PerfettoTP
    _finalizer: weakref.finalize[Any, Any]
    _tp_shell_context: Any

    def __init__(
        self,
        trace_path: str,
        tp_shell_path: str | None = None,
        debug: bool = False,
        cache: bool = True,
    ) -> None:
        """Initializes PerfettoTraceProcessor.

        Args:
            trace_path: Path to the trace file to ingest.
            tp_shell_path: Optional path to the trace_processor_shell binary.
            debug: If True, prints SQL queries.
            cache: If True and trace_path is a URL, reuses local cached trace if valid;
                if False, forces a fresh download overwriting the cache entry (default: True).
        """
        self._tp_shell_context = None

        if tp_shell_path is None:
            try:
                # The logic here involving _tp_shell_context and its __enter__/__exit__
                # methods is necessary to correctly manage the lifecycle of the extracted
                # trace_processor_shell binary when loaded from package resources.
                # importlib.resources.as_file returns a context manager that extracts
                # the resource to a temporary location. We must call __enter__() to get
                # the path and ensure __exit__() is called for cleanup.
                resource = importlib.resources.files("tp_shell.bin").joinpath(
                    "trace_processor_shell"
                )
                self._tp_shell_context = importlib.resources.as_file(resource)
                # Enter the extraction context to obtain a real filesystem path
                extracted_path = self._tp_shell_context.__enter__()
                tp_shell_path = str(extracted_path)
                # Ensure the extracted file is executable (necessary in ZIP contexts)
                os.chmod(tp_shell_path, 0o755)
            except Exception as e:
                if self._tp_shell_context is not None:
                    try:
                        self._tp_shell_context.__exit__(None, None, None)
                    except Exception:
                        pass
                raise FileNotFoundError(
                    "trace_processor_shell was not found in the packaged resources. "
                    "The binary must either be explicitly specified or packaged as a data source dependency."
                ) from e

        self.tp_shell_path = os.path.abspath(tp_shell_path)
        if not os.path.exists(self.tp_shell_path):
            raise FileNotFoundError(
                f"Trace processor shell not found: {self.tp_shell_path}"
            )
        self.debug = debug

        delegate = FuchsiaPlatformDelegate(self.tp_shell_path)

        if (
            trace_path.startswith("http://")
            or trace_path.startswith("https://")
            or trace_path.startswith("gs://")
        ):
            registry = delegate.default_resolver_registry()
            self.trace_path = os.path.abspath(
                self._resolve_and_cache_url(
                    trace_path, registry, force_refresh=not cache
                )
            )
        else:
            self.trace_path = os.path.abspath(trace_path)
            if not os.path.exists(self.trace_path):
                raise FileNotFoundError(
                    f"Trace file not found: {self.trace_path}"
                )

        # Override Perfetto's PlatformDelegate to point directly to our prebuilt shell binary
        perfetto.trace_processor.api.PLATFORM_DELEGATE = lambda: delegate

        # Configure TraceProcessor to use a unique port to avoid conflicts
        config = TraceProcessorConfig(unique_port=True)

        _LOGGER.info(
            f"Initializing Perfetto TraceProcessor for trace: {self.trace_path}"
        )
        try:
            self._tp = PerfettoTP(trace=self.trace_path, config=config)
        except Exception:
            if self._tp_shell_context is not None:
                try:
                    self._tp_shell_context.__exit__(None, None, None)
                except Exception:
                    pass
            raise

        # Register a finalizer to ensure the subprocess is cleaned up even if close() isn't called.
        self._finalizer = weakref.finalize(
            self, self._cleanup, self._tp, self._tp_shell_context
        )

    @staticmethod
    def _cleanup(tp: PerfettoTP, context: Any = None) -> None:
        """Safely tears down the Perfetto TraceProcessor shell process and deletes temporary extraction files."""
        _LOGGER.info("Tearing down Perfetto TraceProcessor shell process...")
        try:
            tp.close()
        except Exception as e:
            _LOGGER.error(f"Error closing trace processor: {e}")

        if context is not None:
            _LOGGER.info(
                "Cleaning up temporary trace_processor_shell extraction..."
            )
            try:
                context.__exit__(None, None, None)
            except Exception as e:
                _LOGGER.error(f"Error cleaning up extraction context: {e}")

    def run_query(self, query: str) -> list[dict[str, Any]]:
        """Runs a SQL query against the trace and returns the result as a list of dicts."""
        if not hasattr(self, "_finalizer") or not self._finalizer.alive:
            raise RuntimeError("Trace processor is closed.")

        if self.debug:
            _LOGGER.debug(
                f"--- DEBUG SQL QUERY ---\n{query.strip()}\n-----------------------"
            )

        try:
            result_iterator = self._tp.query(query)
            # Row.__dict__ on instance contains exactly the dynamic attributes (columns)
            return [row.__dict__ for row in result_iterator]
        except Exception as e:
            _LOGGER.error(f"Error running query: {e}")
            raise

    def get_tables(self) -> set[str]:
        """Returns the set of table and view names available in the trace database."""
        rows = self.run_query(
            "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
        )
        return {row["name"] for row in rows}

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: TracebackType | None,
    ) -> None:
        self.close()

    def close(self) -> None:
        """Tears down the trace processor shell process."""
        if hasattr(self, "_finalizer") and self._finalizer.alive:
            self._finalizer()

    @staticmethod
    def _resolve_and_cache_url(
        url: str,
        registry: ResolverRegistry,
        force_refresh: bool = False,
    ) -> str:
        normalized_url = _normalize_cache_url(url)
        cache_key = hashlib.sha256(normalized_url.encode("utf-8")).hexdigest()
        cache_dir = os.path.join(tempfile.gettempdir(), "perf_analyze_cache")
        cached_file = os.path.join(cache_dir, f"{cache_key}.fxt")

        if not force_refresh and os.path.exists(cached_file):
            if _is_cached_file_valid_trace(cached_file):
                _LOGGER.info("Using cached trace: %s", cached_file)
                return cached_file
            _LOGGER.warning(
                "Evicting invalid or HTML-poisoned cached trace file: %s",
                cached_file,
            )
            try:
                os.remove(cached_file)
            except OSError:
                pass

        results = registry.resolve(url)
        if not results:
            raise ValueError(f"Could not resolve URL: {url}")

        os.makedirs(cache_dir, exist_ok=True)
        _LOGGER.info(
            "Downloading trace URL %s to cache %s...", url, cached_file
        )
        temp_file = f"{cached_file}.tmp.{os.getpid()}"
        try:
            with open(temp_file, "wb") as out:
                for chunk in results[0].generator:
                    out.write(chunk)
            if not _is_cached_file_valid_trace(temp_file):
                raise ValueError(
                    f"Downloaded trace from '{url}' is empty or contains an HTML document "
                    "(possible Corp SSO login redirect)."
                )
            os.replace(temp_file, cached_file)
        except Exception:
            if os.path.exists(temp_file):
                os.remove(temp_file)
            raise
        return cached_file
