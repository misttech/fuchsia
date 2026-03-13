# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import copy
import functools
import inspect
import typing
from functools import wraps
from typing import Any, Callable, Coroutine, ParamSpec, TypeVar

from mobly import base_test

_ASYNC_EVENT_LOOP: asyncio.AbstractEventLoop = asyncio.new_event_loop()


def get_loop() -> asyncio.AbstractEventLoop:
    return _ASYNC_EVENT_LOOP


P = ParamSpec("P")
T = TypeVar("T")


def make_sync_wrapper(
    func: Callable[P, Coroutine[Any, Any, T]]
) -> Callable[P, Coroutine[Any, Any, T] | T]:
    @wraps(func)
    def wrapper(
        *args: P.args, **kwargs: P.kwargs
    ) -> Coroutine[Any, Any, T] | T:
        loop: asyncio.AbstractEventLoop | None = None
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            pass

        # If there was no event loop, then run func on the global event loop.
        # This is a Mobly synchronous entry point.
        if loop is None:
            return get_loop().run_until_complete(func(*args, **kwargs))
        # If an event loop is running, return the coroutine directly.
        # The caller will await the coroutine because the calling test
        # test code must already be running in an async context.
        else:
            return func(*args, **kwargs)

    return wrapper


class _AsyncBaseTestClassMeta(base_test.BaseTestClass):
    _MOBLY_INHERITED_METHOD_NAMES = [
        "pre_run",
        "setup_class",
        "teardown_class",
        "setup_test",
        "teardown_test",
        "on_fail",
        "on_pass",
        "on_skip",
    ]

    def __init_subclass__(cls) -> None:
        super().__init_subclass__()

        dict_items = list(cls.__dict__.items())
        for attr_name, attr_value in dict_items:
            # Handle Mobly lifecycle methods
            if attr_name in cls._MOBLY_INHERITED_METHOD_NAMES:
                assert inspect.iscoroutinefunction(
                    attr_value
                ), f"Lifecycle method {attr_name} in {cls.__name__} must be a coroutine function (async def)."
                async_attr_name = f"_async_{attr_name}"
                setattr(cls, async_attr_name, attr_value)
                setattr(cls, attr_name, make_sync_wrapper(attr_value))

            # Handle async test methods
            elif attr_name.startswith("test_") and inspect.iscoroutinefunction(
                attr_value
            ):
                async_attr_name = f"_async_{attr_name}"
                setattr(cls, async_attr_name, attr_value)
                setattr(cls, attr_name, make_sync_wrapper(attr_value))


class AsyncBaseTestClass(_AsyncBaseTestClassMeta):
    # These methods intentionally mask their Mobly synchronous counterparts.
    # This ensures each subclass of this one will define these methods as async,
    # because mypy checks will enforce that. Then the __init_subclass__ in
    # _AsyncBaseTestClassMeta will wrap them with make_sync_wrapper.
    async def pre_run(self) -> None:
        pass

    async def setup_class(self) -> None:
        pass

    async def teardown_class(self) -> None:
        pass

    async def setup_test(self) -> None:
        pass

    async def teardown_test(self) -> None:
        pass

    async def on_fail(self, record: Any) -> None:
        pass

    async def on_pass(self, record: Any) -> None:
        pass

    async def on_skip(self, record: Any) -> None:
        pass

    def generate_tests(
        self,
        test_logic: Callable[P, None | Coroutine[Any, Any, None]],
        name_func: Callable[P, str],
        arg_sets: list[P.args],
        uid_func: Callable[P, str] | None = None,
    ) -> None:
        if inspect.iscoroutinefunction(test_logic):

            @wraps(test_logic)
            def wrapper(*t_args: P.args, **t_kwargs: P.kwargs) -> None:
                loop: asyncio.AbstractEventLoop | None = None
                try:
                    loop = asyncio.get_running_loop()
                except RuntimeError:
                    pass

                # This should be the typical case of Mobly calling a test method
                # from a synchronous context. Run the coroutine on the global
                # event loop.
                if loop is None:
                    return get_loop().run_until_complete(
                        test_logic(*t_args, **t_kwargs)
                    )
                # This case indicates the test method was called from an async context,
                # but only Mobly calls test methods, and Mobly always calls them
                # from a synchronous context. Therefore, reaching this case indicates
                # something has gone wrong.
                else:
                    raise RuntimeError(
                        "Test logic was called from an active coroutine context, implying it wasn't called by Mobly."
                    )

            return super().generate_tests(
                wrapper, name_func, arg_sets, uid_func
            )
        return super().generate_tests(test_logic, name_func, arg_sets, uid_func)

    async def register_controller(
        self, module: Any, required: bool = True, min_number: int = 1
    ) -> list[Any]:
        res = super().register_controller(module, required, min_number)
        if res:
            for i, obj in enumerate(res):
                if inspect.iscoroutine(obj):
                    res[i] = await obj
        # Patch module.destroy and module.get_info to run synchronously for Mobly teardown.
        # Mobly only allows registering each module once (it raises a ControllerError otherwise),
        # so there is no need to guard against double patching. Furthermore, even if double patching
        # were to occur, the synchronous wrapper is idempotent and safely passes through to the underlying function.
        if hasattr(module, "destroy"):
            original_destroy = getattr(module, "destroy")

            if inspect.iscoroutinefunction(original_destroy):

                def sync_destroy(*args: Any, **kwargs: Any) -> None:
                    get_loop().run_until_complete(
                        original_destroy(*args, **kwargs)
                    )

                setattr(module, "destroy", sync_destroy)

        if hasattr(module, "get_info"):
            original_get_info = getattr(module, "get_info")

            def sync_get_info(*args: Any, **kwargs: Any) -> list[Any]:
                if inspect.iscoroutinefunction(original_get_info):
                    return get_loop().run_until_complete(
                        original_get_info(*args, **kwargs)
                    )
                return original_get_info(*args, **kwargs)

            setattr(module, "get_info", sync_get_info)

        return res


F = typing.TypeVar("F", bound=typing.Callable[..., typing.Any])


def retry(
    count: int, max_consecutive_error: int | None = None
) -> Callable[[F], F]:
    """Decorator for retrying a test case until it passes.

    This is a copy of mobly.base_test.retry that supports both sync and
    async methods.
    """
    if count <= 1:
        raise ValueError(
            f'The `count` for `repeat` must be larger than 1, got "{count}".'
        )

    if max_consecutive_error is not None and max_consecutive_error > count:
        raise ValueError(
            f"The `max_consecutive_error` ({max_consecutive_error}) for `repeat` "
            f"must be smaller than `count` ({count})."
        )

    def _outer_decorator(
        func: F,
    ) -> F:
        setattr(func, base_test.ATTR_MAX_RETRY_CNT, count)
        setattr(func, base_test.ATTR_MAX_CONSEC_ERROR, max_consecutive_error)

        if inspect.iscoroutinefunction(func):

            @functools.wraps(func)
            async def _async_wrapper(*args: Any, **kwargs: Any) -> Any:
                return await func(*args, **kwargs)

            return typing.cast(F, _async_wrapper)

        @functools.wraps(func)
        def _wrapper(*args: Any, **kwargs: Any) -> Any:
            return func(*args, **kwargs)

        return typing.cast(F, _wrapper)

    return _outer_decorator
