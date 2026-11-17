# AI Memory (`mem`)

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

### 4. 分支感知的混合检索 + 快慢双路径
- **热路径（Hot Path < 5ms）**：无需调用 LLM，直接将当前分支 `/core/` 编译为紧凑的上下文 Block 注入 System Prompt。
- **分支感知混合检索（Branch-Aware Hybrid Search）**：融合 **SQLite FTS5（BM25 关键词检索）** 与 **向量相似度检索（Vector Search）** 并经 RRF 重排，且严格限定在当前 Branch 的 Merkle Tree 快照视图内，绝不泄露其他未合并分支的脏记忆。
- **冷路径整理（Cold Consolidation，可选异步）**：任务空闲时在后台分支异步将原始 `/episodic/` 日志去重提炼为 `/semantic/` 事实与 `/procedural/` 经验并原子提交。

---

## 文档导航

| 文档 | 说明 |
| :--- | :--- |
| [系统架构设计 (docs/ARCHITECTURE.md)](./docs/ARCHITECTURE.md) | 总体分层架构、VFS 命名空间、SQLite CAS/Merkle Schema、核心时序图与合并语义 |
| [目录结构与模块规划 (docs/DIRECTORY_STRUCTURE.md)](./docs/DIRECTORY_STRUCTURE.md) | Rust Cargo Workspace 规划、SDK 绑定规划与各 Crate 核心 Trait/接口签名 |
| [开发路线图与里程碑 (docs/ROADMAP.md)](./docs/ROADMAP.md) | M1 至 M4 阶段拆分、核心任务与可量化验收标准 |
| [Agent 开发协作指南 (AGENTS.md)](./AGENTS.md) | 供新开 AI 开发 Session 快速接手的架构不变量、技术栈约束与起步指引 |
