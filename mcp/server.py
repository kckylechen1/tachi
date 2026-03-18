"""
Antigravity Memory MCP Server
A lightweight persistent memory system using SQLite + Voyage-4 + GLM-5.

Tools:
  - save_memory: Store a fact/memory with vector embedding (L0 summary auto-generated)
  - search_memory: Hybrid search (vector + FTS5 + recency), supports path filtering
  - get_memory: Get full memory text by ID (L2 layer)
  - list_memories: Browse memory paths hierarchically (like ls)
  - ingest_event: Enqueue conversation events for async extractor/causal workers
  - extract_facts: Extract facts from text via GLM-5 and save them
  - memory_stats: Get memory database statistics
  - get_pipeline_status: Check async pipeline health and cutover readiness
"""

import asyncio
import hashlib
import json
import logging
import os
import sys
import httpx
from pathlib import Path

from dotenv import load_dotenv

load_dotenv(os.path.expanduser("~/.secrets/master.env"))

# Load .env from project root (two levels up from mcp/server.py)
# Must happen BEFORE importing modules that read os.environ at import time.
_project_root = Path(__file__).resolve().parent.parent
load_dotenv(_project_root / ".env")

from mcp.server import Server
from mcp.server.stdio import stdio_server
from mcp.types import Tool, TextContent

import store
import embedding
from event_queue import enqueue, init_event_queue
from shadow.consistency_check import build_pipeline_health
from workers.launcher import WorkerLauncher

logging.basicConfig(level=logging.INFO, stream=sys.stderr)
logger = logging.getLogger("memory-mcp")

app = Server("antigravity-memory")
_STORE_CONN = store.get_connection()
ENABLE_PIPELINE = os.environ.get("ENABLE_PIPELINE", "false").lower() in (
    "true",
    "1",
    "yes",
)
_RETRYABLE_STATUS_CODES = {408, 409, 429, 500, 502, 503, 504}


def _env_int(name: str, default: int) -> int:
    try:
        return int(os.environ.get(name, default))
    except (TypeError, ValueError):
        return default


def _env_float(name: str, default: float) -> float:
    try:
        return float(os.environ.get(name, default))
    except (TypeError, ValueError):
        return default


_EMBED_RETRY_ATTEMPTS = max(1, _env_int("EMBED_RETRY_ATTEMPTS", 3))
_EMBED_RETRY_BASE_DELAY = max(0.1, _env_float("EMBED_RETRY_BASE_DELAY", 1.5))
_SUMMARY_CONCURRENCY = max(1, _env_int("EXTRACT_FACTS_SUMMARY_CONCURRENCY", 3))
_SUMMARY_TIMEOUT_SECONDS = max(
    5.0, _env_float("EXTRACT_FACTS_SUMMARY_TIMEOUT_SECONDS", 45.0)
)


def _is_retriable_http_error(exc: Exception) -> bool:
    if isinstance(exc, (httpx.TimeoutException, httpx.TransportError)):
        return True
    if isinstance(exc, httpx.HTTPStatusError):
        code = exc.response.status_code if exc.response else None
        return code in _RETRYABLE_STATUS_CODES
    return False


def _summary_fallback(text: str) -> str:
    return text[:80] + "..." if len(text) > 80 else text


async def _embed_single_with_retry(text: str) -> list[float]:
    for attempt in range(1, _EMBED_RETRY_ATTEMPTS + 1):
        try:
            return await embedding.embed(text, input_type="document")
        except Exception as exc:
            if not _is_retriable_http_error(exc) or attempt >= _EMBED_RETRY_ATTEMPTS:
                raise
            await asyncio.sleep(_EMBED_RETRY_BASE_DELAY * (2 ** (attempt - 1)))
    raise RuntimeError("unreachable")


async def _embed_batch_with_retry(texts: list[str]) -> list[list[float]]:
    for attempt in range(1, _EMBED_RETRY_ATTEMPTS + 1):
        try:
            return await embedding.embed_batch(texts, input_type="document")
        except Exception as exc:
            if not _is_retriable_http_error(exc):
                raise
            if attempt >= _EMBED_RETRY_ATTEMPTS:
                break
            await asyncio.sleep(_EMBED_RETRY_BASE_DELAY * (2 ** (attempt - 1)))
    vectors: list[list[float]] = []
    for t in texts:
        vectors.append(await _embed_single_with_retry(t))
    return vectors


