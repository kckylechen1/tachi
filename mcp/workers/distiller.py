"""Distiller worker: periodically derive global behavior rules from causal memories."""

from __future__ import annotations

import asyncio
import json
import logging
import os
from typing import Any

import embedding
import extractor
import store
from memory_core_py import MemoryStore

from .base import BaseWorker

logger = logging.getLogger("memory-workers")

DISTILLER_PROMPT = (
    "你是规则蒸馏器。输入是一组用户纠正 AI 的片段。"
    "请归纳可跨场景复用的通用规则，输出 JSON 数组。"
    "每个元素可以是字符串规则，或对象 {rule, rationale}。"
    "如果样本不足或无法提取出跨场景的通用性，必须返回 []，严禁强行总结具体事件。"
    "仅输出 JSON，不要 markdown。"
)


class DistillerWorker(BaseWorker):
    worker_type = "distiller"
    poll_interval = 7200.0

    def __init__(
        self,
        db_path: str | None = None,
        poll_interval: float | None = None,
        conn: MemoryStore | None = None,
    ) -> None:
        super().__init__(db_path=db_path, poll_interval=poll_interval, conn=conn)
        self.model = os.environ.get("DISTILLER_MODEL", extractor.SILICONFLOW_MODEL)

    @staticmethod
    def _parse_rules(content: str) -> list[str]:
        clean = content.strip()
        if clean.startswith("```"):
            clean = clean.split("\n", 1)[1].rsplit("```", 1)[0]
        try:
            data = json.loads(clean.strip())
        except json.JSONDecodeError:
            return []

        if not isinstance(data, list):
            return []

        rules: list[str] = []
        for item in data:
            if isinstance(item, str):
                text = item.strip()
            elif isinstance(item, dict):
                text = str(item.get("rule", "")).strip() or json.dumps(item, ensure_ascii=False)
            else:
                text = ""
            if text:
                rules.append(text)
        return rules

    async def process(self, payload: dict[str, Any]) -> None:
        _ = payload
        count = store.count_memories_by_source(
            self.db_path,
            source="causal",
            path_prefix="/behavior/corrections",
            include_archived=False,
        )
        if count < 5:
            return

        causal_memories = store.list_memories_by_source(
            self.db_path,
            source="causal",
            path_prefix="/behavior/corrections",
            include_archived=False,
            limit=2000,
        )
        if len(causal_memories) < 5:
            return

        sample_text = "\n\n".join(
            f"[{idx + 1}] {m.get('text', '').strip()}"
            for idx, m in enumerate(causal_memories)
            if str(m.get("text", "")).strip()
        )
        if not sample_text:
            return

        content = await extractor._call_llm(
            messages=[
                {"role": "system", "content": DISTILLER_PROMPT},
                {"role": "user", "content": sample_text},
            ],
            model=self.model,
            temperature=0.1,
            max_tokens=2000,
            timeout=120,
        )

        rules = self._parse_rules(content)
        if not rules:
            return

        vectors = await embedding.embed_batch(rules, input_type="document")
        summaries: list[str] = []
        for rule in rules:
            summaries.append(await extractor.generate_summary(rule))

        conn = self._conn
        for rule_text, vec, summary in zip(rules, vectors, summaries):
            store.save_memory(
                conn,
                rule_text,
                vec,
                path="/behavior/global_rules",
                summary=summary,
                topic="global_rule",
                keywords=["rule", "distillation"],
                scope="general",
                importance=0.95,
                source="distillation",
                metadata={"origin": "distillation", "state": "DRAFT"},
            )

    async def run_loop(self) -> None:
        while True:
            try:
                await self.process({})
            except Exception:
                logger.exception("Worker %s periodic distillation failed", self.worker_type)
            await asyncio.sleep(self.poll_interval)
