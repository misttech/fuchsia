from _typeshed import Incomplete
from mobly.controllers.android_device_lib import snippet_event as snippet_event
from mobly.snippet import errors as errors

MAX_TIMEOUT: Incomplete
DEFAULT_TIMEOUT: int
Error = errors.CallbackHandlerBaseError
TimeoutError = errors.CallbackHandlerTimeoutError

class CallbackHandler:
    ret_value: Incomplete
    def __init__(
        self, callback_id, event_client, ret_value, method_name, ad
    ) -> None: ...
    @property
    def callback_id(self): ...
    def waitAndGet(self, event_name, timeout=...): ...
    def waitForEvent(self, event_name, predicate, timeout=...): ...
    def getAll(self, event_name): ...
