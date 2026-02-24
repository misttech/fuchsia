from _typeshed import Incomplete
from mobly import utils as utils
from mobly.controllers.android_device_lib import (
    event_dispatcher as event_dispatcher,
)
from mobly.controllers.android_device_lib import (
    jsonrpc_client_base as jsonrpc_client_base,
)

class Sl4aClient(jsonrpc_client_base.JsonRpcClientBase):
    ed: Incomplete
    def __init__(self, ad) -> None: ...
    device_port: Incomplete
    def start_app_and_connect(self) -> None: ...
    host_port: Incomplete
    def restore_app_connection(self, port: Incomplete | None = ...) -> None: ...
    def stop_app(self) -> None: ...
    def stop_event_dispatcher(self) -> None: ...
