from _typeshed import Incomplete
from mobly import utils as utils
from mobly.controllers.android_device_lib import adb as adb
from mobly.controllers.android_device_lib import errors as errors
from mobly.controllers.android_device_lib import (
    jsonrpc_client_base as jsonrpc_client_base,
)
from mobly.snippet import errors as snippet_errors

AppStartPreCheckError = snippet_errors.ServerStartPreCheckError
ProtocolVersionError = snippet_errors.ServerStartProtocolError

class SnippetClient(jsonrpc_client_base.JsonRpcClientBase):
    package: Incomplete
    def __init__(self, package, ad) -> None: ...
    @property
    def is_alive(self): ...
    @property
    def user_id(self): ...
    def start_app_and_connect(self) -> None: ...
    host_port: Incomplete
    def restore_app_connection(self, port: Incomplete | None = ...) -> None: ...
    def stop_app(self) -> None: ...
    def help(self, print_output: bool = ...): ...
