#!/bin/sh
# 蓝鲸记忆中枢启动脚本。给 launchd 用，也可以手动跑。
set -e
cd /Users/gleamslime/MemoryHub
export BLUEWHALE_DATA_DIR="$PWD/data/store"
export BLUEWHALE_PORT=25590
export BLUEWHALE_BIND=lan
export BLUEWHALE_TOKEN="$(cat data/.token)"
export BLUEWHALE_PANEL_USER=admin
export BLUEWHALE_PANEL_PASSWORD="$(cat data/.panel_pw)"

# ------------------------------------------------------------ 知识库
#
# 嵌入服务在另一台机器上（192.168.3.252 的 Ollama）。
#
# **它挂了不影响记忆功能**：嵌入是软依赖，知识库检索自动退回纯字面，
# 接口仍返回 200 并在响应里带 degraded 字段。
#
# 模型选 0.6b-q8 / 512 维，是 2026-10-30 实测定的：
#   - 0.6b 与 8b 逐条对比 14:17（8B 并没有更好），但快 5.2 倍、省 3.6 倍内存
#   - 512 与 1024 维差异在噪声内，存储与点积各省一半
# 详见 docs/知识库接入方案.md §5.6
export BLUEWHALE_KB_ENABLE=true
export BLUEWHALE_KB_EMBED_URL=http://192.168.3.252:11434/v1/embeddings
export BLUEWHALE_KB_EMBED_MODEL=qwen3-embedding:0.6b-q8
export BLUEWHALE_KB_EMBED_DIM=512
# 跨机器调用比本机慢，30 秒超时给冷加载与批量嵌入留余量
export BLUEWHALE_KB_EMBED_TIMEOUT=30

exec ./target/release/bluewhale-memory serve
