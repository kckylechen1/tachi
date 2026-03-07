"""Consolidator worker: merge highly similar memories with optimistic locking."""

from __future__ import annotations

from typing import Any

import embedding
import extractor
import store

from .base import BaseWorker

MERGE_PROMPT = (
    "你是记忆合并器。输入两条语义高度相似的记忆。"
    "请输出一条去重后、信息不丢失、无冗余的合并文本。"
    "只输出最终文本，不要 JSON，不要 markdown。"
)


class ConsolidatorWorker(BaseWorker):
    worker_type = "consolidator"

    async def _merge_text(self, old_text: str, new_text: str) -> str:
        merged = await extractor._call_llm(
            messages=[
                {"role": "system", "content": MERGE_PROMPT},
                {
                    "role": "user",
                    "content": f"旧记忆:\n{old_text}\n\n新记忆:\n{new_text}",
                },
            ],
            model=extractor.SILICONFLOW_MODEL,
            temperature=0.1,
            max_tokens=800,
            timeout=90,
        )
        return merged.strip()

    async def process(self, payload: dict[str, Any]) -> None:
        memory_id = str(payload.get("memory_id", "")).strip()
        if not memory_id:
            return

        conn = self._conn
        new_memory = store.get_memory(conn, memory_id, include_archived=False)
        if not new_memory:
            return

        new_text = str(new_memory.get("text", "")).strip()
        if not new_text:
            return

        query_vec = await embedding.embed(new_text, input_type="query")
        candidates = store.search_by_vector(
            conn,
            query_vec,
            top_k=10,
            path_prefix=new_memory.get("path", ""),
            include_archived=False,
        )

        target = None
        for candidate in candidates:
            if candidate.get("id") == memory_id:
                continue
            if float(candidate.get("score", 0.0)) > 0.85:
                target = candidate
                break

        if not target:
            return

        target_id = str(target["id"])
        target_row = store.get_memory_row(self.db_path, target_id, include_archived=False)
        if not target_row:
            return

        old_memory = store.get_memory(conn, target_id, include_archived=False)
        if not old_memory:
            return

        old_text = str(old_memory.get("text", "")).strip()
        if not old_text:
            return

        merged_text = await self._merge_text(old_text, new_text)
        if not merged_text:
            return

        merged_summary = await extractor.generate_summary(merged_text)
        merged_vec = await embedding.embed(merged_text, input_type="document")

        ok = store.merge_memory_with_revision(
            db_path=self.db_path,
            target_id=target_id,
            expected_revision=int(target_row.get("revision", 1)),
            merged_text=merged_text,
            merged_summary=merged_summary,
            merged_vector=merged_vec,
            archive_id=memory_id,
        )
        if not ok:
            raise RuntimeError(
                f"Revision conflict merging {memory_id} into {target_id}, will retry"
            )
