"""
GLM-5 fact extractor for Antigravity Memory MCP.
Aligned with OpenClaw memory-hybrid-bridge extraction prompt.
"""

import asyncio
import atexit
import os
import json
import httpx
from dotenv import load_dotenv

load_dotenv(os.path.expanduser("~/.secrets/master.env"))

SILICONFLOW_API_KEY = os.environ.get("SILICONFLOW_API_KEY", "") or os.environ.get(
    "ZHIPU_API_KEY", ""
)
SILICONFLOW_BASE_URL = os.environ.get(
    "SILICONFLOW_BASE_URL",
    os.environ.get("EXTRACTOR_BASE_URL", "https://api.siliconflow.cn/v1"),
)
SILICONFLOW_MODEL = os.environ.get(
    "SILICONFLOW_MODEL",
    os.environ.get(
        "EXTRACTOR_MODEL", "Qwen/Qwen2.5-7B-Instruct"
    ),  # 7B速度快质量够用，3.5系列thinking模式无法关闭
)
SUMMARY_MODEL = os.environ.get("SUMMARY_MODEL", SILICONFLOW_MODEL)
_RETRYABLE_STATUS_CODES = {408, 409, 429, 500, 502, 503, 504}


def _env_float(name: str, default: float) -> float:
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        return float(raw)
    except ValueError:
        return default


def _env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        return int(raw)
    except ValueError:
        return default


_global_client: httpx.AsyncClient | None = None


def _close_global_client_sync() -> None:
    """同步关闭全局 client（用于 atexit）"""
    global _global_client
    if _global_client and not _global_client.is_closed:
        try:
            loop = asyncio.get_running_loop()
            loop.create_task(_global_client.aclose())
        except RuntimeError:
            pass


atexit.register(_close_global_client_sync)


def _get_global_client(timeout: float) -> httpx.AsyncClient:
    """获取全局 AsyncClient 实例，复用连接池"""
    global _global_client
    if _global_client is None or _global_client.is_closed:
        _global_client = httpx.AsyncClient(
            timeout=httpx.Timeout(
                connect=_env_float("SILICONFLOW_CONNECT_TIMEOUT", 5.0),
                read=timeout,
                write=_env_float("SILICONFLOW_WRITE_TIMEOUT", 10.0),
                pool=_env_float("SILICONFLOW_POOL_TIMEOUT", 10.0),
            ),
            limits=httpx.Limits(
                max_connections=20,
                max_keepalive_connections=10,
                keepalive_expiry=300.0,  # 5分钟长连接复用
            ),
            http2=False,
        )
    return _global_client


def _build_timeout(read_timeout: float) -> httpx.Timeout:
    """保持兼容性，但建议使用 _get_global_client"""
    return httpx.Timeout(
        connect=_env_float("SILICONFLOW_CONNECT_TIMEOUT", 5.0),
        read=read_timeout,
        write=_env_float("SILICONFLOW_WRITE_TIMEOUT", 10.0),
        pool=_env_float("SILICONFLOW_POOL_TIMEOUT", 10.0),
    )


EXTRACTION_PROMPT = """你是一个记忆提取代理。从对话/文档中提取值得**长期记忆**的离散事实。

输出 JSON 数组，每个元素:
- "text": 极简事实（主谓宾，≤30字，删除"刚才/之前/舰长"等口语词）
- "topic": 主题标签
- "keywords": 2-5个关键词
- "scope": "user" / "project" / "general"
- "importance": 0.0-1.0

核心规则:
1) 合并同类：同一根因的多个描述合并为一条，但不同根因保留为独立事实
2) 只留结论：忽略过程描述，保留最终状态/决策/根因
3) 宁少勿多：一段话通常1-3条，但技术上独立的问题不应强行合并
4) 不编造，仅输出 JSON 数组"""


def _strip_code_fence(text: str) -> str:
    clean = text.strip()
    if clean.startswith("```"):
        clean = clean.split("\n", 1)[1].rsplit("```", 1)[0]
    return clean.strip()


