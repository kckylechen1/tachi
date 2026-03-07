"""Causal analyzer worker: detect user corrections to AI behavior."""

from __future__ import annotations

import json
from typing import Any

import embedding
import extractor
import store

from .base import BaseWorker
from .utils import messages_to_text

CAUSAL_PROMPT = (
    "检查用户是否纠正了AI。提取 JSON: context, wrong_action, correct_action。"
    "无纠正返回[]。仅输出 JSON 数组，不要 markdown。"
)


class CausalWorker(BaseWorker):
    worker_type = "causal"

    @staticmethod
    def _parse_json_array(content: str) -> list[dict[str, Any]]:
        clean = content.strip()
        if clean.startswith("```"):
            clean = clean.split("\n", 1)[1].rsplit("```", 1)[0]
        try:
            data = json.loads(clean.strip())
            if isinstance(data, list):
                return [x for x in data if isinstance(x, dict)]
        except json.JSONDecodeError:
            return []
        return []

    async def process(self, payload: dict[str, Any]) -> None:
        messages = payload.get("messages") or []
        if not isinstance(messages, list) or len(messages) < 2:
            return

        event_id = str(payload.get("event_id", "")).strip()
        conversation_text = messages_to_text(messages)
        if not conversation_text:
            return

        content = await extractor._call_llm(
            messages=[
                {"role": "system", "content": CAUSAL_PROMPT},
                {"role": "user", "content": conversation_text},
            ],
            model=extractor.SILICONFLOW_MODEL,
            temperature=0.1,
            max_tokens=1000,
            timeout=90,
        )

        corrections = self._parse_json_array(content)
        if not corrections:
            return

        correction_texts: list[str] = []
        for item in corrections:
            context = str(item.get("context", "")).strip()
            wrong_action = str(item.get("wrong_action", "")).strip()
            correct_action = str(item.get("correct_action", "")).strip()
            if not (context and wrong_action and correct_action):
                continue
            correction_texts.append(
                json.dumps(
                    {
                        "context": context,
                        "wrong_action": wrong_action,
                        "correct_action": correct_action,
                    },
                    ensure_ascii=False,
                )
            )

        if not correction_texts:
            return

        vectors = await embedding.embed_batch(correction_texts, input_type="document")
        summaries: list[str] = []
        for text in correction_texts:
            summaries.append(await extractor.generate_summary(text))

        conn = self._conn
        for text, vec, summary in zip(correction_texts, vectors, summaries):
            store.save_memory(
                conn,
                text,
                vec,
                path="/behavior/corrections",
                summary=summary,
                topic="correction",
                keywords=["correction", "causal"],
                scope="general",
                importance=0.9,
                source="causal",
                metadata={"origin": "causal", "event_id": event_id},
            )
