"""Causal analyzer worker: extract cause-effect relationships and write graph edges.

Uses Qwen3.5-27B (with thinking disabled) for higher-quality causal extraction.
Writes results both as derived items and as graph edges in memory_edges.
"""

from __future__ import annotations

import json
import os
from typing import Any

import extractor
import store

from .base import BaseWorker
from .utils import messages_to_text

# Use 27B for causal analysis — stronger extraction quality.
# enable_thinking=false keeps it fast (~20s) and avoids empty output.
CAUSAL_MODEL = os.environ.get("CAUSAL_MODEL", "Qwen/Qwen3.5-27B")

CAUSAL_PROMPT = """\
从对话中提取因果关系和行为修正。输出 JSON 数组，每个元素是以下之一：

1. 因果关系:
{
  "type": "causal",
  "cause_text": "原因事实（≤30字）",
  "effect_text": "结果事实（≤30字）",
  "relation": "causes|supports|contradicts|follows",
  "confidence": 0.0-1.0
}

2. 行为修正:
{
  "type": "correction",
  "context": "场景",
  "wrong_action": "错误行为",
  "correct_action": "正确行为"
}

规则：
- 仅提取明确的因果/修正关系，不要猜测
- confidence < 0.5 的因果关系不要输出
- 无关系返回 []
- 仅输出 JSON 数组，不要 markdown"""


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

        # Use 27B model with thinking disabled for quality extraction
        content = await extractor._call_llm(
            messages=[
                {"role": "system", "content": CAUSAL_PROMPT},
                {"role": "user", "content": conversation_text},
            ],
            model=CAUSAL_MODEL,
            temperature=0.1,
            max_tokens=1000,
            timeout=120,
            extra_body={"enable_thinking": False},
        )

        items = self._parse_json_array(content)
        if not items:
            return

        # Process each extracted item
        corrections: list[dict] = []
        causal_edges: list[dict] = []

        for item in items:
            item_type = item.get("type", "")

            if item_type == "correction":
                context = str(item.get("context", "")).strip()
                wrong_action = str(item.get("wrong_action", "")).strip()
                correct_action = str(item.get("correct_action", "")).strip()
                if context and wrong_action and correct_action:
                    corrections.append({
                        "context": context,
                        "wrong_action": wrong_action,
                        "correct_action": correct_action,
                    })

            elif item_type == "causal":
                cause = str(item.get("cause_text", "")).strip()
                effect = str(item.get("effect_text", "")).strip()
                relation = str(item.get("relation", "causes")).strip()
                confidence = float(item.get("confidence", 0.7))
                if cause and effect and confidence >= 0.5:
                    causal_edges.append({
                        "cause": cause,
                        "effect": effect,
                        "relation": relation,
                        "confidence": confidence,
                    })

        # ── Save corrections as derived items ─────────────────────────────────
        for c in corrections:
            text = json.dumps(c, ensure_ascii=False)
            summary = await extractor.generate_summary(text)
            store.save_derived(
                text=text,
                path="/behavior/corrections",
                summary=summary,
                importance=0.9,
                source="causal",
                scope="general",
                metadata={"origin": "causal", "event_id": event_id},
            )

        # ── Write causal edges to memory graph ────────────────────────────────
        # For each causal relation, search for matching memories and write edges
        if causal_edges:
            rust_store = store.get_connection()
            for edge_data in causal_edges:
                # Try to find memory IDs matching cause and effect texts
                cause_id = self._find_memory_id(rust_store, edge_data["cause"])
                effect_id = self._find_memory_id(rust_store, edge_data["effect"])

                if cause_id and effect_id and cause_id != effect_id:
                    edge_json = json.dumps({
                        "source_id": cause_id,
                        "target_id": effect_id,
                        "relation": edge_data["relation"],
                        "weight": edge_data["confidence"],
                        "metadata": {"origin": "causal_worker", "event_id": event_id},
                    })
                    try:
                        rust_store.add_edge(edge_json)
                    except Exception as e:
                        import logging
                        logging.getLogger("sigil-causal").warning(
                            f"Failed to add edge: {e}"
                        )

    @staticmethod
    def _find_memory_id(rust_store: Any, text: str) -> str | None:
        """Find the best matching memory ID for a given text snippet."""
        try:
            opts = json.dumps({
                "top_k": 1,
                "record_access": False,
                "weights": {
                    "semantic": 0.0,
                    "fts": 1.0,
                    "symbolic": 0.5,
                    "decay": 0.0,
                },
            })
            results_str = rust_store.search(text, opts)
            results = json.loads(results_str)
            if results and results[0].get("score", {}).get("final", 0) > 0.1:
                return results[0]["entry"]["id"]
        except Exception:
            pass
        return None
