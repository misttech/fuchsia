# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import asyncio
import functools
import types
from typing import Any, Callable, Coroutine, ParamSpec, TypeVar

import fuchsia_async_extension

_Params = ParamSpec("_Params")
_Yield = TypeVar("_Yield")
_Send = TypeVar("_Send")
_Ret = TypeVar("_Ret")


class AsyncAdapterError(Exception):
    """Raised when an asyncmethod is used outside of an AsyncAdapter."""


class AsyncAdapter:
    """A wrapper or mixin that supports async calls in a synchronous context.

    This can be used with any object where you wish to expose functions as
    synchronous when in reality they are implemented in async. This is for
    convenience in areas like Mobly where tests are expected to be run as
    synchronous methods. Or in places where you intend to have things like
    `asyncio.Queue` used across multiple function calls.

    The implementation is simple: the class using this adapter is given an
    async loop to itself. This is the main loop used for every function call
    to this object.

    To expose an async function as synchronous, just use the `asyncmethod`
    decorator.

    For example:

    ```python
    class TestClass(AsyncAdapter, BaseTestClass):

        def __init__(self):
            AsyncAdapter.__init__(self)
            # ...

        @asyncmethod
        async def foo(self) -> None:
            await asyncio.sleep(1)
    ```

    In the above, the `foo` method will be exposed as a synchronous method,
    but inside it is async code.

    If you're using this AsyncAdapter as a mixin and you're getting exceptions
    when using the `asyncmethod` decorator, make sure to put this first in the
    inheritance order to ensure proper initialization based on Python's
    method resolution order.

    Limitations:

    It is not currently possible to call one async-wrapped method from inside
    another async-wrapped method. To workaround this one will have to write
    regular async helper functions.
    """

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._async_adapter_loop = fuchsia_async_extension.get_loop()
        self._async_adapter_loop._name = self.__class__.__qualname__  # type: ignore[attr-defined]

    def loop(self) -> asyncio.AbstractEventLoop:
        """Returns a copy of this class's event loop.

        This is intended for spawning tasks in this class.
        """
        return self._async_adapter_loop

    def cancel_task(self, task: asyncio.Task[Any]) -> None:
        """Cancel a task then verify it has been cancelled.

        Args:
            task: The task to cancel

        Raises:
            RuntimeError: failed cancel verification
        """
        if not task.cancel():
            # Task was already done or cancelled, nothing else to do.
            return

        # Wait for task to completely cancel.
        try:
            self.loop().run_until_complete(task)
            raise RuntimeError(
                "Expected cancellation of task to raise CancelledError"
            )
        except asyncio.exceptions.CancelledError:
            pass  # expected


class BoundAsyncMethod:
    """Represents the asyncmethod bound to a specific instance."""

    def __init__(
        self, descriptor: "AsyncMethodDescriptor", instance: Any
    ) -> None:
        self._descriptor = descriptor
        self.__self__ = instance
        self.__func__ = descriptor
        functools.update_wrapper(self, descriptor)

    def __call__(self, *args: Any, **kwargs: Any) -> Any:
        return self._descriptor(self.__self__, *args, **kwargs)

    def unwrap_from_asyncmethod(self) -> Callable[..., Any]:
        """Returns the original async function, explicitly bound to the instance."""
        return types.MethodType(self._descriptor.__wrapped__, self.__self__)


class AsyncMethodDescriptor:
    """The descriptor that replaces the standard @asyncmethod wrapper function."""

    def __init__(self, func: Callable[..., Any]) -> None:
        self.__wrapped__ = func
        self._is_outermost_decorator = False
        functools.update_wrapper(self, func)

    def __set_name__(self, owner: Any, name: str) -> None:
        self._is_outermost_decorator = True

    def __call__(self, *args: Any, **kwargs: Any) -> Any:
        if not self._is_outermost_decorator:
            raise RuntimeError(
                f"The method '{self.__wrapped__.__name__}' is wrapped incorrectly. "
                "@asyncmethod MUST be the outermost decorator. Please flip the "
                "decorator order."
            )

        coro = self.__wrapped__(*args, **kwargs)
        try:
            loop = getattr(args[0], "_async_adapter_loop")  # args[0] == self
        except AttributeError as e:
            raise AsyncAdapterError(
                "`asyncmethod` was used outside of an `AsyncAdapter`. "
                "Your class must inherit from "
                "`fuchsia_controller_py.wrappers.AsyncAdapter` to use this "
                "decorator. If you're already inheriting this and you're "
                "seeing this exception, put `AsyncAdapter` first in your "
                "inheritance order."
            ) from e

        return loop.run_until_complete(coro)

    def __get__(self, instance: Any, owner: Any = None) -> Any:
        # Accessed on the class directly (e.g. AsyncAdapter.my_method)
        if instance is None:
            return self
        # Accessed on the instance (e.g. self.my_method)
        return BoundAsyncMethod(self, instance)

    def unwrap_from_asyncmethod(self) -> Callable[..., Any]:
        # This unwrap is trivial because there is not an instance of the
        # class that the wrapper is bound to yet.
        return self.__wrapped__


def asyncmethod(
    func: Callable[_Params, Coroutine[_Yield, _Send, _Ret]],
) -> Callable[_Params, _Ret]:
    """A decorator to expose an async method as synchronous.

    This should ONLY be used with classes that inherit `AsyncAdapter`.
    """
    return AsyncMethodDescriptor(func)
