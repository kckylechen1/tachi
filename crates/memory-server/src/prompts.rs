// prompts.rs — LLM prompt templates for memory server

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
