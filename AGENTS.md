# AGENTS.md — AI Memory (`mem`) 开发协作指南

本目录是 **AI Memory (`mem`)** 独立项目的开发工作区。请在后续所有开发工作中严格遵循本文档的架构边界、技术选型与工程不变量。

---

## 1. 项目定位与边界

- **一句话定位**：受 `Ephemeral-AI-Lab/layerfs` 启发构建的**基于单文件 SQLite CAS + Merkle Tree 的版本化结构记忆引擎**，让 AI Agent 的记忆支持 Git 式的 `commit`、`branch`、`rewind`、`diff` 与 `merge`。
- **独立性原则**：本项目专注于**版本化记忆存储、结构化 VFS 命名空间、分支感知混合检索与 MCP/SDK 暴露**。**不要**在此仓库中耦合任何容器运行时（containerd / Docker / MicroVM）或代码执行沙箱逻辑。

---

## 2. 强制技术栈选型

| 层级 | 选型 | 说明 |
| :--- | :--- | :--- |
| **核心语言** | Rust (Edition 2021/2024) | 统一采用 Cargo Workspace 管理 |
| **持久化存储** | `rusqlite` (`bundled`, `fts5`) | 单文件嵌入式 SQLite（开启 WAL 模式），零外部数据库部署依赖 |
| **内容寻址哈希** | `blake3` | 高速强校验哈希，用于计算 Blob / Tree / Commit ID |
| **压缩算法** | `zstd` | 对存储在 `blobs` 表中的记忆载荷进行透明压缩 |
| **向量检索** | `sqlite-vec` 或轻量纯 Rust 余弦检索 | 与 `blob_id` 强绑定，支持纯 BM25 降级模式 |
| **异步与服务端** | `tokio` + `axum` + MCP Rust SDK | 驱动 `mem-daemon` 与 `mem-mcp` |
| **CLI 与序列化** | `clap` + `serde` + `serde_json` | CLI 交互与元数据/YAML Frontmatter 解析 |

---

## 3. 核心架构不变量（Invariants）

1. **CAS 对象绝对不可变**：写入 `blobs`、`trees`、`commits` 表的任何记录一旦落盘严禁 `UPDATE` 修改内容；所有“修改”和“回滚”本质上是创建新对象并移动 `refs` 指针或更新 Branch 的 Working Workspace。
2. **严禁跨分支记忆泄漏**：所有检索（`mem_search`）、读取（`mem_read`）、编译（`compile_core`）操作必须显式绑定目标 `Revision`（默认为指定分支视图），绝对不允许搜出未合并分支或已被 `rewind` 遗弃历史节点的内容。
3. **索引挂载在 `blob_id` 而非 `path`**：FTS5 文本切片与 Embedding 向量必须关联不可变的 `blob_id`，确保零拷贝分支时**零重复 Embedding 计算与零额外索引存储**。
4. **热路径零 LLM 调用**：`mem_read`、`mem_write`、`mem_search`、`compile_core` 必须保持纯本地确定性低延迟（毫秒级）；任何基于 LLM 的记忆摘要或归并只能放在可选的异步 `mem-consolidate` 冷路径分支中执行。
5. **零外部强依赖开箱即用**：核心引擎仅凭一个 `.db` SQLite 文件即可在本地运行，不强制用户先起 Postgres、Redis 或 Qdrant。

---

## 4. 新会话起步指令

当你在本目录下开启开发时，请按以下顺序推进（参考 [docs/ROADMAP.md](./docs/ROADMAP.md)）：

1. 先阅读 [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) 与 [docs/DIRECTORY_STRUCTURE.md](./docs/DIRECTORY_STRUCTURE.md) 确认数据库 Schema、VFS 路径规范与 Trait 接口。
2. 检查本地 Rust 工具链与 C 编译环境（`rusqlite` bundled 模式编译 SQLite 需要基本 C 编译器 `cc`/`gcc`）。
3. 从 **Milestone 1** 开始：
   - 初始化根目录 `Cargo.toml` Workspace。
   - 优先实现 `crates/mem-core`（强类型 `VirtualPath`、`BlobId`/`TreeId`/`CommitId`、`MemoryNode`）。
   - 接着实现 `crates/mem-store`（SQLite WAL 初始化、BLAKE3 CAS 读写）与 `crates/mem-vcs`（Working Workspace、`commit`、Merkle Tree 构建、零拷贝 `branch`、`diff` 与 `rewind`）。
   - 编写完整的并发与回滚边界单元测试，确保核心版本控制内核无懈可击后再推进 Milestone 2 的混合检索。
