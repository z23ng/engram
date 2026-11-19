# Engram (`mem`) 完整使用说明手册

欢迎使用 **Engram (`mem`)** —— 基于单文件 SQLite CAS + Merkle Tree + Obsidian Wiki 图谱的**版本化 AI Agent 记忆引擎**。

本手册涵盖从一行命令安装、交互式初始化、Obsidian Wiki 编写规范、Git 风格分支/回滚工作流、Ollama 与 OpenAI 一键切换，到 Coding Agent (MCP) 自动接入的全流程指引。

---

## 目录

- [第 1 章：快速安装与现代化引导向导](#第-1-章快速安装与现代化引导向导)
- [第 2 章：记忆分层命名空间与 Obsidian Wiki 编写规范](#第-2-章记忆分层命名空间与-obsidian-wiki-编写规范)
- [第 3 章：CLI 全命令参考手册](#第-3-章cli-全命令参考手册)
- [第 4 章：核心实战工作流（Recipes）](#第-4-章核心实战工作流recipes)
- [第 5 章：Coding Agent (MCP) 自动挂载与工具参考](#第-5-章coding-agent-mcp-自动挂载与工具参考)
- [第 6 章：常见问题与排障指南（FAQ）](#第-6-章常见问题与排障指南faq)

---

## 第 1 章：快速安装与现代化引导向导

### 1.1 一行命令安装（推荐）

无需提前安装 Docker、Postgres 或 Qdrant。打开终端执行：

```bash
curl -fsSL https://raw.githubusercontent.com/z23ng/engram/main/install.sh | sh
```

安装器会自动完成：
1. 识别系统架构（Linux/macOS `x86_64` / `arm64`），优先拉取 GitHub Releases 预编译二进制并校验 SHA256（若当前版本尚未发布预编译包则无缝回退 Cargo 编译）；
2. 将 `mem` 安装到 `~/.local/bin/mem` 并自动配置 `PATH`；
3. 自动拉起 **`mem onboard`** 现代交互式引导向导：
   - 探测本机 CPU、内存、NVIDIA GPU 显存（`nvidia-smi`）、本地 Ollama 状态与已安装的 AI 客户端（Cursor / Claude Code / Claude Desktop / Windsurf）；
   - 将你的配置持久化到 `~/.ai-memory/config.json`（**无需再手动 `source env` 或每次传 `--db`**）；
   - 初始化单文件 SQLite 数据库 `~/.ai-memory/memory.db`；
   - 自动将 MCP Server 合并写入你的 AI 编辑器配置（保留原有其他 MCP Server 不被覆盖）；
   - 执行实时健康校验与检索延迟测试。

### 1.2 非交互式 / CI 快捷安装参数

```bash
# 本地 Ollama 开发模式 + 写入演示 Wiki 笔记
curl -fsSL https://raw.githubusercontent.com/z23ng/engram/main/install.sh | sh -s -- --mode ollama --seed-demo --yes

# 直接导入你现有的 Obsidian 知识库
curl -fsSL https://raw.githubusercontent.com/z23ng/engram/main/install.sh | sh -s -- --mode lite --vault ~/Documents/MyObsidianVault --yes

# 生产环境 OpenAI 模式
curl -fsSL https://raw.githubusercontent.com/z23ng/engram/main/install.sh | sh -s -- --mode openai --api-key "sk-..." --yes
```

### 1.3 三种运行档案（Intelligence Profiles）对比

| 模式标识 | Embedding 向量化 | 后台记忆提炼 LLM | 适用场景 |
| :--- | :--- | :--- | :--- |
| **`ollama`**（开发默认） | 本地 Ollama `bge-m3` (`1024` 维) | 本地 Ollama `qwen2.5:7b` | 有本地显卡/Apple Silicon，完全离线免费运行；若服务未启动会自动平滑降级到 `lite` |
| **`openai`**（生产推荐） | OpenAI `text-embedding-3-small` (`1536` 维) | OpenAI `gpt-6-luna` | 生产服务器、零本地显存占用、按量超低成本计费 |
| **`lite`**（零模型极速） | 无（纯 CPU） | 无 | 轻量服务器、CI 测试或仅使用多列加权 FTS5 + Obsidian `[[双链]]` 1-Hop 图谱检索 |

---

## 第 2 章：记忆分层命名空间与 Obsidian Wiki 编写规范

### 2.1 四层虚拟文件系统（VFS Namespace）

所有记忆必须位于以下四个受强类型校验的顶级前缀之一（自动阻止 `..` 路径穿越）：

| 路径前缀 | 认知层级 | 读写与生命周期特征 | 典型存放内容 |
| :--- | :--- | :--- | :--- |
| **`/core/`** | 核心工作记忆 | 体积极小，每次对话由 `mem compile` 在 `<5ms` 内全量拼入 System Prompt | Agent 人设、安全红线、当前项目技术栈、活跃阶段目标 |
| **`/semantic/`** | 语义知识记忆 | 结构化实体/知识卡片，支持持续迭代修改、冲突检测与双链织网 | 架构决策记录（ADR）、数据库选型、业务术语、用户长期偏好 |
| **`/procedural/`** | 程序性技能记忆 | 面向报错/任务场景的操作剧本（SOP），与 `/semantic/` 通过双链互引 | Debug 排查手册、部署步骤、工具避坑指南、常用命令模板 |
| **`/episodic/`** | 情景轨迹记忆 | 按日期/会话归档的只追加时间线日志 | 对话摘要、实验尝试记录、工具调用成功/失败观测日志 |

### 2.2 高命中率 Obsidian Wiki 卡片编写规范

为了让 `mem` 的**多列加权 BM25 + 面包屑向量 + 双链 1-Hop 图谱**发挥最高召回率，推荐在 `/semantic/` 与 `/procedural/` 中采用标准 Obsidian Markdown 格式：

```markdown
---
title: SQLite 并发与锁配置
aliases: ["BUSY_TIMEOUT", "SQLite 写冲突", "database locked"]
tags: ["db/sqlite", "architecture/storage"]
entities:
  - "SQLite -> uses -> WAL Mode"
  - "SQLite -> requires -> BUSY_TIMEOUT"
importance: 0.92
---
# 存储引擎

## WAL 并发配置
- 开启 `PRAGMA journal_mode = WAL` 支持单写多读并发。
- 必须配置 `busy_timeout = 5000`，详细排查步骤见 [[/procedural/sqlite-lock-sop.md|锁排查SOP]]。
- 相关整体架构见 [[存储架构导航 (MOC)]]。
```

**为什么这样写命中率极高？**
1. **`title` 与 `aliases` 享受 5 倍 BM25 权重**：即使提问使用的是缩写或别名（如搜 `"BUSY_TIMEOUT"` 或 `"database locked"`），也能排在第一位。
2. **标题面包屑注入（Breadcrumb Chunking）**：切分切片时自动把 `[Title | Aliases | Tags | Section: # 存储引擎 > ## WAL 并发配置]` 拼接到切片头部，消除长文档切块导致的语义丢失。
3. **`[[Wikilink]]` 1-Hop 图谱扩散（支持路径、文件名、标题、别名解析）**：当 `/semantic/sqlite-wal.md` 被命中时，它链接的 `/procedural/sqlite-lock-sop.md`（出链）以及引用了它的 `architecture-moc.md`（入链/反向链接）即使正文里没有 `"BUSY_TIMEOUT"` 关键词，也会被自动召回并标明 `Outlinks` / `Backlinks`。

### 2.3 多角色协同目录归属（PM / 开发 / Bot）

使用 `mem ingest` 或 `mem vault init-template` 时，不同协作角色的记忆默认分域如下（也可通过 `--path` / `--tier` 显式覆盖）：

| 角色 (`--role`) | 默认层级 | 默认 VFS 路径前缀 | 默认 `importance` | 默认标签 | `--draft` 隔离分支 |
| :--- | :--- | :--- | :---: | :--- | :--- |
| **`pm`** | `/semantic/` | `/semantic/product/` | `0.75` | `role/pm`, `semantic` | `draft/pm-inbox` |
| **`dev`** | `/semantic/` | `/semantic/architecture/` *(Procedural: `/procedural/runbooks/`)* | `0.85` | `role/dev`, `semantic` | `draft/dev-inbox` |
| **`bot`** | `/episodic/` | `/episodic/bot-insights/` *(Semantic: `/semantic/bot-notes/`)* | `0.55` | `role/bot`, `episodic` | `draft/bot-inbox` |

---

## 第 3 章：CLI 全命令参考手册

### 3.1 全局选项与配置优先级

所有命令均支持将全局选项放在子命令前或后：

```bash
mem [--db <PATH>] [--config <PATH>] [--provider ollama|openai|disabled] [--json] <COMMAND>
```

配置解析优先级从高到低为：
1. 命令行显式参数（`--db`、`--provider`）
2. 环境变量（`MEM_DB_PATH`、`MEM_PROVIDER`、`OPENAI_API_KEY`、`MEM_EMBEDDING_*`、`MEM_LLM_*`）
3. 持久化配置文件（`$MEM_CONFIG_PATH` 或 `~/.ai-memory/config.json`）
4. 默认内置预设（`ollama` 本地默认配置）

### 3.2 初始化、配置切换与体检

| 命令 | 说明与常用示例 |
| :--- | :--- |
| **`mem onboard`** *(别名 `setup`)* | 启动现代化交互式安装向导。<br>`mem onboard --mode ollama --seed-demo` |
| **`mem config show`** | 查看当前生效的配置文件路径、数据库路径及模型档案。<br>`mem config show --json` |
| **`mem config use <PROFILE>`** | 一键热切换模型档案并持久化保存（`ollama` \| `openai` \| `lite`）。<br>`mem config use openai --api-key "sk-..."`<br>`mem config use ollama` |
| **`mem doctor`** | 全面健康检查：验证 SQLite WAL、FTS5、`blob_links` 图谱表、模型端点可达性及 `/core` 编译耗时。 |
| **`mem completions <SHELL>`** | 输出 Shell 自动补全脚本（`bash` \| `zsh` \| `fish`）。<br>`mem completions zsh > ~/.zfunc/_mem` |

### 3.3 记忆读写、检索与编译

| 命令 | 说明与常用示例 |
| :--- | :--- |
| **`mem write <VPATH> <CONTENT>`** | 向指定分支的工作区写入记忆节点（即时对当前分支可见并建索引）。<br>• 直接传文本：`mem write /core/goal.md "Ship v1" --importance 1.0 --tags core,goal`<br>• 从本地文件写入：`mem write /semantic/adr.md --file ./adr.md`<br>• 从标准输入写入：`cat note.md \| mem write /semantic/note.md -` |
| **`mem read <VPATH>`** | 读取记忆节点内容（默认读取工作区视图，也可加 `--commit <HASH>` 读取历史快照）。<br>`mem read /semantic/sqlite-wal.md --branch exp/opt` |
| **`mem ls [PREFIX]`** | 列出分支下活跃记忆路径、重要性评分与短哈希。<br>`mem ls /semantic --branch main` |
| **`mem search <QUERY>`** | 执行分支隔离的 **BM25 + 向量 + 1-Hop 双链图谱扩散**混合检索。<br>`mem search "BUSY_TIMEOUT" --branch main --prefix /semantic -k 5` |
| **`mem compile`** | 按 `(importance DESC, path ASC)` 在 `<5ms` 内确定性编译 `/core` 上下文块。<br>`mem compile --max-tokens 1500` |
| **`mem ingest [OPTS] <TEXT \| ->`** | 将 PM / Dev / Bot 的自然语言口语草稿结构化为 Obsidian Wiki 卡片并自动编织 `[[Wikilinks]]`。<br>`mem ingest --role pm --draft "超时重试最多 3 次" --title "Payment Retry"`<br>支持 `--role pm\|dev\|bot`、`--draft`（隔离入 `draft/{role}-inbox`）、`--tag`、`--llm`（可选 `gpt-6-luna`/Ollama 润色，离线自动降级为确定性模板） |
| **`mem vault init-template <DIR>`** | 一键生成协同 Obsidian Vault 目录骨架（`core/`、`semantic/product/`、`semantic/architecture/`、`procedural/runbooks/`、`episodic/bot-insights/`、`_templates/`）。<br>`mem vault init-template ./team-vault` |
| **`mem vault import <DIR>`** | 递归导入本地 Obsidian Vault 至指定单一层级（跳过 `.obsidian/`、`.git/` 与 `_templates/`）。<br>`mem vault import ~/Documents/MyVault --branch main --tier semantic --commit` |
| **`mem vault sync <DIR>`** | 将四层协同 Obsidian Vault 按目录映射全量同步至目标分支（跳过根 `README.md` 与 `_templates/`），并增量构建图谱与向量索引。<br>`mem vault sync ./team-vault --branch main --commit` |
| **`mem vault export <DIR>`** | 将指定分支工作区或 `--commit <HASH>` 导出为标准四层本地 Obsidian 目录树，便于可视化阅览与双向协作。<br>`mem vault export ./team-vault --branch main` |

### 3.4 Git 风格版本控制（分支、提交、对比、回滚、合并、GC）

| 命令 | 说明与常用示例 |
| :--- | :--- |
| **`mem init`** | 手动在指定 `--db` 初始化空记忆库并创建 `main` 分支。 |
| **`mem status`** | 查看当前分支 HEAD、各层活跃文档数、CAS 对象统计与工作区暂存变更。 |
| **`mem commit -m <MSG>`** | 将当前分支的工作区变更固化为不可变 Merkle Tree 与 Commit。<br>`mem commit -m "add sqlite wal decisions" --author architect` |
| **`mem log [-n <N>]`** | 按时间逆序查看当前分支的 Commit DAG 历史。 |
| **`mem branch [NAME]`** | • 无参数列出全部分支；<br>• 零拷贝创建新分支：`mem branch exp/redis-test --from main`；<br>• 删除分支：`mem branch -d exp/redis-test`（受保护分支 `main` 不可删除）。 |
| **`mem diff <BASE> <TARGET>`** | 基于 Merkle Tree 子树哈希快速剪枝比对任意两个分支或 Commit 哈希。<br>`mem diff main exp/redis-test` |
| **`mem rewind <COMMIT_ID>`** | **防污染瞬间回滚**：原子重置分支指针至目标 Commit，并彻底清空脏工作区。<br>`mem rewind 0d601d27c282... --branch main` |
| **`mem merge <SRC> <DST>`** | 执行三路合并（支持 `--strategy auto \| ours \| theirs`，默认 `auto`）。<br>`mem merge exp/redis-test main --strategy auto` |
| **`mem gc`** | 执行 Mark-and-Sweep（标记-清除）垃圾回收，清理被 `rewind` 或删分支孤立的 Blob/Tree/FTS5 行。 |

---

## 第 4 章：核心实战工作流（Recipes）

### Recipe 1：一键把本地 Obsidian 知识库接入 AI Agent

```bash
# 1. 导入整个 Obsidian Vault（不会修改你的本地原文件）
mem vault import ~/Documents/ObsidianVault --branch main --tier semantic
mem commit -m "import obsidian knowledge base"

# 2. 检索关键词或 frontmatter 别名，观察 1-Hop 双链与反向链接扩散效果
mem search "数据库并发锁" -k 5
```

### Recipe 2：多 Subagent 零拷贝并行探索与三路合并

当主 Agent 需要同时派两个 Subagent 调研不同方案时，各自拉出隔离分支，绝对不会互相覆盖记忆：

```bash
# 亚毫秒级零拷贝派生两个探索分支
mem branch exp/postgres --from main
mem branch exp/sqlite --from main

# Subagent 1 与 Subagent 2 在各自隔离分支写入并发现结论
mem write --branch exp/postgres /semantic/db-eval.md "Recommend Postgres for multi-region writes"
mem commit --branch exp/postgres -m "postgres evaluation"

mem write --branch exp/sqlite /semantic/db-eval.md "Recommend SQLite WAL for single-file edge deployment"
mem write --branch exp/sqlite /procedural/wal-checklist.md "# WAL Checklist\nSet busy_timeout=5000"
mem commit --branch exp/sqlite -m "sqlite wal evaluation"

# 主 Agent 审查差异，决定采纳 exp/sqlite 并合并回 main
mem diff main exp/sqlite
mem merge exp/sqlite main
mem branch -d exp/postgres
mem branch -d exp/sqlite
mem gc
```

### Recipe 3：发现 Agent 写入幻觉后的一键“时间旅行”回滚

```bash
# 1. 查看健康的历史 Commit ID
mem log -n 5

# 2. 原子回滚到幻觉发生前的健康快照（并自动丢弃未提交脏工作区）
mem rewind <healthy_commit_id> --branch main

# 3. 确认幻觉内容与它引入的虚假双链已彻底从检索视图消失
mem search "hallucinated term"
```

### Recipe 4：本地开发（Ollama）无痛切换到生产（OpenAI）

```bash
# 本地开发期：跑在 RTX 显卡 / Apple Silicon 的 Ollama 上（免费离线）
mem config use ollama
mem doctor

# 上线生产期：一行命令切换至 OpenAI（.db 数据库与所有历史版本无需任何迁移）
mem config use openai --api-key "sk-your-prod-key"
mem doctor
```

### Recipe 5：PM + 开发 + Bot 多角色协同喂数据（Obsidian Vault + IDE MCP + Draft Inbox）

当团队中 **PM（产品需求/业务规则）**、**开发（架构 ADR/Runbook）** 与 **Bot（线上观察/会话总结）** 共享同一个 Engram 知识库时，建议采用“**分域命名空间 + 双向 Vault + 草稿收件箱分支**”工作流，既降低非研发人员录入成本，又绝对避免垃圾/半成品直接污染 `main`：

1. **Obsidian Vault 协同模板 + `sync` / `export` 双向同步**：

```bash
# 初始化四层协同目录结构与角色模板（不会覆盖已有笔记）
mem vault init-template ./team-vault

# 团队成员在 Obsidian 中分别维护：
#   team-vault/semantic/product/       (PM：PRD、验收口径、FAQ)
#   team-vault/semantic/architecture/  (开发：ADR、表结构、接口不变量)
#   team-vault/procedural/runbooks/    (开发/运维：排障 SOP)
#   team-vault/episodic/bot-insights/  (Bot：会话复盘与边缘案例)

# 双向同步入库与导出快照
mem vault sync ./team-vault --branch main --commit
mem vault export ./team-vault --branch main
```

2. **开发在 IDE 中用 MCP `mem_write` / `mem_ingest` 边写代码边沉淀**：
   Coding Agent（Cursor / Claude Code / Windsurf）在解决复杂 Bug 或确定接口不变量后，直接调用 MCP 工具 **`mem_write`** 写入精确路径（例如 `/semantic/architecture/idempotency.md`），或调用 **`mem_ingest`** 将讨论结论自动编织成带 `[[Wikilinks]]` 的结构化卡片并 `mem_commit`。

3. **PM / 运营 / Bot 用 `mem ingest --role pm|dev|bot --draft [--llm]` 丢口语草稿进隔离信箱 + 审核合入**：

```bash
# PM 无需了解文件路径或 Markdown 语法，口语片段自动匹配主干笔记并进入 draft/pm-inbox 隔离分支
mem ingest --role pm --draft --title "支付超时重试策略" \
  --tag payment,prd \
  "所有重试必须遵守 Idempotency Key，最多重试 3 次。"

# 可选开启 --llm，借助已配置的 gpt-6-luna 或本地 Ollama qwen2.5:7b 自动润色分节（端点不可达时零报错退回本地模板）
mem ingest --role bot --draft --llm "Load test retry succeeded on attempt 2 for checkout API"

# 研发或 PM Owner 审核差异后再合入主干，Bot 在 main 上立即通过 1-Hop 双链搜到最新规范
mem diff main draft/pm-inbox
mem merge draft/pm-inbox main
```

---

## 第 5 章：Coding Agent (MCP) 自动挂载与工具参考

### 5.1 自动配置 AI 客户端

无需手写复杂的 JSON，直接执行：

```bash
# 自动扫描并合并写入 Cursor / Claude Code / Claude Desktop / Windsurf（保留已有 MCP 配置）
mem mcp install

# 或仅安装到指定客户端
mem mcp install --client cursor
mem mcp install --client claude-code
```

### 5.2 暴露给 Agent 的 11 个核心 MCP Tools

| 工具名称 | 核心参数 | 功能描述 |
| :--- | :--- | :--- |
| **`mem_read`** | `path`, `branch?`, `commit_id?` | 读取指定分支工作区或精确历史 Commit 中的记忆文件 |
| **`mem_write`** | `path`, `content`, `tags?`, `importance?`, `branch?` | 写入或更新 Markdown 记忆并实时建立 Wiki 图谱与 FTS5 索引 |
| **`mem_ingest`** | `text`, `role?`, `draft?`, `title?`, `path?`, `tier?`, `tags?`, `use_llm?`, `branch?` | 将自然语言草稿按 `pm/dev/bot` 角色结构化、自动编织 `[[Wikilinks]]` 并支持入 `draft/{role}-inbox` 隔离审查 |
| **`mem_list`** | `prefix?`, `branch?` | 列出当前分支或某目录前缀下全部活跃记忆节点 |
| **`mem_search`** | `query`, `prefix?`, `branch?`, `top_k?` | 分支强隔离的 BM25 + Vector + 1-Hop `[[Wikilink]]` 混合检索 |
| **`mem_compile_core`** | `branch?`, `max_tokens?` | `<5ms` 确定性编译 `/core` 高密度上下文块 |
| **`mem_commit`** | `message`, `branch?`, `author?` | 将工作区改动固化为不可变 Merkle Tree 快照 |
| **`mem_branch`** | `name`, `from?` | 零拷贝创建隔离记忆分支 |
| **`mem_rewind`** | `commit_id`, `branch?` | 原子回滚分支并丢弃未提交污染记忆 |
| **`mem_diff`** | `base`, `target` | 结构化比对两个分支或 Commit 间的增删改差异 |
| **`mem_merge`** | `source`, `target`, `strategy?` | 三路合并分支并自动检测层级与同路径冲突 |

---

## 第 6 章：常见问题与排障指南（FAQ）

1. **Q：本地没有安装或启动 Ollama，`mem` 还能用吗？**
   **A**：可以。当探测到 Ollama 离线时，`mem` 会自动平滑降级为 **Level-1 模式（SQLite 多列加权 FTS5 BM25 + Obsidian 双链 1-Hop 图谱扩散）**，所有版本控制、导入、检索与 MCP 工具均可正常工作。
2. **Q：切换模型（例如从 `ollama` 切到 `openai`）会导致已有记忆库丢失吗？**
   **A**：不会。`~/.ai-memory/config.json` 仅控制推理端点与模型名称，`~/.ai-memory/memory.db` 中的所有文档、Merkle 树和历史 Commit 100% 保留。注意：若切换不同维度的 Embedding 模型（如从 `1024` 维的 `bge-m3` 切到 `1536` 维的 `text-embedding-3-small`），`cosine_similarity` 会忽略维度不匹配的旧向量并依靠 FTS5 + 双链正常工作，如需让旧文档使用新模型向量，可在新库重新 `vault sync` 或 `vault import`。
3. **Q：生产环境连接 `https://api.openai.com/v1` 需要什么条件？**
   **A**：系统需安装 `curl`（Linux / macOS 默认自带，Docker 镜像已内置），并通过 `mem config use openai --api-key "sk-..."` 或环境变量 `OPENAI_API_KEY` 配置有效密钥。
4. **Q：如何备份或迁移整个记忆库？**
   **A**：Engram 采用单文件嵌入式架构，只需复制 `~/.ai-memory/memory.db`（建议先停止写入或用 SQLite `.backup` 命令）和 `~/.ai-memory/config.json` 即可完整迁移所有历史版本与图谱索引。
5. **Q：PM 或 Bot 用 `mem ingest --draft` 写入后，线上 Bot 为什么一时搜不到？**
   **A**：这是有意为之的安全隔离：`--draft` 会自动派生并写入 `draft/pm-inbox` 或 `draft/bot-inbox` 隔离分支，防止未审核草稿直接污染 `main`。执行 `mem diff main draft/pm-inbox` 检查无误并 `mem merge draft/pm-inbox main` 合入后，`main` 分支即可立即检索。
6. **Q：执行 `mem ingest --llm` 时如果 Ollama 没启动或未配置 `OPENAI_API_KEY` 会报错吗？**
   **A**：不会报错。当 LLM 端点不可达、服务禁用或缺少 API Key 时，`ingest` 会自动回退为本地确定性 Obsidian Markdown 模板 + `[[Wikilink]]` 自动织链完成写入与提交。