async def _generate_summaries(texts: list[str]) -> list[str]:
    """生成摘要。修复：使用本地截断作为 fallback，减少 LLM 调用"""
    import extractor  # Lazy import: only needed for explicit fact extraction

    sem = asyncio.Semaphore(max(1, _SUMMARY_CONCURRENCY))  # 减少并发到1，避免资源争抢

    async def _one(t: str) -> str:
        # 如果文本已经很短，直接截断，不调用 LLM
        if len(t) <= 80:
            return t

        async with sem:
            try:
                # 缩短超时到 15 秒，失败快速 fallback
                return await asyncio.wait_for(
                    extractor.generate_summary(t),
                    timeout=min(15.0, _SUMMARY_TIMEOUT_SECONDS),
                )
            except Exception:
                # 快速 fallback 到本地截断
                return _summary_fallback(t)

    return await asyncio.gather(*(_one(t) for t in texts))


def _validate_args(tool_name: str, args: dict | None) -> dict:
    """Validate MCP tool arguments with lightweight schema checks."""
    if args is None:
        args = {}
    if not isinstance(args, dict):
        raise ValueError(
            f"invalid arguments for {tool_name}: body must be a JSON object"
        )

    errors: list[str] = []

    def _require_str(field: str) -> None:
        if field not in args:
            errors.append(f"missing required field '{field}'")
            return
        value = args.get(field)
        if not isinstance(value, str):
            errors.append(f"'{field}' must be a string")
            return
        if not value.strip():
            errors.append(f"'{field}' must be a non-empty string")

    def _optional_str(field: str) -> None:
        if field in args and not isinstance(args.get(field), str):
            errors.append(f"'{field}' must be a string")

    def _optional_int(field: str, min_value: int | None = None) -> None:
        if field not in args:
            return
        value = args.get(field)
        if not isinstance(value, int) or isinstance(value, bool):
            errors.append(f"'{field}' must be an integer")
            return
        if min_value is not None and value < min_value:
            op = ">=" if min_value == 0 else ">"
            bound = min_value if min_value == 0 else min_value - 1
            errors.append(f"'{field}' must be {op} {bound}")

    def _optional_number_range(field: str, low: float, high: float) -> None:
        if field not in args:
            return
        value = args.get(field)
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            errors.append(f"'{field}' must be a number")
            return
        fv = float(value)
        if fv < low or fv > high:
            errors.append(f"'{field}' must be between {low} and {high}")

    if tool_name == "save_memory":
        _require_str("text")
        _optional_str("path")
        _optional_str("topic")
        _optional_str("scope")
        _optional_number_range("importance", 0.0, 1.0)
        if "keywords" in args:
            keywords = args.get("keywords")
            if not isinstance(keywords, list):
                errors.append("'keywords' must be an array of strings")
            elif any(not isinstance(k, str) for k in keywords):
                errors.append("'keywords' must contain only strings")
    elif tool_name == "search_memory":
        _require_str("query")
        _optional_int("top_k", min_value=1)
        _optional_str("path_prefix")
    elif tool_name in {"get_memory", "delete_memory"}:
        _require_str("id")
    elif tool_name == "list_memories":
        _optional_str("path")
        _optional_int("offset", min_value=0)
        _optional_int("limit", min_value=1)
    elif tool_name == "extract_facts":
        _require_str("text")
        _optional_str("source")
    elif tool_name == "ingest_event":
        _require_str("conversation_id")
        _require_str("turn_id")
        if "messages" not in args:
            errors.append("missing required field 'messages'")
        elif not isinstance(args.get("messages"), list):
            errors.append("'messages' must be an array")
    elif tool_name == "get_pipeline_status":
        _optional_int("period_hours", min_value=1)
    elif tool_name in {"set_state", "get_state"}:
        _require_str("key")
        _optional_str("namespace")
        if tool_name == "set_state" and "value" not in args:
            errors.append("missing required field 'value'")

    if errors:
        raise ValueError(f"invalid arguments for {tool_name}: {'; '.join(errors)}")
    return args


