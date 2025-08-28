import enum

from _typeshed import Incomplete
from mobly import signals as signals
from mobly import utils as utils

OUTPUT_FILE_INFO_LOG: str
OUTPUT_FILE_DEBUG_LOG: str
OUTPUT_FILE_SUMMARY: str

class Error(Exception): ...

def uid(uid): ...

class TestSummaryEntryType(enum.Enum):
    TEST_NAME_LIST: str
    RECORD: str
    SUMMARY: str
    CONTROLLER_INFO: str
    USER_DATA: str

class TestSummaryWriter:
    def __init__(self, path) -> None: ...
    def __copy__(self): ...
    def __deepcopy__(self, *args): ...
    def dump(self, content, entry_type) -> None: ...

class TestResultEnums:
    RECORD_NAME: str
    RECORD_CLASS: str
    RECORD_BEGIN_TIME: str
    RECORD_END_TIME: str
    RECORD_RESULT: str
    RECORD_UID: str
    RECORD_EXTRAS: str
    RECORD_EXTRA_ERRORS: str
    RECORD_DETAILS: str
    RECORD_TERMINATION_SIGNAL_TYPE: str
    RECORD_STACKTRACE: str
    RECORD_SIGNATURE: str
    RECORD_RETRY_PARENT: str
    RECORD_POSITION: str
    TEST_RESULT_PASS: str
    TEST_RESULT_FAIL: str
    TEST_RESULT_SKIP: str
    TEST_RESULT_ERROR: str

class ControllerInfoRecord:
    KEY_TEST_CLASS: Incomplete
    KEY_CONTROLLER_NAME: str
    KEY_CONTROLLER_INFO: str
    KEY_TIMESTAMP: str
    test_class: Incomplete
    controller_name: Incomplete
    controller_info: Incomplete
    timestamp: Incomplete
    def __init__(self, test_class, controller_name, info) -> None: ...
    def to_dict(self): ...

class ExceptionRecord:
    exception: Incomplete
    type: Incomplete
    stacktrace: Incomplete
    extras: Incomplete
    position: Incomplete
    is_test_signal: Incomplete
    def __init__(self, e, position: Incomplete | None = ...) -> None: ...
    def to_dict(self): ...
    def __deepcopy__(self, memo): ...

class TestResultRecord:
    test_name: Incomplete
    test_class: Incomplete
    begin_time: Incomplete
    end_time: Incomplete
    uid: Incomplete
    signature: Incomplete
    retry_parent: Incomplete
    termination_signal: Incomplete
    extra_errors: Incomplete
    result: Incomplete
    def __init__(self, t_name, t_class: Incomplete | None = ...) -> None: ...
    @property
    def details(self): ...
    @property
    def termination_signal_type(self): ...
    @property
    def stacktrace(self): ...
    @property
    def extras(self): ...
    def test_begin(self) -> None: ...
    def update_record(self) -> None: ...
    def test_pass(self, e: Incomplete | None = ...) -> None: ...
    def test_fail(self, e: Incomplete | None = ...) -> None: ...
    def test_skip(self, e: Incomplete | None = ...) -> None: ...
    def test_error(self, e: Incomplete | None = ...) -> None: ...
    def add_error(self, position, e) -> None: ...
    def to_dict(self): ...

class TestResult:
    requested: Incomplete
    failed: Incomplete
    executed: Incomplete
    passed: Incomplete
    skipped: Incomplete
    error: Incomplete
    controller_info: Incomplete
    def __init__(self) -> None: ...
    def __add__(self, r): ...
    def add_record(self, record) -> None: ...
    def add_controller_info_record(self, controller_info_record) -> None: ...
    def add_class_error(self, test_record) -> None: ...
    def is_test_executed(self, test_name): ...
    @property
    def is_all_pass(self): ...
    def requested_test_names_dict(self): ...
    def summary_str(self): ...
    def summary_dict(self): ...
