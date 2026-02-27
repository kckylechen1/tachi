"""Standalone worker runner - for running workers without MCP server."""

import asyncio
import logging
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)) or os.path.abspath(".."))
sys.path.insert(0, os.path.dirname(__file__) or ".")

from launcher import WorkerLauncher

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(name)s] %(message)s")


async def main():
    launcher = WorkerLauncher()
    launcher.start()
    print("Sigil workers started. Press Ctrl+C to stop.")
    try:
        await asyncio.gather(*launcher._tasks)
    except asyncio.CancelledError:
        pass
    finally:
        await launcher.stop()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\nWorkers stopped.")
