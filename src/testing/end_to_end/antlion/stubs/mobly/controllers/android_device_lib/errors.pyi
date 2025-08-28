from _typeshed import Incomplete
from mobly import signals as signals

HIERARCHY_TOKEN: str

class Error(signals.ControllerError): ...

class DeviceError(Error):
    def __init__(self, ad, msg) -> None: ...

class ServiceError(DeviceError):
    SERVICE_TYPE: Incomplete
    def __init__(self, device, msg) -> None: ...
