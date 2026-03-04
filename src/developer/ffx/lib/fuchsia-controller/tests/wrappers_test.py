# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import asyncio
import unittest
from typing import Callable, ParamSpec, TypeVar
from unittest.mock import patch

from fuchsia_controller_py.wrappers import (
    AsyncAdapter,
    AsyncAdapterError,
    AsyncMethodDescriptor,
    BoundAsyncMethod,
    asyncmethod,
)

P = ParamSpec("P")
T = TypeVar("T")


def no_op_decorator(func: Callable[P, T]) -> Callable[P, T]:
    """A simple decorator to test the trapping mechanism."""

    def wrapper(*args: P.args, **kwargs: P.kwargs) -> T:
        return func(*args, **kwargs)

    return wrapper


class ValidAdapter(AsyncAdapter):
    @asyncmethod
    async def multiply(self, x: int, y: int) -> int:
        await asyncio.sleep(0.01)
        return x * y

    @asyncmethod
    @no_op_decorator  # Valid: other decorators must be on the inside
    async def inner_wrapped_method(self) -> str:
        return "success"


class InvalidAdapterWrapped(AsyncAdapter):
    @no_op_decorator  # Invalid: asyncmethod must be outermost
    @asyncmethod
    @no_op_decorator
    async def multiply(self, x: int, y: int) -> int:
        return x * y


class InvalidAdapterMissingInheritance:
    # Does not inherit from AsyncAdapter
    @asyncmethod
    async def multiply(self, x: int, y: int) -> int:
        return x * y


class TestAsyncMethodDescriptor(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        # Mock fuchsia_async_extension run locally using standard asyncio
        self.loop_patcher = patch("fuchsia_async_extension.get_loop")
        self.mock_get_loop = self.loop_patcher.start()
        self.mock_get_loop.return_value = asyncio.new_event_loop()

    def tearDown(self) -> None:
        self.loop_patcher.stop()

    def test_sync_execution(self) -> None:
        """Verifies that the method runs synchronously via the adapter's loop."""
        adapter = ValidAdapter()
        result = adapter.multiply(5, 4)
        self.assertEqual(result, 20)

    async def test_bound_unwrap(self) -> None:
        """Verifies that unwrapping from an instance returns a method bound to that instance."""
        adapter = ValidAdapter()

        # Unwrap the method
        assert isinstance(adapter.multiply, BoundAsyncMethod)
        unwrapped_coro_func = adapter.multiply.unwrap_from_asyncmethod()

        # Because it is properly bound, we do NOT need to pass `self` (adapter)
        result = await unwrapped_coro_func(5, 6)

        self.assertEqual(result, 30)
        # Prove the binding holds the correct instance
        assert isinstance(adapter.multiply, BoundAsyncMethod)
        self.assertIs(unwrapped_coro_func.__self__, adapter)

    def test_unbound_unwrap(self) -> None:
        """Verifies that unwrapping from the class returns the raw, unbound function."""
        # Unwrapping directly from the class
        assert isinstance(ValidAdapter.multiply, AsyncMethodDescriptor)
        unwrapped_raw_func = ValidAdapter.multiply.unwrap_from_asyncmethod()

        # Prove it's just the raw function (no __self__ attribute)
        self.assertFalse(hasattr(unwrapped_raw_func, "__self__"))
        self.assertEqual(unwrapped_raw_func.__name__, "multiply")

    def test_missing_async_adapter_raises_error(self) -> None:
        """Verifies the fallback exception if the class doesn't inherit AsyncAdapter."""
        bad_adapter = InvalidAdapterMissingInheritance()

        with self.assertRaisesRegex(
            AsyncAdapterError, "`asyncmethod` was used outside"
        ):
            bad_adapter.multiply(2, 2)

    def test_outer_decorator_trap_raises_error(self) -> None:
        """Verifies the __set_name__ trap catches wrappers placed outside @asyncmethod."""
        adapter = InvalidAdapterWrapped()

        with self.assertRaisesRegex(
            RuntimeError, "MUST be the outermost decorator"
        ):
            adapter.multiply(2, 2)

    def test_inner_decorator_executes_safely(self) -> None:
        """Verifies that @asyncmethod still works if it wraps another decorator."""
        adapter = ValidAdapter()
        result = adapter.inner_wrapped_method()
        self.assertEqual(result, "success")


if __name__ == "__main__":
    unittest.main()
