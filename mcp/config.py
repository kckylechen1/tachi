"""Centralized runtime configuration for memory MCP."""

from __future__ import annotations

import os


APP_NAME = os.environ.get("TACHI_APP_NAME", "tachi")
LOG_PREFIX = APP_NAME


def _expand_path(value: str) -> str:
    return os.path.abspath(os.path.expanduser(value))


DEFAULT_HOME = _expand_path(f"~/.{APP_NAME}")
BASE_DIR = _expand_path(
    os.environ.get("TACHI_HOME")
    or os.environ.get("SIGIL_HOME")
    or DEFAULT_HOME
)

# Keep MEMORY_DB_PATH as top priority for backwards compatibility.
DB_PATH = _expand_path(
    os.environ.get("MEMORY_DB_PATH")
    or os.path.join(BASE_DIR, "memory.db")
)


def logger_name(component: str = "") -> str:
    component = component.strip("-")
    return f"{LOG_PREFIX}-{component}" if component else LOG_PREFIX