@app.list_tools()
async def list_tools() -> list[Tool]:
    return [
        Tool(
            name="save_memory",
            description="Save a fact or memory for long-term retrieval. Generates L0 summary and Voyage-4 vector.",
            inputSchema={
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The full fact or memory to save (L2)",
                    },
                    "path": {
                        "type": "string",
                        "description": "Hierarchical path, e.g. /project/openclaw/docs",
                        "default": "/",
                    },
                    "topic": {
                        "type": "string",
                        "description": "Deprecated fallback for topic",
                        "default": "",
                    },
                    "keywords": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Relevant keywords",
                        "default": [],
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["user", "project", "general"],
                        "description": "Deprecated fallback for scope",
                        "default": "general",
                    },
                    "importance": {
                        "type": "number",
                        "description": "Importance 0.0-1.0",
                        "default": 0.7,
                    },
                },
                "required": ["text"],
            },
        ),
        Tool(
            name="search_memory",
            description="Hybrid search returning L0 summaries. To view full text, use get_memory.",
            inputSchema={
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"},
                    "path_prefix": {
                        "type": "string",
                        "description": "Optional path prefix to filter (e.g. /project/openclaw)",
                        "default": "",
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Max results to return",
                        "default": 6,
                    },
                },
                "required": ["query"],
            },
        ),
        Tool(
            name="get_memory",
            description="Retrieve the full L2 original text of a memory by its ID.",
            inputSchema={
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Memory ID returned from search_memory or list_memories",
                    },
                },
                "required": ["id"],
            },
        ),
        Tool(
            name="list_memories",
            description="List memories and sub-directories under a given path recursively (1 level deep) like 'ls'.",
            inputSchema={
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The path directory to list, e.g. / or /project",
                        "default": "/",
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Offset for pagination",
                        "default": 0,
                        "minimum": 0,
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results for pagination",
                        "default": 100,
                        "minimum": 1,
                    },
                },
                "required": [],
            },
        ),
        Tool(
            name="extract_facts",
            description="Extract structured facts from text using GLM-5 and save to memory.",
            inputSchema={
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Conversation or document text",
                    },
                    "source": {
                        "type": "string",
                        "description": "Source label",
                        "default": "extraction",
                    },
                },
                "required": ["text"],
            },
        ),
        Tool(
            name="ingest_event",
            description="Ingest one conversation turn and enqueue async worker tasks.",
            inputSchema={
                "type": "object",
                "properties": {
                    "conversation_id": {
                        "type": "string",
                        "description": "Conversation/session ID",
                    },
                    "turn_id": {
                        "type": "string",
                        "description": "Turn ID within conversation",
                    },
                    "messages": {
                        "type": "array",
                        "description": "Turn messages, usually [{role, content}, ...]",
                        "items": {
                            "anyOf": [
                                {"type": "string"},
                                {
                                    "type": "object",
                                    "properties": {
                                        "role": {"type": "string"},
                                        "content": {"type": "string"},
                                    },
                                    "required": ["content"],
                                },
                            ]
                        },
                    },
                },
                "required": ["conversation_id", "turn_id", "messages"],
            },
        ),
        Tool(
            name="memory_stats",
            description="Get memory database statistics.",
            inputSchema={
                "type": "object",
                "properties": {},
            },
        ),
        Tool(
            name="get_pipeline_status",
            description="Get async pipeline health and cutover gate status.",
            inputSchema={
                "type": "object",
                "properties": {
                    "period_hours": {
                        "type": "integer",
                        "description": "Event stats window in hours",
                        "default": 24,
                    },
                },
            },
        ),
        Tool(
            name="set_state",
            description="Set a deterministic key-value state. No vector search, no LLM. For structured data like trading positions, watchlists, configs.",
            inputSchema={
                "type": "object",
                "properties": {
                    "namespace": {
                        "type": "string",
                        "description": "State namespace, e.g. 'trading', 'config'",
                        "default": "default",
                    },
                    "key": {
                        "type": "string",
                        "description": "State key, e.g. 'watchlist', 'fund_position'",
                    },
                    "value": {"description": "Any JSON-serializable value to store"},
                },
                "required": ["key", "value"],
            },
        ),
        Tool(
            name="get_state",
            description="Get a deterministic key-value state by namespace+key. No vector search.",
            inputSchema={
                "type": "object",
                "properties": {
                    "namespace": {
                        "type": "string",
                        "description": "State namespace",
                        "default": "default",
                    },
                    "key": {"type": "string", "description": "State key to retrieve"},
                },
                "required": ["key"],
            },
        ),
    ]


