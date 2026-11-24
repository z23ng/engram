# Engram (`mem`) 开发路线图与里程碑

本项目采用**“先夯实不可变 CAS 与版本控制内核，再构建分支隔离混合检索，最后交付 MCP/CLI 工具链与 FUSE/异步整理”**的演进路线。

---

## Milestone 1: CAS 存储与版本控制内核（CAS & Versioned Tree Kernel）

**阶段目标**：完成类似 LayerFS 核心的嵌入式 SQLite 内容寻址存储与 Merkle Tree 版本控制引擎，跑通零拷贝分支、回滚与 Diff。

### 核心任务
- [x] **1.1 初始化 Rust Workspace 与 `mem-core`**
  - 建立 Workspace 结构，定义 `VirtualPath`（严格路径规范化与 `MemoryTier` 解析）、`BlobId`、`TreeId`、`CommitId`、`MemoryNode`。
- [x] **1.2 `mem-store` SQLite CAS 底座**
  - 配置 `rusqlite`（bundled SQLite + WAL 模式），建表 `blobs`、`tree_objects`、`trees`、`commits`、`refs`、`workspace_entries`。
  - 实现基于 **BLAKE3 + zstd** 的内容寻址写入去重与读取。
- [x] **1.3 `mem-vcs` 核心版本操作**
  - 实现按分支隔离的 Working Workspace 读写（`write`、`delete`、`read`、`list`）。
  - 实现 Merkle Tree 自底向上构造与 `commit` 不可变快照提交。
  - 实现 `create_branch`（零拷贝分支）、`rewind`（原子回滚分支指针并丢弃脏暂存区）、`log`、`diff`（利用 Merkle Tree 子树哈希快速剪枝比对）与基础三路合并。
- [x] **1.4 不可达对象垃圾回收（Mark & Sweep GC）**
  - 实现从所有 `refs` 与暂存区出发遍历可达 `commits -> trees -> blobs`，安全清理被 `rewind` 或删除分支遗弃的孤儿数据（含 `fts_chunks` 清理）。

### 验收标准（Exit Criteria）
1. 相同内容重复写入不同路径或分支时，`blobs` 表仅存一份物理记录。
2. 在已有 10,000 个记忆节点的库中创建新分支耗时 **< 1ms**，空间增量 **0 字节**。
3. 执行错误写入与提交后，调用 `rewind` 可 100% 精准还原到上一 Commit 的目录树状态，单元测试覆盖全部边界场景（目录覆盖文件、嵌套删除、连续回滚）。

---

## Milestone 2: 分支感知混合检索与热路径编译（Branch-Aware Search & Hot Compiler）

**阶段目标**：在不依赖外部向量库的前提下，构建严格受 Branch 视图隔离的 BM25 + Vector 混合检索引擎及 `<5ms` `/core` 上下文编译器。

### 核心任务
- [x] **2.1 内容切分与 Obsidian Wiki 图谱表挂载到 `blob_id`**
  - 在 `mem-store` 增加 `chunk_index`、SQLite `fts5` 虚表（`fts_chunks`）及双链/实体关系图谱表（`blob_links`），所有文本切片、向量与图谱边严格挂载于不可变 `blob_id`。
  - 实现 Obsidian Wiki 解析器（YAML Frontmatter `aliases`/`tags`/`entities`、标题层级面包屑切片、`[[Wikilinks]]` 提取）及本地 Obsidian Vault 导入器 `import_vault`。
  - 实现可插拔 `EmbeddingProvider` Trait、Level-1 无模型降级（`NoopEmbedder`）与统一 OpenAI 兼容客户端（开发态默认本地 Ollama `bge-m3`，生产态切换 OpenAI `text-embedding-3-small`）。
- [x] **2.2 分支隔离过滤、双链 1-Hop 图谱扩散与 Graph-RRF 混合重排**
  - 实现高效的当前分支活跃 `blob_id` 视图解析（含 Working Workspace 增量覆盖与分支内 Wiki 目标符号表）。
  - 实现多列加权 FTS5 BM25（含 CJK 双字索引）+ 向量余弦相似度检索 + Obsidian 1-Hop `[[Wikilink]]`（Outlinks/Backlinks/Entity）图谱扩散，通过 Graph-RRF 结合 `importance` 重排输出。
- [x] **2.3 `/core` 热路径编译器（Hot Compiler）**
  - 实现 `compile_core(rev, max_tokens)`，按 `(importance DESC, path ASC)` 确定性拼装可注入 System Prompt 的 `<core_memory>` 上下文 Block。

