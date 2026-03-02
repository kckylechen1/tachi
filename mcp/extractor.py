"""
GLM-5 fact extractor for Antigravity Memory MCP.
Aligned with OpenClaw memory-hybrid-bridge extraction prompt.
"""

import os
import json
import httpx

ZHIPU_API_KEY = os.environ.get("SILICONFLOW_API_KEY", "") or os.environ.get("ZHIPU_API_KEY", "")
ZHIPU_BASE_URL = os.environ.get("EXTRACTOR_BASE_URL", "https://api.siliconflow.cn/v1")
ZHIPU_MODEL = os.environ.get("EXTRACTOR_MODEL", "THUDM/glm-4-9b-chat")

EXTRACTION_PROMPT = """你是一个记忆提取代理。从以下对话/文档中提取值得长期记忆的离散事实。

对每个事实输出 JSON 对象:
- "text": 完整、精确的事实复述（与原文同语言）
- "topic": 简短主题标签
- "keywords": 2-5个关键词数组
- "scope": "user"（关于用户）/ "project"（关于技术工作）/ "general"（其他）
- "importance": 0.0-1.0（用户偏好/决策 > 0.8，一般事实 0.5-0.7，闲聊 < 0.3）

规则:
1) 不编造信息，只提取原文中明确存在的事实
2) 跳过问候、模糊陈述、格式标记
3) 仅输出 JSON 数组，不要 markdown 包装

输出格式: [{...}, {...}]"""


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
    if not ZHIPU_API_KEY:
        raise ValueError("ZHIPU_API_KEY not set")

    async with httpx.AsyncClient(timeout=timeout) as client:
        r = await client.post(
            f"{ZHIPU_BASE_URL}/chat/completions",
            headers={
                "Authorization": f"Bearer {ZHIPU_API_KEY}",
                "Content-Type": "application/json",
            },
            json={
                "model": model or ZHIPU_MODEL,
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
    if not ZHIPU_API_KEY:
        raise ValueError("ZHIPU_API_KEY not set")

    if len(text.strip()) < 50:
        return []

    content = await _call_llm(
        messages=[
            {"role": "system", "content": EXTRACTION_PROMPT},
            {"role": "user", "content": text},
        ],
        model=ZHIPU_MODEL,
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
    if not ZHIPU_API_KEY:
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
