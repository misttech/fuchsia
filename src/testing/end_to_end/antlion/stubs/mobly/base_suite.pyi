import abc

from _typeshed import Incomplete

class BaseSuite(abc.ABC, metaclass=abc.ABCMeta):
    def __init__(self, runner, config) -> None: ...
    @property
    def user_params(self): ...
    def add_test_class(
        self,
        clazz,
        config: Incomplete | None = ...,
        tests: Incomplete | None = ...,
        name_suffix: Incomplete | None = ...,
    ) -> None: ...
    @abc.abstractmethod
    def setup_suite(self, config): ...
    def teardown_suite(self) -> None: ...
