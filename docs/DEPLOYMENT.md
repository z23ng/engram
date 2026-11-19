# AI Memory (`mem`) 一键部署与运维指南

本项目提供**零外部服务强依赖**的开箱即用体验：核心引擎编译为单个 `mem` 二进制文件，所有记忆、Merkle Tree 版本历史、FTS5 全文索引、向量与 Obsidian `[[双链]]` 图谱均持久化在单个 SQLite `.db` 文件中。

---

## 1. 三种部署模式对比

| 部署模式 | 适用阶段 | Embedding 模型 | 后台整理 LLM | 硬件要求 |
| :--- | :--- | :--- | :--- | :--- |
| **`ollama`（默认推荐）** | 本地开发 / 离线隐私 | Ollama `bge-m3` (`1024` 维) | Ollama `qwen2.5:7b` | 建议 8GB+ GPU（如 RTX 4080 Super）；若服务离线会自动降级为 Lite 模式 |
| **`openai`** | 生产环境 / 无 GPU 服务器 | OpenAI `text-embedding-3-small` (`1536` 维) | OpenAI `gpt-4o-mini` | 任意 CPU + `curl` + `OPENAI_API_KEY` |
| **`lite`** | 极简纯本地 / CI / 无模型 | 无（仅 FTS5 多列 BM25 + Obsidian `[[双链]]` 1-Hop 图谱） | 无 | 任意双核 CPU + 128MB 内存 |

---

## 2. 一键部署（交互向导 / 命令行）

### 方式 A：交互式向导（最简单）
```bash
./scripts/deploy.sh
# 或使用 Make
make deploy
```

### 方式 B：非交互式一键命令

1. **本地开发默认（Ollama + 演示 Obsidian Wiki 知识库）**：
   ```bash
   ./scripts/deploy.sh --mode ollama --seed-demo
   ```
   *(若尚未安装 Ollama，脚本会优雅提示安装命令，并确保 `mem` 在 Lite 降级模式下立即可用)*

2. **生产环境（OpenAI API + Release 优化编译）**：
   ```bash
   ./scripts/deploy.sh --mode openai --api-key "sk-..." --release
   ```

3. **极简纯 CPU 模式 + 直接导入现有 Obsidian 知识库**：
   ```bash
   ./scripts/deploy.sh --mode lite --vault ~/Documents/MyObsidianVault
   ```

脚本会自动完成以下 6 步：
1. 检查 Rust 工具链（`cargo` / `rustc`）与 C 编译器（`cc` / `gcc`）；
2. 配置并探测模型服务端点（`ollama` / `openai` / `lite`）；
3. 离线编译 `mem-cli` 并安装 `mem` 命令到 `~/.local/bin/mem`；
4. 初始化单文件数据库（默认 `~/.ai-memory/memory.db`），自动生成 `mem.env` 与 `mcp_config.json`；
5. 按需导入本地 Obsidian Vault 或写入演示 Wiki 卡片并执行端到端检索/编译验证；
6. 运行 `mem doctor` 输出完整体检报告。

---

## 3. 从开发态（Ollama）平滑切换至生产态（OpenAI）

整个记忆库数据与版本号完全兼容，切换无需迁移或重导 `.db` 文件，只需修改环境变量（或编辑 `~/.ai-memory/mem.env`）：

```bash
# 切换为生产 OpenAI 配置
export MEM_PROVIDER=openai
export OPENAI_API_KEY="sk-your-production-key"
export MEM_EMBEDDING_BASE_URL="https://api.openai.com/v1"
export MEM_EMBEDDING_MODEL="text-embedding-3-small"
export MEM_EMBEDDING_DIMENSIONS=1536
export MEM_LLM_BASE_URL="https://api.openai.com/v1"
export MEM_LLM_MODEL="gpt-4o-mini"

# 验证配置状态
mem doctor
```

详细模板可参考根目录的 [.env.example](../.env.example)。

---

## 4. 接入 Coding Agent（Claude Code / Cursor / MCP 客户端）

部署脚本已在数据库同级目录生成了可直接粘贴使用的 `~/.ai-memory/mcp_config.json`：

```json
{
  "mcpServers": {
    "ai-memory": {
      "command": "/home/<user>/.local/bin/mem",
      "args": ["--db", "/home/<user>/.ai-memory/memory.db", "mcp"],
      "env": {
        "MEM_PROVIDER": "ollama",
        "MEM_EMBEDDING_MODEL": "bge-m3",
        "MEM_LLM_MODEL": "qwen2.5:7b"
      }
    }
  }
}
```

暴露给 Agent 的 10 个版本化 MCP 工具：
- `mem_read` / `mem_write` / `mem_list`
- `mem_search`（分支隔离 + Obsidian `[[双链]]` 1-Hop 扩散混合检索）
- `mem_compile_core`（`<5ms` `/core` 上下文编译）
- `mem_commit` / `mem_branch` / `mem_rewind` / `mem_diff` / `mem_merge`

---

## 5. Docker 容器化部署（可选）

对于习惯容器化部署的服务器环境，仓库提供了多阶段构建的 [Dockerfile](../Dockerfile) 与 [docker-compose.yml](../docker-compose.yml)：

```bash
# 仅启动单文件 mem 容器（Lite 或对接外部 OpenAI）
MEM_PROVIDER=openai OPENAI_API_KEY=sk-... docker compose up --build

# 同时拉起本地 Ollama 容器
MEM_PROVIDER=ollama docker compose --profile ollama up --build
```

---

## 6. 常用运维与排查命令

```bash
# 全面健康体检（检查 WAL、FTS5、Wiki 图谱表、模型连通性与 /core 编译耗时）
mem --db ~/.ai-memory/memory.db doctor

# 查看分支状态、各层记忆数量与未提交工作区
mem --db ~/.ai-memory/memory.db status

# 一键导入本地 Obsidian 知识库
mem --db ~/.ai-memory/memory.db vault import ~/MyVault --branch main --tier semantic

# 回收已被 rewind 或删除分支遗弃的孤儿 Blob 与 FTS5 索引
mem --db ~/.ai-memory/memory.db gc
```
