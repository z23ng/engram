# AI Memory (`mem`) 目录结构与核心模块规划

本项目采用 **Rust Cargo Workspace** 实现高性能、单文件嵌入式核心引擎，并预留 Python/TypeScript SDK 绑定与可选 FUSE 挂载能力。

---

## 1. Cargo Workspace 目录树

```text
ai-memory/
├── Cargo.toml                  # Workspace 定义、统一依赖与编译配置
├── Cargo.lock
├── rust-toolchain.toml         # 锁定 Rust 工具链版本
├── AGENTS.md                   # AI 开发会话协作指南
├── README.md
├── docs/
│   ├── ARCHITECTURE.md
│   ├── DIRECTORY_STRUCTURE.md
│   └── ROADMAP.md
├── crates/
│   ├── mem-core/               # 领域模型层：VirtualPath、MemoryNode、Tier、Commit、Branch、Diff、Error
│   ├── mem-store/              # 存储底座：SQLite WAL 连接池、BLAKE3 CAS、zstd 压缩、Schema 迁移
│   ├── mem-vcs/                # 版本控制内核：Merkle Tree、Branch/Checkout、Commit、Diff、Rewind、3-Way Merge、GC
│   ├── mem-search/             # 检索与编译层：FTS5 BM25、Vector 索引、分支感知 RRF 重排、/core 热编译
│   ├── mem-consolidate/        # (可选冷路径) 后台异步记忆提炼、冲突消解与衰减归档接口
│   ├── mem-daemon/             # HTTP / gRPC 独立记忆服务端（供跨进程与远程 Agent 调用）
│   ├── mem-mcp/                # Model Context Protocol Server（暴露给 Claude Code / Cursor / Agents）
│   ├── mem-cli/                # 命令行工具：mem init / status / write / read / commit / branch / rewind / diff / merge / search
│   └── mem-fuse/               # (Phase 4 可选) 类 LayerFS 的 POSIX FUSE 虚拟文件系统挂载适配器
├── bindings/
│   ├── python/                 # PyO3 原生 Python 绑定与异步客户端
│   └── typescript/             # NAPI-RS 或 HTTP/MCP TypeScript SDK
└── tests/
    ├── integration/            # 端到端分支隔离、Rewind、三路合并、GC 正确性测试
    └── benchmarks/             # 万级记忆节点检索延迟、Zero-copy Branch 耗时与数据库体积基准
```

---

## 2. 各 Crate 核心职责与关键接口

### 2.1 `crates/mem-core`
- **职责**：纯内存领域模型、强类型路径校验与核心 Error 定义，不耦合 SQLite 或网络 I/O。
- **核心类型**：
  - `VirtualPath`：规范化虚拟路径（强制以 `/` 开头，拦截 `..` 路径穿越，解析所属 `MemoryTier`）。
  - `MemoryTier`：`Core | Episodic | Semantic | Procedural`。
  - `MemoryNode`：`path`、`content`、`metadata (tags, importance, ttl, custom)`。
  - `BlobId` / `TreeId` / `CommitId`（封装 `[u8; 32]` BLAKE3 哈希类型与 Hex 展示）。
  - `DiffEntry`：`Added | Modified | Deleted` 变更描述。

### 2.2 `crates/mem-store`
- **职责**：基于 `rusqlite`（bundled SQLite + FTS5）实现事务性底层存储。
- **核心模块**：
  - `schema.rs`：初始化与版本化迁移 `blobs`、`trees`、`commits`、`refs`、`workspace_entries`、`chunk_index`、`fts_chunks` 等表结构。
  - `cas.rs`：`put_blob`（BLAKE3 计算 + zstd 压缩 + 幂等去重插入）与 `get_blob`。
  - `branch_view.rs`：维护或高效解析指定 Branch 的活跃 `(path -> blob_id)` 映射（结合 Working Workspace 叠加层）。

### 2.3 `crates/mem-vcs`
- **职责**：实现完整的类 Git / LayerFS 树状版本控制内核。
- **核心接口**：

```rust
#[async_trait]
pub trait VersionControlEngine: Send + Sync {
    /// 初始化仓库并创建默认 main 分支
    fn init(&self) -> Result<CommitId>;

    /// 在指定分支的工作区写入或删除记忆节点
    fn write(&self, branch: &str, node: MemoryNode) -> Result<()>;
    fn delete(&self, branch: &str, path: &VirtualPath) -> Result<()>;

    /// 读取指定分支（Working Workspace 优先，其次 Commit Tree）或指定历史 Commit 中的节点
    fn read(&self, rev: &Revision, path: &VirtualPath) -> Result<Option<MemoryNode>>;

    /// 将工作区变更固化为不可变 Merkle Tree 与 Commit
    fn commit(&self, branch: &str, author: &str, message: &str) -> Result<CommitId>;

    /// 零拷贝创建新分支
    fn create_branch(&self, new_branch: &str, from_rev: &Revision) -> Result<()>;

    /// 将分支指针回滚到目标 Commit，并清空该分支脏工作区
    fn rewind(&self, branch: &str, target: &CommitId) -> Result<()>;

    /// 计算两个修订版本（分支或 Commit）之间的结构化差异
    fn diff(&self, base: &Revision, target: &Revision) -> Result<Vec<DiffEntry>>;

    /// 三路合并：将 source 分支合并进 target 分支
    fn merge(&self, source_branch: &str, target_branch: &str, strategy: MergeStrategy) -> Result<MergeOutcome>;

    /// 标记-清除垃圾回收：清理已删除或被 Rewind 孤立且无引用可达的 blobs/trees
    fn gc(&self) -> Result<GcStats>;
}
```