@app.call_tool()
async def call_tool(name: str, arguments: dict | None) -> list[TextContent]:
    try:
        if name not in {
            "save_memory",
            "search_memory",
            "get_memory",
            "list_memories",
            "extract_facts",
            "ingest_event",
            "memory_stats",
            "get_pipeline_status",
            "set_state",
            "get_state",
        }:
            return [TextContent(type="text", text=f"Unknown tool: {name}")]

        validated_args = _validate_args(name, arguments)

        if name == "save_memory":
            return await _save_memory(validated_args)
        elif name == "search_memory":
            return await _search_memory(validated_args)
        elif name == "get_memory":
            return await _get_memory(validated_args)
        elif name == "list_memories":
            return await _list_memories(validated_args)
        elif name == "extract_facts":
            return await _extract_facts(validated_args)
        elif name == "ingest_event":
            return await _ingest_event(validated_args)
        elif name == "memory_stats":
            return await _memory_stats()
        elif name == "get_pipeline_status":
            return await _get_pipeline_status(validated_args)
        elif name == "set_state":
            return await _set_state(validated_args)
        elif name == "get_state":
            return await _get_state(validated_args)
    except Exception as e:
        logger.exception(f"Error in {name}")
        return [
            TextContent(
                type="text",
                text=f"Error: {type(e).__name__}: {e or 'timeout or empty error'}",
            )
        ]


async def _save_memory(args: dict) -> list[TextContent]:
    text = args["text"].strip()
    if not text:
        return [TextContent(type="text", text="Error: empty text")]

    path = args.get("path", "")
    scope = args.get("scope", "general")
    topic = args.get("topic", "")

    if not path or path == "/":
        path = f"/{scope}"
        if topic:
            path += f"/{topic.replace(' ', '_').replace('/', '_')}"

    # Phase 1: Use local fallback for L0 summary instead of LLM call
    summary = _summary_fallback(text)
    vec = await embedding.embed(text, input_type="document")

    conn = _STORE_CONN
    try:
        result = store.save_memory(
            conn,
            text,
            vec,
            path=path,
            summary=summary,
            topic=topic,
            keywords=args.get("keywords", []),
            scope=scope,
            importance=args.get("importance", 0.7),
            source="manual",
        )
        if result is None:
            return [
                TextContent(
                    type="text",
                    text="⏭ Skipped: similar memory already exists (cosine >= 0.92)",
                )
            ]
        return [
            TextContent(
                type="text",
                text=f"✅ Saved to {result['path']}: [{result['topic']}] {result['summary']} (id: {result['id']})",
            )
        ]
    except Exception as e:
        logger.exception("Error in save_memory")
        return [TextContent(type="text", text=f"Error: {e}")]


async def _search_memory(args: dict) -> list[TextContent]:
    query = args["query"].strip()
    if not query:
        return [TextContent(type="text", text="Error: empty query")]

    top_k = args.get("top_k", 6)
    path_prefix = args.get("path_prefix", "")

    vec = await embedding.embed(query, input_type="query")
    conn = _STORE_CONN
    try:
        candidates = store.hybrid_search(
            conn, vec, query, top_k=top_k, path_prefix=path_prefix
        )
        if not candidates:
            return [TextContent(type="text", text="No relevant memories found.")]

        results = candidates

        lines = []
        for i, r in enumerate(results, 1):
            score_pct = int(r["score"] * 100)
            # Only return the L0 summary
            lines.append(
                f"{i}. [id:{r['id']}] [{r['path']}] ({score_pct}%)\n   <Summary>: {r['summary']}"
            )
        return [TextContent(type="text", text="\n".join(lines))]
    except Exception as e:
        logger.exception("Error in search_memory")
        return [TextContent(type="text", text=f"Error: {e}")]


