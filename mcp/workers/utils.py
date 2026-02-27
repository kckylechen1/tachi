"""Shared utility functions for Sigil workers."""

from __future__ import annotations

from typing import Any


def messages_to_text(messages: list[Any]) -> str:
    """Convert a list of message dicts/strings into a single text block."""
    lines: list[str] = []
    for msg in messages:
        if isinstance(msg, dict):
            role = str(msg.get("role", "unknown"))
            content = str(msg.get("content", "")).strip()
            if content:
                lines.append(f"{role}: {content}")
        elif isinstance(msg, str):
            text = msg.strip()
            if text:
                lines.append(text)
    return "\n".join(lines)
