# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
import asyncio
import unittest

from fidl._ipc import _QueueWrapper


class QueueWrapperTest(unittest.IsolatedAsyncioTestCase):
    async def test_put_nowait_single(self) -> None:
        queue = _QueueWrapper(name="q")
        queue.put_nowait(2)
        res = await queue.get()
        queue.task_done()
        self.assertEqual(res, 2)

    async def test_put_nowait_multiple(self) -> None:
        queue = _QueueWrapper(name="q")
        for i in range(10):
            queue.put_nowait(i)

        for i in range(10):
            res = await queue.get()
            queue.task_done()
            self.assertEqual(res, i)

    async def test_put_nowait_multiple_concurrent(self) -> None:
        queue = _QueueWrapper(name="q")

        for i in range(10):
            queue.put_nowait(i)

        async def get_func() -> int:
            res = await queue.get()
            queue.task_done()
            return res

        items = sorted(await asyncio.gather(*[get_func() for _ in range(10)]))
        assert items == list(range(10))

    def test_init_without_loop_put_first(self) -> None:
        queue = _QueueWrapper(name="q")

        async def test_logic() -> None:
            queue.put_nowait(1)
            assert await queue.get() == 1
            queue.task_done()

        asyncio.run(test_logic())

    def test_init_without_loop_get_first(self) -> None:
        queue = _QueueWrapper(name="q")

        async def get_func() -> int:
            res = await queue.get()
            queue.task_done()
            return res

        loop = asyncio.new_event_loop()
        get_task = loop.create_task(get_func())

        # Run the event loop so get_task will stall at its first await.
        loop.run_until_complete(asyncio.sleep(0.1))

        async def put_func() -> None:
            queue.put_nowait(1)

        loop.run_until_complete(put_func())
        assert loop.run_until_complete(get_task) == 1

    def test_across_loops_previous_closed(self) -> None:
        queue = _QueueWrapper(name="q")

        with asyncio.Runner() as runner:

            async def test_logic() -> None:
                queue.put_nowait(1)
                assert await queue.get() == 1
                queue.task_done()

            runner.run(test_logic())

        with asyncio.Runner() as runner:

            async def test_logic() -> None:
                queue.put_nowait(2)
                assert await queue.get() == 2
                queue.task_done()

            runner.run(test_logic())

    def test_across_loops_previous_open_put_fails(self) -> None:
        queue = _QueueWrapper(name="q")

        loop_a = asyncio.new_event_loop()
        loop_b = asyncio.new_event_loop()

        async def put_func() -> None:
            queue.put_nowait(1)

        loop_a.run_until_complete(put_func())

        with self.assertRaises(RuntimeError):
            loop_b.run_until_complete(put_func())

    def test_across_loops_previous_open_get_fails(self) -> None:
        queue = _QueueWrapper(name="q")

        loop_a = asyncio.new_event_loop()
        loop_b = asyncio.new_event_loop()

        async def put_func() -> None:
            queue.put_nowait(1)

        loop_a.run_until_complete(put_func())

        async def get_func() -> None:
            await queue.get()
            queue.task_done()

        with self.assertRaises(RuntimeError):
            loop_b.run_until_complete(get_func())

    def test_put_non_async_fails(self) -> None:
        queue = _QueueWrapper(name="q")
        with self.assertRaises(RuntimeError):
            queue.put_nowait(1)
