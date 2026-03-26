"""Base worker loop for memory event processing."""

from __future__ import annotations

import asyncio
import json
import logging
from typing import Any

import store
from event_queue import claim, complete, fail, init_event_queue

logger = logging.getLogger("memory-workers")


class BaseWorker:
    worker_type: str = ""
    poll_interval: float = 2.0

    def __init__(self, db_path: str | None = None, poll_interval: float | None = None, conn=None) -> None:
        self.db_path = db_path or store.DB_PATH
        if poll_interval is not None:
            self.poll_interval = poll_interval
        # Shared MemoryStore connection — avoids sqlite-vec lock conflicts
        self._conn = conn or store.get_connection()
        init_event_queue(self.db_path)

    async def process(self, payload: dict[str, Any]) -> None:
        """Subclasses implement task-specific logic."""
        raise NotImplementedError

    async def run_loop(self) -> None:
        """Continuously poll queue and process claimed tasks."""
        if not self.worker_type:
            raise ValueError("worker_type must be defined")

        while True:
            task = claim(self.db_path, self.worker_type)
            if task:
                try:
                    payload = json.loads(task["payload"])
                    await self.process(payload)
                    complete(self.db_path, task["event_id"], self.worker_type)
                except Exception as exc:
                    logger.exception("Worker %s failed on event %s", self.worker_type, task.get("event_id"))
                    fail(self.db_path, task["event_id"], self.worker_type, str(exc))
            else:
                await asyncio.sleep(self.poll_interval)