async def _call_llm(
    messages: list[dict],
    model: str | None = None,
    temperature: float = 0.1,
    max_tokens: int = 4000,
    timeout: float = 120,
    retries: int | None = None,
    retry_base_delay: float | None = None,
) -> str:
    """Shared LLM caller used by extractors/workers."""
    if not SILICONFLOW_API_KEY:
        raise ValueError("SILICONFLOW_API_KEY not set")

    max_attempts = (
        retries
        if retries is not None
        else max(1, _env_int("SILICONFLOW_RETRY_ATTEMPTS", 2))  # 减少重试
    )
    base_delay = (
        retry_base_delay
        if retry_base_delay is not None
        else _env_float("SILICONFLOW_RETRY_BASE_DELAY", 1.0)  # 减少退避
    )

    # 使用全局 client 复用连接池
    client = _get_global_client(timeout)

    for attempt in range(1, max_attempts + 1):
        try:
            r = await client.post(
                f"{SILICONFLOW_BASE_URL}/chat/completions",
                headers={
                    "Authorization": f"Bearer {SILICONFLOW_API_KEY}",
                    "Content-Type": "application/json",
                },
                json={
                    "model": model or SILICONFLOW_MODEL,
                    "temperature": temperature,
                    "max_tokens": max_tokens,
                    "messages": messages,
                },
            )
            r.raise_for_status()
            return r.json()["choices"][0]["message"]["content"]
        except httpx.HTTPStatusError as exc:
            code = exc.response.status_code if exc.response else None
            if code not in _RETRYABLE_STATUS_CODES or attempt >= max_attempts:
                raise
        except (httpx.TimeoutException, httpx.TransportError) as e:
            if attempt >= max_attempts:
                raise
            # 不要关闭 client，让连接保持，下次请求会自动重连
        await asyncio.sleep(base_delay * (2 ** (attempt - 1)))
    raise RuntimeError("unreachable")


async def extract_facts(text: str) -> list[dict]:
    """
    Extract structured facts from text using GLM-5.
    Returns list of fact dicts with text, topic, keywords, scope, importance.
    """
    if not SILICONFLOW_API_KEY:
        raise ValueError("SILICONFLOW_API_KEY not set")

    if len(text.strip()) < 50:
        return []

    content = await _call_llm(
        messages=[
            {"role": "system", "content": EXTRACTION_PROMPT},
            {"role": "user", "content": text},
        ],
        model=SILICONFLOW_MODEL,
        temperature=0.1,
        max_tokens=max(200, _env_int("EXTRACTOR_MAX_TOKENS", 500)),  # 减少tokens
        timeout=_env_float("EXTRACTOR_TIMEOUT_SECONDS", 30.0),  # 大幅缩短超时
        retries=max(1, _env_int("EXTRACTOR_RETRY_ATTEMPTS", 2)),  # 减少重试
        retry_base_delay=_env_float("EXTRACTOR_RETRY_BASE_DELAY", 1.0),
    )

    # Parse JSON (handle markdown wrapping)
    clean = _strip_code_fence(content)

    try:
        facts = json.loads(clean.strip())
        if not isinstance(facts, list):
            return []
    except json.JSONDecodeError:
        return []

    # Validate and normalize
    valid = []
    for f in facts:
        if not isinstance(f, dict) or "text" not in f:
            continue
        valid.append(
            {
                "text": str(f["text"]),
                "topic": str(f.get("topic", "")),
                "keywords": f.get("keywords", [])
                if isinstance(f.get("keywords"), list)
                else [],
                "scope": f.get("scope", "general")
                if f.get("scope") in ("user", "project", "general")
                else "general",
                "importance": min(1.0, max(0.0, float(f.get("importance", 0.7)))),
            }
        )

    return valid


async def generate_summary(text: str) -> str:
    """Generate a one-sentence L0 summary using GLM-5/4-flash."""
    if not SILICONFLOW_API_KEY:
        return text[:80] + "..." if len(text) > 80 else text
    if len(text.strip()) < 30:
        return text.strip()

    try:
        content = await _call_llm(
            messages=[
                {
                    "role": "system",
                    "content": "You are a summarization agent. Compress the given text into a single precisely worded sentence that captures the core fact or point. Do not use conversational filler, quotes, or markdown. Use the same language as the input text.",
                },
                {"role": "user", "content": text},
            ],
            model=SUMMARY_MODEL,
            temperature=0.1,
            max_tokens=100,
            timeout=_env_float("SUMMARY_TIMEOUT_SECONDS", 35.0),
            retries=max(1, _env_int("SUMMARY_RETRY_ATTEMPTS", 1)),
            retry_base_delay=_env_float("SUMMARY_RETRY_BASE_DELAY", 1.0),
        )
        return content.strip()
    except Exception:
        # Fallback
        return text[:80] + "..." if len(text) > 80 else text
