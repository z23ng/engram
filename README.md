# MemVaultTree — AI Memory (`mem`)

> **Git-like Versioned Structured Memory Engine for AI Agents**
> 单文件 SQLite CAS + Merkle Tree 版本控制 + Obsidian `[[双链]]` Wiki 图谱混合检索 + 原生 MCP Server（纯 Rust 零外部服务依赖）

[![CI](https://github.com/z23ng/memvaulttree/actions/workflows/ci.yml/badge.svg)](https://github.com/z23ng/memvaulttree/actions/workflows/ci.yml)
[![Release](https://github.com/z23ng/memvaulttree/actions/workflows/release.yml/badge.svg)](https://github.com/z23ng/memvaulttree/actions/workflows/release.yml)

受 `Ephemeral-AI-Lab/layerfs` 启发构建的**面向 AI Agent 的版本化结构记忆引擎**——将内容寻址存储（BLAKE3 CAS + zstd）、Merkle Tree 分支版本控制与 Obsidian 风格双链图谱融合检索结合进单个 SQLite 文件，让 Agent 的大脑真正具备 **“零拷贝分支试错”**、**“防幻觉瞬间回滚（Rewind）”** 与 **“高命中率结构化检索”** 能力。

---

## 一行命令安装 & 交互式引导（30 秒上手）

```bash
curl -fsSL https://raw.githubusercontent.com/z23ng/memvaulttree/main/install.sh | sh
```

执行后将自动完成二进制安装并启动 **`mem onboard`** 现代引导向导：
- **自动硬件与环境探测**：识别 CPU/内存、NVIDIA GPU 显存、本地 Ollama 运行状态与已安装的 AI 客户端；
- **零环境配置持久化**：自动写入 `~/.ai-memory/config.json`，从此无需手动 `source env`；
- **零接触 MCP 自动挂载**：自动将 `ai-memory` 合并写入 **Cursor / Claude Code / Claude Desktop / Windsurf** 配置；
- **开发 / 生产一行命令热切换**：本地用 Ollama，上生产后执行 `mem config use openai --api-key sk-...` 即可无缝切换。

```text
┌  MemVaultTree (`mem`) — Interactive Setup
│  Git-like Versioned Memory + Obsidian Wiki Graph + MCP
│
◇  Host & Hardware Discovery
│  ├─ System   : linux / x86_64 (16 threads, 15.0 GiB RAM)
│  ├─ GPU      : NVIDIA GeForce RTX 4080 SUPER, 16376 MiB
│  ├─ Ollama   : online at 127.0.0.1:11434
│  └─ Agents   : Cursor, Claude Code
│
◇  ollama Profile saved: ~/.ai-memory/config.json -> bge-m3 + qwen2.5:7b
│
◇  Memory Vault Initialized ~/.ai-memory/memory.db
│  ├─ Root     : main (daf73130fd0d)
│  └─ Starter  : seeded 4 Obsidian Wiki notes & committed snapshot
│
◇  MCP Agent Auto-Wiring
│  ├─ Cursor                 merged (~/.cursor/mcp.json)
│  ├─ Claude Code            merged (~/.claude.json)
│  └─ Universal MCP Snippet  created (~/.ai-memory/mcp_config.json)
│
◇  Live Verification Passed
│  ├─ /core Compiler : 0.18ms (44 tokens, target < 5.00ms)
│  └─ 1-Hop Search   : "BUSY_TIMEOUT" -> 3 linked notes in 0.75ms
│
└  MemVaultTree is ready! No env sourcing needed — config is auto-loaded.
```

---

## 为什么选择 MemVaultTree (`mem`)？

市面上的主流 Agent Memory 方案（如扁平 Vector RAG 或同步调 LLM 提取 Fact 的黑盒库）在复杂工程任务中存在四大痛点：

| 传统方案痛点 | MemVaultTree (`mem`) 架构解法 |
| :--- | :--- |
| **扁平切片碎纸机效应**（Markdown 知识库切碎后丢失上下文与引用关系） | **Obsidian Wiki 解析 + 面包屑切片 + 1-Hop 双链扩散**：自动提取 YAML `aliases`/`tags`/`entities` 与 `[[Wikilinks]]`，检索命中种子卡片时顺藤摸瓜召回出链 SOP 与反向链接（Backlinks）。 |
| **记忆污染不可逆**（Agent 产生幻觉写入错误记忆后难以清理） | **BLAKE3 CAS 不可变快照 + `mem rewind`**：所有历史树不可篡改，一行 `rewind` 瞬间回滚至健康 Commit，孤立脏数据及索引随后由 `mem gc` 安全清除。 |
| **多 Subagent 并行覆盖冲突** | **亚毫秒零拷贝分支 + 三路合并**：`mem branch` 仅插入一行 `refs` 记录（10,000 节点库耗时 `<1ms`，0 字节额外存储），探索结束通过 `diff` 与 `merge` 归并回 `main`。 |
| **黑盒高写入延迟与强外部依赖** | **热路径零 LLM 调用 + 单文件 `.db`**：`read` / `write` / `search` / `compile_core` 均为纯本地毫秒级操作，无需提前部署 Postgres、Redis、Neo4j 或 Qdrant。 |

---

## 核心常用命令速览

```bash
# 1. 初始化向导 / 切换模型（Ollama 本地开发 <-> OpenAI 生产 <-> Lite 纯 CPU）
mem onboard --mode ollama --seed-demo
mem config use openai --api-key "sk-..."
mem config show

# 2. 导入本地 Obsidian 知识库并检索（支持 1-Hop 双链与反向链接扩散）
mem vault import ~/Documents/MyObsidianVault --branch main --tier semantic
mem commit -m "import obsidian vault"
mem search "BUSY_TIMEOUT"

# 3. Git 风格分支探索、对比、回滚与合并
mem branch exp/refactor --from main
mem write --branch exp/refactor /semantic/findings.md "Use WAL mode"
mem diff main exp/refactor
mem merge exp/refactor main
mem rewind <healthy_commit_id>

# 4. 热路径编译 /core 上下文（<5ms）与自动挂载到 Coding Agents
mem compile --max-tokens 1500
mem mcp install
mem doctor
```

---

## 文档导航

| 文档 | 说明 |
| :--- | :--- |
| [**完整使用说明手册 (docs/USER_GUIDE.md)**](./docs/USER_GUIDE.md) | **推荐阅读**：从安装向导、Obsidian Wiki 编写规范、全 CLI 命令到实战工作流与 FAQ |
| [部署与运维指南 (docs/DEPLOYMENT.md)](./docs/DEPLOYMENT.md) | 预编译安装、Docker 编排、Ollama→OpenAI 切换与 MCP 客户端集成 |
| [系统架构设计 (docs/ARCHITECTURE.md)](./docs/ARCHITECTURE.md) | 总体分层架构、VFS 命名空间、SQLite CAS/Merkle Schema、核心时序图与合并语义 |
| [目录结构与模块规划 (docs/DIRECTORY_STRUCTURE.md)](./docs/DIRECTORY_STRUCTURE.md) | Rust Cargo Workspace 规划与各 Crate 核心 Trait/接口签名 |
| [开发路线图与里程碑 (docs/ROADMAP.md)](./docs/ROADMAP.md) | M1 至 M4 阶段拆分、核心任务与可量化验收标准 |
| [Agent 开发协作指南 (AGENTS.md)](./AGENTS.md) | 供新开 AI 开发 Session 快速接手的架构不变量与技术栈约束 |
