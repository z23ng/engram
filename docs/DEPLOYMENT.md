# MemVaultTree (`mem`) 部署与运维指南

本项目采用**单二进制 + 单文件嵌入式 SQLite** 架构，零外部数据库强依赖，支持从开发机平滑切换至生产服务器。

---

## 1. 现代化一键安装（推荐）

### 远程一键安装（自动下载预编译二进制 + 启动交互向导）
```bash
curl -fsSL https://raw.githubusercontent.com/z23ng/memvaulttree/main/install.sh | sh
```

### 从源码仓库安装与运行
```bash
git clone https://github.com/z23ng/memvaulttree.git
cd memvaulttree
./install.sh              # 启动交互式向导（含硬件/GPU/AI客户端探测）
# 或使用 Make 快捷指令
make demo                 # Lite 模式 + 写入 Obsidian Wiki 示例并验证
```

安装完成后，配置已自动持久化至 `~/.ai-memory/config.json`，后续任何 `mem` 命令均可直接运行，无需每次加载环境变量。

---

## 2. 三种运行档案与开发→生产切换

| 运行档案 | Embedding 模型 | 后台整理 LLM | 硬件要求 |
| :--- | :--- | :--- | :--- |
| **`ollama`**（本地开发默认） | Ollama `bge-m3` (`1024` 维) | Ollama `qwen2.5:7b` | 建议 8GB+ GPU；若服务未启动会自动平滑降级为 `lite` |
| **`openai`**（生产推荐） | OpenAI `text-embedding-3-small` (`1536` 维) | OpenAI `gpt-4o-mini` | 任意 CPU + 系统 `curl` + `OPENAI_API_KEY` |
| **`lite`**（极简零模型） | 无 | 无 | 任意双核 CPU + 128MB 内存（FTS5 + Obsidian 双链图谱） |

### 一行命令从本地 Ollama 切到生产 OpenAI（无需迁移数据库）
```bash
# 本地开发期
mem config use ollama
mem doctor

# 上生产后直接切换（~/.ai-memory/memory.db 数据无缝保留）
mem config use openai --api-key "sk-your-production-key"
mem config show
mem doctor
```

---

## 3. Coding Agent (MCP) 一键自动挂载

无需手动复制粘贴 JSON，执行一条命令即可自动探测并无损合并写入已安装的 AI 客户端配置（Cursor、Claude Code、Claude Desktop、Windsurf）：

```bash
mem mcp install
# 或指定单个客户端
mem mcp install --client cursor
mem mcp install --client claude-code
```

---

## 4. Docker 容器化部署（可选）

对于习惯容器化运维的服务器，可直接复用仓库内置的 [Dockerfile](../Dockerfile) 与 [docker-compose.yml](../docker-compose.yml)：

```bash
# 仅启动单文件 mem 容器（Lite 或对接生产 OpenAI）
MEM_PROVIDER=openai OPENAI_API_KEY=sk-... docker compose up --build

# 同时拉起本地 Ollama 容器
MEM_PROVIDER=ollama docker compose --profile ollama up --build
```

---

## 5. 常用运维指令

```bash
# 全面健康体检（WAL、FTS5、blob_links 图谱表、模型连通性与 /core <5ms 校验）
mem doctor

# 查看分支、各层节点数量、暂存区与当前模型配置
mem status

# 备份（仅需复制单个 .db 与 config.json）
cp ~/.ai-memory/memory.db ~/backup/memory.$(date +%F).db
cp ~/.ai-memory/config.json ~/backup/config.json

# 清理已被 rewind 或删分支孤立的数据对象与 FTS5 索引
mem gc
```

更多详细命令、Obsidian Wiki 编写规范与实战工作流请参阅 [完整使用说明手册 (USER_GUIDE.md)](./USER_GUIDE.md)。
