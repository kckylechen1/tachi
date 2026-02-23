import { randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import type { BridgeConfig, MemoryEntry } from "./config.js";

// ============================================================================
// Input Sanitization (防注入)
// ============================================================================

const FALLBACK_PROMPT = `你是 memory_builder。任务：从输入窗口提炼 1 条可验证、可追溯的结构化记忆，输出严格 JSON（不要 markdown）。
字段要求：id, text, keywords[], timestamp(ISO8601), location, persons[], entities[], topic, metadata.source_refs[], summary(L0短句必填), path(当前路径，默认 "/openclaw/legacy").
约束：
1) 不要编造未知信息；无法确定的字段用空字符串或空数组。
2) metadata.source_refs 必须包含至少一条 {"ref_type":"message","ref_id":"..."}.
3) text 要忠实复述，不加入新事实。
4) summary 要在10到30个字以内提纲挈领。`;

function safeJsonParse<T>(text: string): T | null {
  try {
    return JSON.parse(text) as T;
  } catch {
    return null;
  }
}

function isValidIsoDate(s: string): boolean {
  return Number.isFinite(Date.parse(s));
}

export function sanitizeForExtractorInput(input: string): string {
  if (!input) return "";
  const noControlChars = input
    .replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g, "")
    .replace(/[\u200B-\u200F\u2060\uFEFF]/g, "");

  return noControlChars
    .replace(/```[\s\S]*?```/g, "[code-block-omitted]")
    .replace(
      /\b(ignore|override|bypass)\b[\s\S]{0,40}\b(instruction|system|prompt|policy)\b/gi,
      "[sanitized-instruction-pattern]",
    )
    .replace(/<\/?(system|developer|assistant|tool)[^>]*>/gi, "[sanitized-role-tag]")
    .trim();
}

export function validateMemoryEntry(obj: unknown): obj is MemoryEntry {
  if (!obj || typeof obj !== "object") return false;
  const e = obj as MemoryEntry;
  return Boolean(
    typeof e.id === "string" &&
    typeof e.text === "string" &&
    Array.isArray(e.keywords) &&
    typeof e.timestamp === "string" &&
    isValidIsoDate(e.timestamp) &&
    typeof e.location === "string" &&
    Array.isArray(e.persons) &&
    Array.isArray(e.entities) &&
    typeof e.topic === "string" &&
    e.metadata &&
    Array.isArray(e.metadata.source_refs) &&
    e.metadata.source_refs.length > 0,
  );
}

export async function loadPromptTemplate(promptPath: string): Promise<string> {
  try {
    const text = await fs.readFile(promptPath, "utf8");
    return text.trim() || FALLBACK_PROMPT;
  } catch {
    return FALLBACK_PROMPT;
  }
}

// ============================================================================
// LLM Extraction (提炼)
// ============================================================================

export async function extractMemoryEntry(params: {
  config: BridgeConfig;
  inputWindowText: string;
  sourceRefId: string;
  agentId: string;
  logger?: { info: (...args: any[]) => void; warn: (...args: any[]) => void };
}): Promise<MemoryEntry | null> {
  const { config, inputWindowText, sourceRefId, agentId, logger } = params;
  const prompt = await loadPromptTemplate(config.promptPath);
  const apiKey = config.extractor.apiKey;
  if (!apiKey) {
    logger?.warn("memory-hybrid-bridge: no extractor API key, skipping extraction");
    return null;
  }

  const sanitizedInput = sanitizeForExtractorInput(inputWindowText);
  if (!sanitizedInput) return null;

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), config.extractor.timeoutMs);

  try {
    const res = await fetch(`${config.extractor.baseUrl.replace(/\/$/, "")}/chat/completions`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${apiKey}`,
      },
      body: JSON.stringify({
        model: config.extractor.model,
        temperature: 0,
        messages: [
          { role: "system", content: prompt },
          {
            role: "user",
            content: `输入窗口(已清洗):\n${sanitizedInput}\n\nsource_ref_id=${sourceRefId}`,
          },
        ],
      }),
      signal: controller.signal,
    });

    if (!res.ok) {
      logger?.warn(`memory-hybrid-bridge: extractor API returned ${res.status}`);
      return null;
    }
    const data = (await res.json()) as any;
    const text = data?.choices?.[0]?.message?.content;
    if (typeof text !== "string") return null;

    const parsed = safeJsonParse<MemoryEntry>(text.trim());
    if (!parsed) return null;

    if (!parsed.id) parsed.id = randomUUID();
    if (!parsed.timestamp) parsed.timestamp = new Date().toISOString();
    if (!parsed.path) parsed.path = `/openclaw/agent-${agentId || "main"}`;
    if (!parsed.summary) parsed.summary = parsed.text.substring(0, 100);

    if (!parsed.metadata) parsed.metadata = { source_refs: [] };
    if (!Array.isArray(parsed.metadata.source_refs) || parsed.metadata.source_refs.length === 0) {
      parsed.metadata.source_refs = [{ ref_type: "message", ref_id: sourceRefId }];
    }

    return validateMemoryEntry(parsed) ? parsed : null;
  } catch (err) {
    logger?.warn(`memory-hybrid-bridge: extraction failed: ${String(err)}`);
    return null;
  } finally {
    clearTimeout(timer);
  }
}

// ============================================================================
// Embedding (向量化)
// ============================================================================

export async function getEmbedding(params: {
  config: BridgeConfig;
  text: string;
  logger?: { info: (...args: any[]) => void; warn: (...args: any[]) => void };
}): Promise<number[] | null> {
  const { config, text, logger } = params;
  const apiKey = config.embedding.apiKey;
  if (!apiKey) {
    logger?.warn("memory-hybrid-bridge: no embedding API key");
    return null;
  }

  try {
    const res = await fetch(`${config.embedding.baseUrl.replace(/\/$/, "")}/embeddings`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${apiKey}`,
      },
      body: JSON.stringify({
        model: config.embedding.model,
        input: text,
      }),
    });

    if (!res.ok) {
      logger?.warn(`memory-hybrid-bridge: embedding API returned ${res.status}`);
      return null;
    }

    const data = (await res.json()) as any;
    return data?.data?.[0]?.embedding ?? null;
  } catch (err) {
    logger?.warn(`memory-hybrid-bridge: embedding failed: ${String(err)}`);
    return null;
  }
}
