"""Extractor worker: parse conversation events into memory facts."""

from __future__ import annotations

from typing import Any

import embedding
import extractor
import store
from event_queue import enqueue

from .base import BaseWorker
from .utils import messages_to_text


class ExtractorWorker(BaseWorker):
    worker_type = "extractor"

    async def process(self, payload: dict[str, Any]) -> None:
        messages = payload.get("messages") or []
        if not isinstance(messages, list) or not messages:
            return

        event_id = str(payload.get("event_id", "")).strip()
        text = messages_to_text(messages)
        if not text:
            return

        raw_facts = await extractor.extract_facts(text)
        normalized_facts = [f for f in raw_facts if str(f.get("text", "")).strip()]
        if not normalized_facts:
            return

        texts = [str(f["text"]).strip() for f in normalized_facts]
        vectors = await embedding.embed_batch(texts, input_type="document")
        summaries: list[str] = []
        for item in texts:
            summaries.append(await extractor.generate_summary(item))

        conn = self._conn
        for idx, (fact, vec, summary) in enumerate(zip(normalized_facts, vectors, summaries)):
            fact_text = str(fact.get("text", "")).strip()
            if not fact_text:
                continue

            scope = fact.get("scope", "general")
            topic = str(fact.get("topic", ""))
            path = f"/{scope}"
            if topic:
                path += f"/{topic.replace(' ', '_').replace('/', '_')}"

            saved = store.save_memory(
                conn,
                fact_text,
                vec,
                path=path,
                summary=summary,
                topic=topic,
                keywords=fact.get("keywords", []),
                scope=scope,
                importance=float(fact.get("importance", 0.7)),
                source="extraction",
                metadata={"origin": "extraction", "event_id": event_id},
            )
            if not saved:
                continue

            consolidate_event_id = f"{event_id}:consolidator:{idx}:{saved['id']}" if event_id else f"consolidator:{saved['id']}"
            enqueue(
                self.db_path,
                consolidate_event_id,
                "consolidator",
                {
                    "event_id": event_id,
                    "memory_id": saved["id"],
                    "path": saved.get("path", path),
                    "origin": "extraction",
                },
            )
