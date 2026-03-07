"""
GLM-5 fact extractor for Antigravity Memory MCP.
Aligned with OpenClaw memory-hybrid-bridge extraction prompt.
"""

import os
import json
import httpx
from dotenv import load_dotenv

load_dotenv(os.path.expanduser("~/.secrets/master.env"))

SILICONFLOW_API_KEY = os.environ.get("SILICONFLOW_API_KEY", "") or os.environ.get("ZHIPU_API_KEY", "")
SILICONFLOW_BASE_URL = os.environ.get("SILICONFLOW_BASE_URL", os.environ.get("EXTRACTOR_BASE_URL", "https://api.siliconflow.cn/v1"))
SILICONFLOW_MODEL = os.environ.get("SILICONFLOW_MODEL", os.environ.get("EXTRACTOR_MODEL", "Qwen/Qwen3.5-27B"))

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
) -> str:
    """Shared LLM caller used by extractors/workers."""
    if not SILICONFLOW_API_KEY:
        raise ValueError("SILICONFLOW_API_KEY not set")

    async with httpx.AsyncClient(timeout=timeout) as client:
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
        max_tokens=4000,
        timeout=120,
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
        valid.append({
            "text": str(f["text"]),
            "topic": str(f.get("topic", "")),
            "keywords": f.get("keywords", []) if isinstance(f.get("keywords"), list) else [],
            "scope": f.get("scope", "general") if f.get("scope") in ("user", "project", "general") else "general",
            "importance": min(1.0, max(0.0, float(f.get("importance", 0.7)))),
        })

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
                {"role": "system", "content": "You are a summarization agent. Compress the given text into a single precisely worded sentence that captures the core fact or point. Do not use conversational filler, quotes, or markdown. Use the same language as the input text."},
                {"role": "user", "content": text},
            ],
            model="glm-4v-flash",
            temperature=0.1,
            max_tokens=100,
            timeout=30,
        )
        return content.strip()
    except Exception:
        # Fallback
        return text[:80] + "..." if len(text) > 80 else text