async def _get_memory(args: dict) -> list[TextContent]:
    id_ = args.get("id", "").strip()
    if not id_:
        return [TextContent(type="text", text="Error: empty id")]

    conn = _STORE_CONN
    try:
        mem = store.get_memory(conn, id_)
        if not mem:
            return [TextContent(type="text", text=f"Memory ID {id_} not found.")]

        created = mem.get("created_at", mem.get("timestamp", ""))
        out = f"ID: {mem['id']}\nPath: {mem['path']}\nCreated: {created}\nSummary: {mem['summary']}\n---\n{mem['text']}"
        return [TextContent(type="text", text=out)]
    except Exception as e:
        logger.exception("Error in get_memory")
        return [TextContent(type="text", text=f"Error: {e}")]


async def _list_memories(args: dict) -> list[TextContent]:
    path = args.get("path", "/")

    conn = _STORE_CONN
    try:
        res = store.list_by_path(conn, path)

        lines = [f"Path: {res['path']}"]
        if res["directories"]:
            lines.append("Directories:")
            for d in res["directories"]:
                lines.append(f"  📁 {d}/")

        if res["memories"]:
            lines.append("Memories:")
            for m in res["memories"]:
                lines.append(f"  📄 [id:{m['id']}] {m['summary']}")

        if not res["directories"] and not res["memories"]:
            lines.append("(Empty)")

        return [TextContent(type="text", text="\n".join(lines))]
    except Exception as e:
        logger.exception("Error in list_memories")
        return [TextContent(type="text", text=f"Error: {e}")]


async def _extract_facts(args: dict) -> list[TextContent]:
    import extractor  # Lazy import: only needed for explicit fact extraction

    text = args["text"].strip()
    if not text:
        return [TextContent(type="text", text="Error: empty text")]

    source = args.get("source", "extraction")

    # Extract facts via GLM-5
    try:
        facts = await extractor.extract_facts(text)
    except Exception as e:
        logger.exception("Error during extraction stage")
        return [
            TextContent(
                type="text", text=f"Error: extraction failed ({type(e).__name__}: {e})"
            )
        ]

    if not facts:
        return [TextContent(type="text", text="No facts extracted.")]

    # Generate vector + L0 summary for each
    texts = [f["text"] for f in facts]
    try:
        vectors = await _embed_batch_with_retry(texts)
    except Exception as e:
        logger.exception("Error during embedding stage")
        return [
            TextContent(
                type="text", text=f"Error: embedding failed ({type(e).__name__}: {e})"
            )
        ]
    summaries = await _generate_summaries(texts)

    conn = _STORE_CONN
    saved, skipped = 0, 0
    try:
        for fact, vec, summ in zip(facts, vectors, summaries):
            scope = fact.get("scope", "general")
            topic = fact.get("topic", "")
            path = f"/{scope}"
            if topic:
                path += f"/{topic.replace(' ', '_').replace('/', '_')}"

            result = store.save_memory(
                conn,
                fact["text"],
                vec,
                path=path,
                summary=summ,
                topic=topic,
                keywords=fact.get("keywords", []),
                scope=scope,
                importance=fact.get("importance", 0.7),
                source=source,
            )
            if result:
                saved += 1
            else:
                skipped += 1
    except Exception as e:
        logger.exception("Error in extract_facts")
        return [TextContent(type="text", text=f"Error: {e}")]

    summary_lines = [
        f"📝 Extracted {len(facts)} facts → ✅ {saved} saved, ⏭ {skipped} duplicates"
    ]
    for i, f in enumerate(facts, 1):
        summary_lines.append(
            f"  {i}. [path:/{f.get('scope', '?')}...] {f['text'][:80]}"
        )
    return [TextContent(type="text", text="\n".join(summary_lines))]


