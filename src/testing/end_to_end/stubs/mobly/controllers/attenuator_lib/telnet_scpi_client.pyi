from _typeshed import Incomplete
from mobly.controllers import attenuator as attenuator

class TelnetScpiClient:
    tx_cmd_separator: Incomplete
    rx_cmd_separator: Incomplete
    prompt: Incomplete
    host: Incomplete
    port: Incomplete
    def __init__(
        self,
        tx_cmd_separator: str = ...,
        rx_cmd_separator: str = ...,
        prompt: str = ...,
    ) -> None: ...
    def open(self, host, port: int = ...) -> None: ...
    @property
    def is_open(self): ...
    def close(self) -> None: ...
    def cmd(self, cmd_str, wait_ret: bool = ...): ...
