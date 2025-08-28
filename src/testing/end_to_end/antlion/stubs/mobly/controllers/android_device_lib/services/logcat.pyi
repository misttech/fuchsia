from _typeshed import Incomplete
from mobly import utils as utils
from mobly.controllers.android_device_lib import adb as adb
from mobly.controllers.android_device_lib import errors as errors
from mobly.controllers.android_device_lib.services import (
    base_service as base_service,
)

CREATE_LOGCAT_FILE_TIMEOUT_SEC: int

class Error(errors.ServiceError):
    SERVICE_TYPE: str

class Config:
    clear_log: Incomplete
    logcat_params: Incomplete
    output_file_path: Incomplete
    def __init__(
        self,
        logcat_params: Incomplete | None = ...,
        clear_log: bool = ...,
        output_file_path: Incomplete | None = ...,
    ) -> None: ...

class Logcat(base_service.BaseService):
    OUTPUT_FILE_TYPE: str
    adb_logcat_file_path: Incomplete
    def __init__(
        self, android_device, configs: Incomplete | None = ...
    ) -> None: ...
    def create_output_excerpts(self, test_info): ...
    @property
    def is_alive(self): ...
    def clear_adb_log(self) -> None: ...
    def update_config(self, new_config) -> None: ...
    def start(self) -> None: ...
    def stop(self) -> None: ...
    def pause(self) -> None: ...
    def resume(self) -> None: ...