async def _ingest_event(args: dict) -> list[TextContent]:
    conversation_id = str(args.get("conversation_id", "")).strip()
    turn_id = str(args.get("turn_id", "")).strip()
    messages = args.get("messages", [])

    if not conversation_id:
        return [TextContent(type="text", text="Error: empty conversation_id")]
    if not turn_id:
        return [TextContent(type="text", text="Error: empty turn_id")]
    if not isinstance(messages, list):
        return [TextContent(type="text", text="Error: messages must be an array")]

    event_id = hashlib.sha256(
        f"{conversation_id}|{turn_id}".encode("utf-8")
    ).hexdigest()
    payload = {
        "event_id": event_id,
        "conversation_id": conversation_id,
        "turn_id": turn_id,
        "messages": messages,
    }

    init_event_queue(store.DB_PATH)
    if ENABLE_PIPELINE:
        enqueue(store.DB_PATH, event_id, "extractor", payload)
        enqueue(store.DB_PATH, event_id, "causal", payload)

    return [
        TextContent(
            type="text", text=json.dumps({"event_id": event_id}, ensure_ascii=False)
        )
    ]


async def _memory_stats() -> list[TextContent]:
    conn = _STORE_CONN
    try:
        stats = store.get_stats(conn)
        return [
            TextContent(
                type="text", text=json.dumps(stats, indent=2, ensure_ascii=False)
            )
        ]
    except Exception as e:
        logger.exception("Error in memory_stats")
        return [TextContent(type="text", text=f"Error: {e}")]


async def _get_pipeline_status(args: dict) -> list[TextContent]:
    raw_period = args.get("period_hours", 24)
    try:
        period_hours = int(raw_period)
    except (TypeError, ValueError):
        return [TextContent(type="text", text="Error: period_hours must be an integer")]

    if period_hours <= 0:
        return [TextContent(type="text", text="Error: period_hours must be > 0")]

    try:
        health = build_pipeline_health(period_hours=period_hours, db_path=store.DB_PATH)
        return [
            TextContent(
                type="text", text=json.dumps(health, indent=2, ensure_ascii=False)
            )
        ]
    except Exception as e:
        logger.exception("Error in get_pipeline_status")
        return [TextContent(type="text", text=f"Error: {e}")]


async def _set_state(args: dict) -> list[TextContent]:
    namespace = args.get("namespace", "default")
    key = args.get("key", "").strip()
    if not key:
        return [TextContent(type="text", text="Error: empty key")]
    value = args.get("value")
    try:
        result = store.set_state(
            db_path=store.DB_PATH,
            namespace=namespace,
            key=key,
            value=value,
            modified_by="mcp",
        )
        return [
            TextContent(
                type="text",
                text=f"✅ State set: {namespace}/{key} (v{result['version']})",
            )
        ]
    except Exception as e:
        logger.exception("Error in set_state")
        return [TextContent(type="text", text=f"Error: {e}")]


async def _get_state(args: dict) -> list[TextContent]:
    namespace = args.get("namespace", "default")
    key = args.get("key", "").strip()
    if not key:
        return [TextContent(type="text", text="Error: empty key")]
    try:
        result = store.get_state(db_path=store.DB_PATH, namespace=namespace, key=key)
        if result is None:
            return [
                TextContent(type="text", text=f"State not found: {namespace}/{key}")
            ]
        return [
            TextContent(
                type="text", text=json.dumps(result, indent=2, ensure_ascii=False)
            )
        ]
    except Exception as e:
        logger.exception("Error in get_state")
        return [TextContent(type="text", text=f"Error: {e}")]


async def main():
    logger.info("Starting Antigravity Memory MCP server (v2)...")
    init_event_queue(store.DB_PATH)

    # Phase 2+3: Ensure new tables exist and migrate causal data
    store.ensure_hard_state_table(store.DB_PATH)
    store.ensure_derived_items_table(store.DB_PATH)
    migrated = store.migrate_causal_to_derived(store.DB_PATH)
    if migrated > 0:
        logger.info(
            f"Phase 3 migration: moved {migrated} causal records to derived_items"
        )

    # Phase 1: Feature flag to disable async pipeline workers
    launcher = WorkerLauncher(db_path=store.DB_PATH, conn=_STORE_CONN)
    if ENABLE_PIPELINE:
        launcher.start()
        logger.info("Pipeline workers ENABLED")
    else:
        logger.info("Pipeline workers DISABLED (set ENABLE_PIPELINE=true to enable)")
    try:
        async with stdio_server() as (read_stream, write_stream):
            await app.run(
                read_stream, write_stream, app.create_initialization_options()
            )
    finally:
        await launcher.stop()


if __name__ == "__main__":
    asyncio.run(main())