### 2.4 `crates/mem-search`
- **职责**：提供 Obsidian Wiki 图谱解析、分支隔离的 BM25 + Vector + 1-Hop 双链混合检索、`/core` 热路径编译与本地 Vault 导入能力。
- **核心模块**：
  - `config.rs`：`MemConfig`、`ProviderBackend`（`Disabled | Ollama | OpenAi`）、`EmbeddingProviderConfig` 与 `ConsolidateLlmConfig`，支持开发态本地 Ollama 与生产态 OpenAI API 一键切换。
  - `wiki.rs`：Obsidian Markdown 解析器（YAML Frontmatter `title`/`aliases`/`tags`/`entities`、`[[Wikilinks]]` 提取、标题层级面包屑语义切片）。
  - `embedder.rs`：`EmbeddingProvider` Trait、Level-1 `NoopEmbedder`、离线 `DeterministicHashEmbedder` 与支持 Ollama/OpenAI 的 `OpenAiCompatibleEmbedder`。
  - `hybrid.rs`：`MemSearchEngine`，维护挂载于不可变 `blob_id` 的 `chunk_index` / `fts_chunks` / `blob_links`，执行分支隔离的 BM25 + Vector + 1-Hop `[[Wikilink]]`（Outlinks/Backlinks/Entity）扩散与 Graph-RRF 重排。
  - `compiler.rs`：`compile_core` 读取 `/core/` 目录节点，按 `(importance DESC, path ASC)` 与 Token 预算在 `< 5ms` 内确定性编译上下文。
  - `vault.rs`：`import_vault` 递归导入本地 Obsidian Markdown 知识库（自动跳过 `.obsidian/`）并构建版本化图谱索引。

### 2.5 `crates/mem-mcp`
- **职责**：作为 MCP Server（`stdio` / Streamable HTTP）直接嵌入任意 AI Coding Agent 或个人助手。
- **暴露的核心 MCP Tools**：
  - `mem_read(path, branch?)` / `mem_list(prefix, branch?)`
  - `mem_write(path, content, tags?, importance?, branch?)`
  - `mem_search(query, prefix?, branch?, top_k?)`
  - `mem_compile_core(branch?, max_tokens?)`
  - `mem_commit(message, branch?)`
  - `mem_branch(name, from?)`
  - `mem_rewind(commit_id, branch?)`
  - `mem_diff(base, target)`
  - `mem_merge(source, target)`

### 2.6 `crates/mem-cli`
- **职责**：提供类 Git 体验的命令行交互工具 `mem`。
- **典型工作流示例**：
  ```bash
  mem init ~/.ai-memory/project.db
  mem write /core/rules.md "Never edit prod database directly"
  mem commit -m "init project core rules"
  mem branch subagent/refactor
  mem write --branch subagent/refactor /semantic/findings.md "..."
  mem search --branch subagent/refactor "database"
  mem diff main subagent/refactor
  mem merge subagent/refactor main
  mem serve --mcp --db ~/.ai-memory/project.db
  ```

### 2.7 `crates/mem-fuse`（Phase 4 可选模块）
- **职责**：借鉴 LayerFS 的 FUSE 挂载能力，允许将某个 Memory Branch 直接挂载为本地只读或可写目录（如 `/mnt/memory/main/`），让不熟悉 MCP 的工具或脚本直接通过 `cat`/`grep`/`echo` 读写版本化记忆。

---

## 3. 核心数据结构草案

```rust
/// 记忆节点虚拟路径与层级
pub enum MemoryTier {
    Core,
    Episodic,
    Semantic,
    Procedural,
}

/// 记忆元数据（随内容一同持久化，参与 diff）
pub struct MemoryMetadata {
    pub content_type: String,           // e.g. "text/markdown", "application/json"
    pub importance: f32,                // 0.0 ~ 1.0，影响核心编译优先级与检索加权
    pub tags: Vec<String>,
    pub updated_at_ms: i64,
    pub ttl_seconds: Option<u64>,       // 可选记忆过期时间
}

/// 修订版本定位符
pub enum Revision {
    BranchWorkspace(String),            // 分支当前视图（已提交树 + 未提交工作区）
    BranchHead(String),                 // 仅分支最新已提交 Commit
    Commit(CommitId),                   // 精确历史 Commit 哈希
}

/// 三路合并结果
pub enum MergeOutcome {
    FastForward(CommitId),
    Merged(CommitId),
    Conflict {
        base_commit: CommitId,
        conflicts: Vec<MergeConflict>,
    },
}
```