### 验收标准（Exit Criteria）
1. **分支强隔离验证**：在分支 B 写入特定关键词并提交后，在分支 A 执行 `mem_search` 绝对无法搜出分支 B 的任何内容。
2. 相同文件跨 50 个分支共享时，Embedding 计算与文本索引只触发 1 次。
3. 10,000 节点规模下，`/core` 编译耗时 **< 5ms**，纯本地混合检索耗时 **< 30ms**。

---

## Milestone 3: MCP Server、CLI 工具链与三路合并（MCP, CLI & 3-Way Merge）

**阶段目标**：提供立即可用的 `mem` CLI 与 `mem-mcp` Server，支持多 Subagent 分支合并回主干，并接入真实 Coding Agent。

### 核心任务
- [x] **3.1 三路合并引擎（Three-Way Merge）**
  - 实现 Commit DAG 最近公共祖先（LCA）查找算法。
  - 实现三棵树（`base` / `ours` / `theirs`）自动合并、`/episodic/` 追加合并与同路径冲突结构化报告。
- [x] **3.2 `mem-cli` 完整命令行工具**
  - 交付完整类 Git 体验的 CLI（`onboard`, `config`, `init`, `status`, `write`, `read`, `ls`, `commit`, `log`, `branch`, `rewind`, `diff`, `merge`, `search`, `compile`, `ingest`, `vault`, `gc`, `doctor`, `completions`）。
- [x] **3.3 内置 stdio MCP Server（12 个工具 + `mem mcp install`）**
  - 实现标准 JSON-RPC 2.0 stdio MCP Server（暴露 `mem_read`、`mem_write`、`mem_ingest`、`mem_list`、`mem_search`、`mem_compile_core`、`mem_commit`、`mem_branch`、`mem_rewind`、`mem_diff`、`mem_merge`、`mem_consolidate`），并支持一键无损写入 Cursor / Claude Code / Claude Desktop / Windsurf。
- [x] **3.4 `mem-daemon`（HTTP REST / SSE / HTTP+SSE MCP 多进程守护模式）**
  - 提供独立 HTTP REST / SSE / Streamable MCP 守护进程（`mem daemon` / `mem-daemon`），支持 Bearer Token 鉴权、实时分支过滤事件流 `/v1/events` 与定时后台整理。

### 验收标准（Exit Criteria）
1. 能够模拟两个并发 Subagent 在各自独立分支修改不同记忆路径后，无损三路合并回 `main`；修改同一路径时准确检出冲突并保留双侧版本供决策。
2. Claude Code / Cursor 等 MCP 客户端配置 `mem-mcp` 后，可自主通过工具在会话间保存经验、开分支试错并在出错时回滚记忆。

---

## Milestone 4: SDK 生态、FUSE 挂载与后台整理（SDKs, FUSE & Consolidation）

**阶段目标**：打通主流 AI 开发语言 SDK、可选的 LayerFS 风格 POSIX 目录挂载以及冷路径自动记忆整理能力。

### 核心任务
- [x] **4.1 Python / TypeScript 原生 SDK**
  - 基于 PyO3（Python 原生扩展 `engram.open` + `EngramClient` HTTP/SSE 客户端）与零依赖 TypeScript SDK（`@engram-memory/sdk`，提供 `EngramClient` 与本地 `LocalEngram`），方便 LangGraph / CrewAI / 自研 Agent 框架集成。
- [x] **4.2 `mem-fuse`（POSIX FUSE 挂载视图）**
  - 基于 `fuser` 将指定 Memory Branch 或只读历史 Commit 挂载为本地 POSIX 目录（`mem mount` / `mem-fuse`），支持标准 `ls` / `cat` / `grep` / `echo` / `mv` / `mkdir` 检查和编辑记忆节点，写入后自动映射回 Working Workspace 并即时建索引。
- [x] **4.3 `mem-consolidate`（后台“睡眠整理”工作流）**
  - 实现冷路径整理引擎（`mem consolidate` / MCP `mem_consolidate`）：在独立 `consolidate/*` 分支扫描 `/episodic/` 轨迹日志，去重压缩并提炼 `/semantic/` 事实卡片与 `/procedural/` SOP，自动编织 `[[Wikilinks]]`，支持 `diff` 审查、归档衰减与自动三路合并回 `main`。

### 验收标准（Exit Criteria）
1. Python Agent 脚本可通过 `import engram` 在 5 行代码内完成本地记忆库初始化、分支创建与上下文检索。
2. 后台整理任务全程运行在隔离分支，即使整理过程中发生异常或结果不理想，也不会污染主干 `main` 记忆，可随时审查 `diff` 后合并或丢弃。
