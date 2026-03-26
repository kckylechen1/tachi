"""Worker lifecycle manager for Tachi MCP server."""

import asyncio
import logging

from memory_core_py import MemoryStore

import config as mcp_config
import store
from event_queue import init_event_queue
from workers.causal import CausalWorker
from workers.consolidator import ConsolidatorWorker
from workers.distiller import DistillerWorker
from workers.extractor import ExtractorWorker

logger = logging.getLogger(mcp_config.logger_name("workers"))


class WorkerLauncher:
    """Manages worker lifecycle - start/stop all workers."""

    def __init__(self, db_path: str | None = None, conn: MemoryStore | None = None):
        self.db_path = db_path or store.DB_PATH
        self.conn = conn or store.get_connection()
        self._tasks: list[asyncio.Task] = []
        init_event_queue(self.db_path)

    def start(self):
        """Start all workers as background asyncio tasks."""
        workers = [
            ExtractorWorker(db_path=self.db_path, conn=self.conn),
            CausalWorker(db_path=self.db_path, conn=self.conn),
            ConsolidatorWorker(db_path=self.db_path, conn=self.conn),
            DistillerWorker(db_path=self.db_path, conn=self.conn),
        ]
        for worker in workers:
            task = asyncio.create_task(worker.run_loop())
            task.set_name(f"worker-{worker.worker_type}")
            self._tasks.append(task)
            logger.info("Started worker: %s", worker.worker_type)

    async def stop(self):
        """Cancel all worker tasks gracefully."""
        for task in self._tasks:
            task.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()
        logger.info("All workers stopped")
