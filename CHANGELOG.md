# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- **MCP/Workers**: Activated causal worker pipeline for asynchronous extraction of cause-and-effect relationships.
- **Core**: Refactored `memory_relations` support to allow robust linking of related memory fragments.

### Changed
- **MCP/Extractor**: Upgraded default fact extraction model to Qwen3.5-27B for significantly better causal relationships and structured fact parsing.
- **Docs**: Updated the recommended extraction model in `README.zh-CN.md` and `README.md` to Qwen3.5-27B.
- **Core (Fix)**: Stabilized vector KNN searches and kept auto-capture writable.
