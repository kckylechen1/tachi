// prompts.rs — LLM prompt templates extracted from Python extractor.py / causal.py

/// Fact extraction system prompt (from Python extractor.py EXTRACTION_PROMPT)
pub const EXTRACTION_PROMPT: &str = r#"你是一个记忆提取代理。从对话/文档中提取值得**长期记忆**的离散事实。

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
4) 不编造，仅输出 JSON 数组"#;

/// L0 summary system prompt
pub const SUMMARY_PROMPT: &str = "You are a summarization agent. Compress the given text into a single precisely worded sentence that captures the core fact or point. Do not use conversational filler, quotes, or markdown. Use the same language as the input text.";

/// Causal extraction system prompt (from Python causal.py)
pub const CAUSAL_PROMPT: &str = r#"从对话中提取因果关系和行为修正。输出 JSON 数组，每个元素是以下之一：

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
- 仅输出 JSON 数组，不要 markdown"#;

/// Memory merge system prompt (from Python consolidator.py)
pub const MERGE_PROMPT: &str = "你是记忆合并器。输入两条语义高度相似的记忆。请输出一条去重后、信息不丢失、无冗余的合并文本。只输出最终文本，不要 JSON，不要 markdown。";

/// Contradiction detection prompt (from Python consolidator.py)
pub const CONTRADICTION_PROMPT: &str = r#"你是矛盾检测器。判断以下两条记忆是否存在事实矛盾。
如果矛盾，输出 JSON: {"contradicts": true, "reason": "简要说明"}
如果不矛盾，输出: {"contradicts": false}
只输出 JSON，不要其他内容。"#;

/// Rule distillation prompt (from Python distiller.py)
pub const DISTILLER_PROMPT: &str = "你是规则蒸馏器。输入是一组用户纠正 AI 的片段。请归纳可跨场景复用的通用规则，输出 JSON 数组。每个元素可以是字符串规则，或对象 {rule, rationale}。如果样本不足或无法提取出跨场景的通用性，必须返回 []，严禁强行总结具体事件。仅输出 JSON，不要 markdown。";
