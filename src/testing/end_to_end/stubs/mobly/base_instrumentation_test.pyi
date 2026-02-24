from enum import Enum

from _typeshed import Incomplete
from mobly import base_test as base_test
from mobly import records as records
from mobly import signals as signals
from mobly import utils as utils

class _InstrumentationStructurePrefixes:
    STATUS: str
    STATUS_CODE: str
    RESULT: str
    CODE: str
    FAILED: str

class _InstrumentationKnownStatusKeys:
    CLASS: str
    ERROR: str
    STACK: str
    TEST: str
    STREAM: str

class _InstrumentationStatusCodes:
    UNKNOWN: Incomplete
    OK: str
    START: str
    IN_PROGRESS: str
    ERROR: str
    FAILURE: str
    IGNORED: str
    ASSUMPTION_FAILURE: str

class _InstrumentationStatusCodeCategories:
    TIMING: Incomplete
    PASS: Incomplete
    FAIL: Incomplete
    SKIPPED: Incomplete

class _InstrumentationKnownResultKeys:
    LONGMSG: str
    SHORTMSG: str

class _InstrumentationResultSignals:
    FAIL: str
    PASS: str

class _InstrumentationBlockStates(Enum):
    UNKNOWN: int
    METHOD: int
    RESULT: int

class _InstrumentationBlock:
    state: Incomplete
    prefix: Incomplete
    previous_instrumentation_block: Incomplete
    error_message: str
    status_code: Incomplete
    current_key: Incomplete
    known_keys: Incomplete
    unknown_keys: Incomplete
    begin_time: Incomplete
    def __init__(
        self,
        state=...,
        prefix: Incomplete | None = ...,
        previous_instrumentation_block: Incomplete | None = ...,
    ) -> None: ...
    @property
    def is_empty(self): ...
    def set_error_message(self, error_message) -> None: ...
    def set_status_code(self, status_code_line) -> None: ...
    def set_key(self, structure_prefix, key_line) -> None: ...
    def add_value(self, line) -> None: ...
    def transition_state(self, new_state): ...

class _InstrumentationBlockFormatter:
    DEFAULT_INSTRUMENTATION_METHOD_NAME: str
    def __init__(self, instrumentation_block) -> None: ...
    def create_test_record(self, mobly_test_class): ...
    def has_completed_result_block_format(self, error_message): ...

class InstrumentationTestMixin:
    DEFAULT_INSTRUMENTATION_OPTION_PREFIX: str
    DEFAULT_INSTRUMENTATION_ERROR_MESSAGE: str
    def parse_instrumentation_options(
        self, parameters: Incomplete | None = ...
    ): ...
    def run_instrumentation_test(
        self,
        device,
        package,
        options: Incomplete | None = ...,
        prefix: Incomplete | None = ...,
        runner: Incomplete | None = ...,
    ): ...

class BaseInstrumentationTestClass(
    InstrumentationTestMixin, base_test.BaseTestClass
): ...
