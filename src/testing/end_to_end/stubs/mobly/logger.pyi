import logging

from _typeshed import Incomplete
from mobly import records as records
from mobly import utils as utils

LINUX_MAX_FILENAME_LENGTH: int
WINDOWS_MAX_FILENAME_LENGTH: int
WINDOWS_RESERVED_CHARACTERS_REPLACEMENTS: Incomplete
WINDOWS_RESERVED_FILENAME_REGEX: Incomplete
WINDOWS_RESERVED_FILENAME_PREFIX: str
log_line_format: str
log_line_time_format: str
log_line_timestamp_len: int
logline_timestamp_re: Incomplete

def is_valid_logline_timestamp(timestamp): ...
def logline_timestamp_comparator(t1, t2): ...
def epoch_to_log_line_timestamp(
    epoch_time, time_zone: Incomplete | None = ...
): ...
def get_log_line_timestamp(delta: Incomplete | None = ...): ...
def get_log_file_timestamp(delta: Incomplete | None = ...): ...
def kill_test_logger(logger) -> None: ...
def create_latest_log_alias(actual_path, alias) -> None: ...
def setup_test_logger(
    log_path,
    prefix: Incomplete | None = ...,
    alias: str = ...,
    console_level=...,
) -> None: ...
def sanitize_filename(filename): ...
def normalize_log_line_timestamp(log_line_timestamp): ...

class PrefixLoggerAdapter(logging.LoggerAdapter):
    EXTRA_KEY_LOG_PREFIX: str
    _KWARGS_TYPE: Incomplete
    _PROCESS_RETURN_TYPE: Incomplete
    extra: _KWARGS_TYPE
    def process(
        self, msg: str, kwargs: _KWARGS_TYPE
    ) -> _PROCESS_RETURN_TYPE: ...
