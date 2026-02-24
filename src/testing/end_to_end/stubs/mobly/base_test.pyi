from typing import Any, Callable, Sequence

from _typeshed import Incomplete
from mobly import records as records
from mobly import runtime_test_info as runtime_test_info

TEST_CASE_TOKEN: str
RESULT_LINE_TEMPLATE: Incomplete
TEST_STAGE_BEGIN_LOG_TEMPLATE: str
TEST_STAGE_END_LOG_TEMPLATE: str
STAGE_NAME_PRE_RUN: str
STAGE_NAME_SETUP_GENERATED_TESTS: str
STAGE_NAME_SETUP_CLASS: str
STAGE_NAME_SETUP_TEST: str
STAGE_NAME_TEARDOWN_TEST: str
STAGE_NAME_TEARDOWN_CLASS: str
STAGE_NAME_CLEAN_UP: str
ATTR_REPEAT_CNT: str
ATTR_MAX_RETRY_CNT: str
ATTR_MAX_CONSEC_ERROR: str

class Error(Exception): ...

def repeat(count: int, max_consecutive_error: Incomplete | None = ...): ...
def retry(max_count: int): ...

class BaseTestClass:
    TAG: str
    tests: list[str]
    root_output_path: str
    log_path: str
    test_bed_name: str
    testbed_name: str
    user_params: dict[str, Any]
    results: records.TestResult
    summary_writer: Incomplete
    controller_configs: dict[str, Any]
    current_test_info: runtime_test_info.RuntimeTestInfo

    def __init__(self, configs: Any) -> None: ...
    def unpack_userparams(
        self,
        req_param_names: list[str] | None = ...,
        opt_param_names: list[str] | None = ...,
        **kwargs: Any,
    ) -> None: ...
    def register_controller(
        self, module: Any, required: bool = ..., min_number: int = ...
    ) -> list[Any]: ...
    def pre_run(self) -> None: ...
    def setup_generated_tests(self) -> None: ...
    def setup_class(self) -> None: ...
    def teardown_class(self) -> None: ...
    def setup_test(self) -> None: ...
    def teardown_test(self) -> None: ...
    def on_fail(self, record: records.TestResultRecord) -> None: ...
    def on_pass(self, record: records.TestResultRecord) -> None: ...
    def on_skip(self, record: records.TestResultRecord) -> None: ...
    def record_data(self, content: Any) -> None: ...
    def exec_one_test(
        self,
        test_name: str,
        test_method: Callable[..., Any],
        record: records.TestResultRecord | None = ...,
    ) -> None: ...
    def generate_tests(
        self,
        test_logic: Callable[..., Any],
        name_func: Callable[..., str],
        arg_sets: Sequence[Any],
        uid_func: Callable[..., str] | None = ...,
    ) -> None: ...
    def get_existing_test_names(self) -> list[str]: ...
    def run(self, test_names: list[str] | None = ...) -> None: ...
