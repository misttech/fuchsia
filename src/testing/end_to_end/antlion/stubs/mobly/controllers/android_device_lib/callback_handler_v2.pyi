from mobly.snippet import callback_handler_base as callback_handler_base
from mobly.snippet import errors as errors

TIMEOUT_ERROR_MESSAGE: str

class CallbackHandlerV2(callback_handler_base.CallbackHandlerBase):
    def callEventWaitAndGetRpc(self, callback_id, event_name, timeout_sec): ...
    def callEventGetAllRpc(self, callback_id, event_name): ...
