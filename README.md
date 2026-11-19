# MemVaultTree — AI Memory (`mem`)

受 `Ephemeral-AI-Lab/layerfs` 启发构建的**面向 AI Agent 的版本化结构记忆引擎（Versioned Structured Memory Engine）**——将内容寻址存储（CAS）、Merkle Tree 版本控制与混合检索引擎结合进单文件嵌入式数据库，为 Agent 提供具备“时间旅行”与“分支隔离”能力的大脑。

---

## 为什么需要新的 AI Memory 引擎？

市面上的主流 Agent Memory 方案（如 Mem0、Zep、传统扁平 Vector RAG）在工程与复杂任务场景存在四大痛点：
1. **黑盒与高写入延迟**：每次对话都要同步调用 LLM 提取 Fact，延迟动辄数百毫秒到数秒，且提取过程不可预测。
2. **扁平化与结构缺失**：把所有记忆揉进同一个向量库，精准配置、项目规则与历史经验混杂，缺乏清晰的目录层级与置信度管理。
3. **记忆污染不可逆（Poisoning）**：当 Agent 在长任务中产生幻觉并写入了错误结论后，传统记忆库无法一键回滚到“幻觉发生之前”的状态。
4. **多 Agent 并行冲突**：主 Agent 派生多个 Subagent 并行探索不同方案时，所有子任务共享同一份可写记忆库，极易互相覆盖和污染。

**AI Memory (`mem`)** 将 `LayerFS` 的文件系统版本控制范式升维到 **Agent 认知与记忆管理**，提供彻底的白盒可控方案。

---

## 核心特性

### 1. LayerFS 式的 CAS + Merkle Tree 存储底座
- **单文件零外部依赖**：基于嵌入式 SQLite（WAL 模式）保存所有记忆节点，每一份记忆内容与元信息均通过 **BLAKE3 内容寻址（CAS）** 与 zstd 压缩去重存储。
- **数据不可变性（Immutability）**：所有历史 Blob、Tree 与 Commit 一经写入永不原地篡改，彻底消除意外覆盖风险。

### 2. Agent 大脑的 Git 语义（`commit` / `branch` / `rewind` / `diff` / `merge`）
- **零拷贝分支（Zero-Copy Branching）**：派生 Subagent 或开启高风险探索任务前，亚毫秒级创建独立记忆分支（`subagent/explore-1`），与主干完全隔离。
- **瞬间回滚（Rewind）**：推理走入死胡同或发现记忆污染时，一条命令即可将指定分支指针安全回滚至上一个健康 Checkpoint。
- **结构化 Diff 与三路合并（Three-Way Merge）**：支持对比任意两个 Commit 间的记忆增删改，子任务结束后通过结构化与语义冲突检测将有效经验合并回 `main`。

### 3. 虚拟记忆分层命名空间（VFS Namespace）
摒弃扁平向量汤，所有记忆以虚拟文件系统路径组织：
- `/core/`：高密度、强置顶的核心记忆（系统约束、用户偏好、当前任务目标）。
- `/episodic/`：按时间线与会话归档的轨迹日志（对话记录、关键工具调用事件）。
- `/semantic/`：结构化知识卡片（项目架构、实体关系、长期事实沉淀）。
- `/procedural/`：技能与经验记忆（Playbook、SOP、报错避坑指南）。

### 4. 分支感知 Obsidian 双链图谱混合检索 + Ollama/OpenAI 双态切换
- **热路径（Hot Path < 5ms）**：无需调用 LLM，直接将当前分支 `/core/` 编译为紧凑的上下文 Block 注入 System Prompt。
- **Obsidian Wiki-Graph + Mem0 融合检索（`mem-search`）**：融合 **多列加权 FTS5（含 `aliases`/`tags` 与 CJK 支持）**、**向量余弦检索** 与 **1-Hop `[[Wikilink]]` 出链/反向链接/实体图谱扩散** 并经 Graph-RRF 重排，严格限定在当前 Branch 快照视图内，绝不泄露未合并分支或已回滚记忆。
- **本地 Ollama（开发）与 OpenAI（生产）无缝切换**：`MemConfig::ollama_dev()` 默认对接本地 Ollama（`bge-m3` + `qwen2.5:7b`），上生产后通过 `MemConfig::openai_prod()` 或 `MEM_PROVIDER=openai` 一键切换至 OpenAI（`text-embedding-3-small` + `gpt-4o-mini`），亦支持无模型纯 BM25 + 双链图谱模式运行。
- **冷路径整理（Cold Consolidation，可选异步）**：任务空闲时在后台隔离分支异步将原始 `/episodic/` 日志提炼为带 `[[双链]]` 的 `/semantic/` 事实卡片与 `/procedural/` SOP。

---

## 快速开始（一键部署）

### 一键部署向导
```bash
# 交互式向导（支持本地 Ollama / 生产 OpenAI / 极简 Lite 三种模式）
./scripts/deploy.sh

# 或直接运行本地演示（自动写入 Obsidian Wiki 示例并执行混合检索与 <5ms 编译）
./scripts/deploy.sh --mode lite --seed-demo
# 或使用 Makefile
make demo
```

### 从开发态（Ollama）切换至生产态（OpenAI）
```bash
# 1. 本地开发（默认 Ollama bge-m3 + qwen2.5:7b）
./scripts/deploy.sh --mode ollama

# 2. 上线生产（仅切换环境变量或重新生成 mem.env，无需迁移 .db 文件）
export MEM_PROVIDER=openai
export OPENAI_API_KEY="sk-..."
mem doctor
```

详细配置与 Docker / MCP 客户端接入说明见 [一键部署与运维指南 (docs/DEPLOYMENT.md)](./docs/DEPLOYMENT.md)。

---

## 文档导航

| 文档 | 说明 |
| :--- | :--- |
| [一键部署与运维指南 (docs/DEPLOYMENT.md)](./docs/DEPLOYMENT.md) | 交互式/命令行一键部署、Ollama→OpenAI 切换、MCP 接入与 Docker 部署 |
| [系统架构设计 (docs/ARCHITECTURE.md)](./docs/ARCHITECTURE.md) | 总体分层架构、VFS 命名空间、SQLite CAS/Merkle Schema、核心时序图与合并语义 |
| [目录结构与模块规划 (docs/DIRECTORY_STRUCTURE.md)](./docs/DIRECTORY_STRUCTURE.md) | Rust Cargo Workspace 规划、SDK 绑定规划与各 Crate 核心 Trait/接口签名 |
| [开发路线图与里程碑 (docs/ROADMAP.md)](./docs/ROADMAP.md) | M1 至 M4 阶段拆分、核心任务与可量化验收标准 |
| [Agent 开发协作指南 (AGENTS.md)](./AGENTS.md) | 供新开 AI 开发 Session 快速接手的架构不变量、技术栈约束与起步指引 |
