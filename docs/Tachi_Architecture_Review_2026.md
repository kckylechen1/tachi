# Tachi 架构重构与瘦身白皮书
**(Based on Codex 5.4 & Kimi K2.5 Architecture Review)**

## 1. 核心定调
**Tachi 不应该是“试图理解你人生的智能大脑”，而应该是一个“脏活累活分清楚、出错可追、重建不疼的 Reliable Memory Substrate”。**

- **放弃幻想**：砍掉旧版脆弱的 `提取 -> 合并 -> 蒸馏 -> 回写` 异步流水线。这套机制在拿 LLM 当数据库状态机用，极易造成语义漂移、状态污染和链式崩溃。
- **保留优势**：坚守 Rust/SQLite 的强类型、事务性和高性能本地检索优势。

## 2. 读写分离与数据库分层
不要把“事件、状态、记忆”一锅炖，数据库必须严格拆分为 4+1 层：

1. **`events`（原始事实流）**
   - **规则**：Append-Only。所有对话、工具返回的原话直接存入。不做 LLM 处理。这是系统的绝对真相，索引坏了全靠它重建。
2. **`hard_state`（硬状态层 / 真相层）**
   - **规则**：Key-Value 强硬覆盖（Upsert）。存储交易仓位、观察池、偏好开关等。**绝不允许走向量召回，拒绝大模型模糊推断。**
3. **`memory_items`（软记忆层）**
   - **规则**：存储短平快的“盘感、历史评价”。不准大模型原地改写融合历史。如果信息过时，打上 `is_stale=true` 并追加新条目。
4. **`indexes`（纯索引层）**
   - **规则**：向量（Vector）和全文（FTS5）索引。属于可随时丢弃重建的“目录”，异步 Job 只准碰这层，不准碰事实层。
5. **`derived_items`（派生层/因果层）**
   - **规则**：存储从历史中提取的 rules、causal、summaries。**只能派生，绝不能反向污染/覆盖事实层。**

## 3. 极简的“最小路由器”写入机制
用极简的规则路由替代复杂的提取器。
- 这是一条**硬状态**变更？ -> 同步覆写 `hard_state`
- 这是一条**软记忆**？ -> 追加到 `memory_items`
- 都不是？ -> 只进 `events`。
- **计算向量/建索引** -> 扔到后台异步跑，失败了也无所谓（只影响召回，不丢数据）。

## 4. 拒绝 Fork：领域模型（Domain Pack）化
**针对 Trading/Stock 场景的结论：绝不要 Fork Tachi 内核！**

- `watchlist` / `fund_position` 本质是领域语义（Schema），不是内核机制。
- **Core 负责**：怎么存、怎么搜、权限、版本（Domain-agnostic）。
- **Domain Pack 负责**：Trading Schema（持仓、订单）、Adapter（行情接入）、特化视图。
- **何时独立系统？**：只有当涉及到资金正确性、订单执行、严格风控、强一致性账本时，才应该把 Execution/Ledger 独立成外部系统，脱离 Memory Core。

## 5. 跨模态扩展预留（Multimodal）
为未来的医疗、视觉 Agent 做准备，抽象化 `MemoryUnit`：
- 不只是 Text，支持 Image / PDF / Audio 等作为 `artifacts` / `blob_refs`。
- Core 保持抽象，Modal Handler 交给 Domain Pack 去解析。
