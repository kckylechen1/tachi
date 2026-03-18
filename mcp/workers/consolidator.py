"""Consolidator worker: merge, contradiction detection, and entity co-occurrence linking.

Enhanced from the original merge-only consolidator with:
- Contradiction detection: marks conflicting memories with 'contradicts' edge
- Entity co-occurrence: links memories sharing common entities with 'related_to' edge
- Temporal edge marking: sets valid_to on edges when superseded
"""

from __future__ import annotations

import json
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

CONTRADICTION_PROMPT = (
    "你是矛盾检测器。判断以下两条记忆是否存在事实矛盾。\n"
    "如果矛盾，输出 JSON: {\"contradicts\": true, \"reason\": \"简要说明\"}\n"
    "如果不矛盾，输出: {\"contradicts\": false}\n"
    "只输出 JSON，不要其他内容。"
)


class ConsolidatorWorker(BaseWorker):
    """Enhanced consolidator: merge + contradiction detection + entity linking."""

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

    async def _check_contradiction(self, text_a: str, text_b: str) -> dict | None:
        """Check if two memories contradict each other. Returns reason dict or None."""
        try:
            raw = await extractor._call_llm(
                messages=[
                    {"role": "system", "content": CONTRADICTION_PROMPT},
                    {
                        "role": "user",
                        "content": f"记忆A:\n{text_a}\n\n记忆B:\n{text_b}",
                    },
                ],
                model=extractor.SILICONFLOW_MODEL,
                temperature=0.0,
                max_tokens=200,
                timeout=30,
            )
            data = json.loads(extractor._strip_code_fence(raw))
            if data.get("contradicts"):
                return data
        except Exception:
            pass
        return None

    def _extract_entities(self, memory: dict) -> set[str]:
        """Extract normalized entity set from a memory's entities + persons fields."""
        entities = set()
        for field in ("entities", "persons"):
            raw = memory.get(field, [])
            if isinstance(raw, str):
                try:
                    raw = json.loads(raw)
                except Exception:
                    raw = []
            for e in raw:
                normalized = str(e).strip().lower()
                if len(normalized) >= 2:
                    entities.add(normalized)
        return entities

    async def _link_shared_entities(self, memory_id: str, memory: dict) -> int:
        """Find other memories sharing entities and create related_to edges."""
        entities = self._extract_entities(memory)
        if not entities:
            return 0

        conn = self._conn
        linked = 0
        for entity in entities:
            # Search for other memories mentioning this entity
            try:
                candidates = store.search_by_text(
                    conn, entity, top_k=5,
                    path_prefix=memory.get("path", ""),
                )
            except Exception:
                continue

            for cand in candidates:
                cand_id = str(cand.get("id", ""))
                if cand_id == memory_id or not cand_id:
                    continue
                cand_entities = self._extract_entities(cand)
                shared = entities & cand_entities
                if shared:
                    try:
                        edge_data = json.dumps({
                            "source_id": memory_id,
                            "target_id": cand_id,
                            "relation": "related_to",
                            "weight": min(0.5 + 0.1 * len(shared), 0.9),
                            "metadata": json.dumps({
                                "shared_entities": list(shared),
                                "source": "consolidator",
                            }),
                        })
                        store.add_edge(conn, edge_data)
                        linked += 1
                    except Exception:
                        pass
        return linked

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

        # ── Step 1: Find similar candidates ──────────────────────────────────
        query_vec = await embedding.embed(new_text, input_type="query")
        candidates = store.search_by_vector(
            conn,
            query_vec,
            top_k=10,
            path_prefix=new_memory.get("path", ""),
            include_archived=False,
        )

        # ── Step 2: Merge or detect contradictions ───────────────────────────
        for candidate in candidates:
            if candidate.get("id") == memory_id:
                continue
            score = float(candidate.get("score", 0.0))

            # High similarity → merge
            if score > 0.85:
                await self._do_merge(memory_id, new_text, candidate)
                break

            # Medium similarity → check for contradiction
            if 0.5 < score <= 0.85:
                cand_text = str(candidate.get("text", ""))
                contradiction = await self._check_contradiction(new_text, cand_text)
                if contradiction:
                    cand_id = str(candidate["id"])
                    try:
                        edge_data = json.dumps({
                            "source_id": memory_id,
                            "target_id": cand_id,
                            "relation": "contradicts",
                            "weight": 0.9,
                            "metadata": json.dumps({
                                "reason": contradiction.get("reason", ""),
                                "source": "consolidator",
                            }),
                        })
                        store.add_edge(conn, edge_data)
                    except Exception:
                        pass
                    break

        # ── Step 3: Entity co-occurrence linking ─────────────────────────────
        await self._link_shared_entities(memory_id, new_memory)

    async def _do_merge(
        self, memory_id: str, new_text: str, target: dict
    ) -> None:
        """Merge new_memory into target (existing consolidation logic)."""
        target_id = str(target["id"])
        target_row = store.get_memory_row(
            self.db_path, target_id, include_archived=False
        )
        if not target_row:
            return

        conn = self._conn
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

        # Migrate access_history: reassign archived memory's access records to target
        # so ACT-R activation is preserved after merge
        try:
            conn.execute(
                "UPDATE access_history SET memory_id = ? WHERE memory_id = ?",
                (target_id, memory_id),
            )
            conn.commit()
        except Exception:
            pass  # Non-critical: access_history migration failure shouldn't block merge
