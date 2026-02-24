from collections.abc import Generator

from _typeshed import Incomplete
from mobly import base_test as base_test
from mobly import config_parser as config_parser
from mobly import logger as logger
from mobly import records as records
from mobly import signals as signals
from mobly import utils as utils

class Error(Exception): ...

def main(argv: Incomplete | None = ...) -> None: ...
def parse_mobly_cli_args(argv): ...

class TestRunner:
    class _TestRunInfo:
        config: Incomplete
        test_class: Incomplete
        test_class_name_suffix: Incomplete
        tests: Incomplete
        def __init__(
            self,
            config,
            test_class,
            tests: Incomplete | None = ...,
            test_class_name_suffix: Incomplete | None = ...,
        ) -> None: ...

    class _TestRunMetaData:
        root_output_path: Incomplete
        def __init__(self, log_dir, testbed_name) -> None: ...
        def generate_test_run_log_path(self): ...
        def set_start_point(self) -> None: ...
        def set_end_point(self) -> None: ...
        @property
        def run_id(self): ...
        @property
        def time_elapsed_sec(self): ...

    results: Incomplete
    def __init__(self, log_dir, testbed_name) -> None: ...
    def mobly_logger(
        self, alias: str = ..., console_level=...
    ) -> Generator[Incomplete, None, None]: ...
    def add_test_class(
        self,
        config,
        test_class,
        tests: Incomplete | None = ...,
        name_suffix: Incomplete | None = ...,
    ) -> None: ...
    def run(self) -> None: ...
