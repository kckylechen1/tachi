# Tachi V2: 跨模态、跨领域可扩展记忆底座 (Memory Substrate)
**(Based on Architecture Review - 2026-03-14)**

## 核心定位
**你不是在做一个 Trading Memory，而是在做一个 `agent-native`、`domain-extensible`、`多模态` 的 memory operating substrate。**
未来的形态是：**通用内核不变，领域特性以 Pack (插件) 的形式往上插。**
- Trading Agent 挂 `trading-pack`
- Medical Agent 挂 `medical-pack`
- Browser Agent 挂 `web-pack`

---

## 一、Tachi Core 负责的 4 个基础层

Core 不懂业务，它只管抽象的数据流转。

### 1. 事件层 (Event Layer)
支持各种模态的原生输入，做 Append-Only 归档。
- text / image / audio / pdf / structured json / tool result

### 2. 对象层 (Object Layer)
- `event`：原始输入
- `hard_state`：可覆盖的明确状态
- `memory_item`：语义化的历史记忆
- `derived_item`：大模型推导出的因果/规则
- `artifact` / `blob_ref`：非结构化文件的引用

### 3. 索引层 (Index Layer)
- FTS（全文检索）
- Vector Embedding（文本 / 图像 / 多模态）
- Metadata & Time filters

### 4. 组装层 (Assembly Layer)
把硬状态、软记忆、领域上下文按照既定策略拼接，喂给上层 Agent。

---

## 二、数据结构的终极抽象：`MemoryUnit`

为了支持跨模态（如医疗影像）和跨领域，底层实体必须抽象为统一的 `MemoryUnit`：

```typescript
MemoryUnit {
  id: string
  kind: enum           // event | state | memory | derived | artifact
  modality: enum       // text | image | audio | pdf | json | mixed
  scope: string        // e.g., "user:kyle", "project:Tachi"
  source: string       // 来源追踪
  content_ref: string  // Text body 或指向 S3/本地文件系统的 Blob Pointer
  summary?: string     // 可选摘要
  metadata: json       // 任意结构化字段 (供 Domain Pack 使用)
  embedding_ref?: str  // 可选的向量索引关联
  created_at: int
  updated_at: int
  superseded_by?: str  // 被哪条新记忆覆盖 (软记忆用)
}
```

**以医疗场景 (`medical-pack`) 为例，这套结构可以完美兼容：**
- 一张 MRI 图像：`kind = artifact`, `modality = image`
- “左肺阴影增大”的解释：`kind = derived`, `modality = text`
- “患者青霉素过敏”：`kind = state`, `modality = json` (覆盖写入)
- “患者多次表达术前焦虑”：`kind = memory`, `modality = text`

---

## 三、Domain Pack (领域包) 的契约边界

Domain Pack 不去改动 Core，而是通过注册机制挂载领域知识。一个合格的 Pack 必须提供以下 5 个扩展点：

### 1. Schema Registry
注册该领域的特有实体类型。
- *Medical Pack*：`patient_profile`, `allergy_state`, `imaging_artifact`
- *Trading Pack*：`portfolio_state`, `watchlist_item`, `order_intent`

### 2. Adapter Registry (摄入转换器)
将外部系统的脏数据转换为标准 `MemoryUnit`。
- *Medical Pack*：EHR Adapter (读病历系统)
- *Trading Pack*：Market Feed Adapter (读行情 API)

### 3. Modality Handlers
告诉 Core 遇到特殊文件怎么处理。
- 图片怎么入库？PDF 怎么切片？哪些要算多模态向量？哪些只保留本地路径引用？

### 4. Retrieval Policy (召回策略)
定义该领域的上下文拼接逻辑。
- *Medical 策略*：先拉 Patient Hard State -> 再拉近期影像摘要 -> 补 1~2 条 Derived Caution 规则。

### 5. Validation / Safety Hooks (安全守门员)
- *Medical / Financial 强制规则*：不允许仅靠 Soft Memory 回答致命问题；必须优先读取并标注 Verified Hard State；派生结论不能当做最终诊断/交易指令。

---

## 四、落地建议：双轨对照开发

为了验证 Core 足够通用，**绝对不能只拿着 Trading 场景做开发**。
建议同时选取两个差异极大的场景作为对照组来验证架构：

1. `trading-pack`（偏高频状态、强数字、低容错）
2. `medical-pack` 或 `clinical-pack`（偏多模态、大文本、生命安全底线）

只要这两个 Pack 都能用同一套 `MemoryUnit` 跑通读写和检索，Tachi Core V2 的地基就算彻底打成了。
