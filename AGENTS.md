# AGENTS.md — 蓝鲸记忆中枢（给接手的 AI / 工程师）

> 这份文件每轮都会进模型上下文，**只写「不读就会犯错」的结论**。
> 论证过程、实测数据、踩坑记录在 `docs/`（尤其 `架构方案.md`、`知识库接入方案.md`）与 `git log` 里，别整读。

## 1. 这是什么

**一个对象的跨渠道记忆服务。** 全库只有「它」一个主体：

- 记忆按 `owner_id = 'me'` 存，**没有多对象 / 多角色 / 多用户**的概念；
- MC 插件、QQ 机器人、直播弹幕是**三个写入口**，共用同一份记忆与同一份人格；
- 不按人分库、不加第二个 `owner_id` 值。「谁」只出现在 `person`（它认识的人），不是记忆的归属者。

### 措辞规范（硬要求）

代码、注释、文档、面板、提交信息里**不要出现「NPC」**。
那是 MC 插件的词，会把产品说成「游戏 NPC 记忆库」。统一用：

| 不要写 | 写成 |
| --- | --- |
| NPC | 它 / 角色 / 这个对象 |
| NPC 记忆 | 记忆 / 它的记忆 |
| 多个 NPC | （本来就不存在；说「多个渠道」） |
| NPC 说的话 | 它说的话 / 回合输出 |

历史文档里的 `npcs/*.json`、`<npc名>/` 是**另一仓库的真实路径**，可以保留字面值，但叙述文字不许再叫 NPC。

## 2. 目录

```
src/
  main.rs        启动、子命令（serve / migrate / snapshot / stats）
  config.rs      环境变量与默认值
  db.rs          建库 + 增量迁移（SCHEMA_VERSION，只增不减）、单写连接 + 读连接池
  model.rs       领域类型（Channel / Scope / Person …）
  http.rs        /v1/*（X-API-Token）：写入、/v1/context 注入、健康检查
  legacy_api.rs  给 MC 插件/QQ 的历史 CRUD（/v1/relation、/v1/message …）
  query.rs       面板与注入用的读模型（分页、关系、场景称呼…）
  admin.rs       /api/admin/*（会话 cookie）：登录、面板接口、关系/身份编辑
  webui.html     单文件管理面板（无 CDN，内网可用）
  kb/            知识库：切块 / 嵌入 / 检索 / 回填
  snapshot.rs    VACUUM INTO 快照
  rollback.rs    逻辑回滚（deleted_by）
  migrate.rs     从 MC 侧旧库 / 计划 JSON 迁移
tests/           http_api.rs（协议）、admin_api.rs（面板）
tools/           smoke.py 等联调脚本
docs/            架构、对接、查询、知识库四份文档
```

## 3. 命令

```bash
# 构建/测试都必须把 CARGO_HOME 指到仓库内，
# 默认 ~/.cargo 在本机沙箱里会 Operation not permitted
export CARGO_HOME=$PWD/.cargo_home

cargo build --release
./scripts-verify.sh        # 格式 + clippy(-D warnings) + 单测 + 两个集成测试 + release
                           # **交付前必须全绿**，只看退出码

# 本机线上（launchd 托管，别用 nohup——会话结束会被整棵进程树回收）
launchctl kickstart -k gui/$(id -u)/com.bluewhale.memory
tail -f data/service.log
curl -s localhost:25590/v1/health
```

- 面板：`http://<主机>:25590`，会话 cookie 有效期 1 年，落库 `panel_session`。
- 数据目录 `data/store/`（可用 `BLUEWHALE_DATA_DIR` 覆盖）。

## 4. 硬规则

1. **备份只能 `VACUUM INTO`**（内置 `snapshot`）。`cp` 抓 `.db` 会丢 WAL 里已提交的事务（实测 5/408 条，零报错）。
2. **两条鉴权链互不替代**：`/v1/*` 用 `X-API-Token`；`/` 与 `/api/admin/*` 用会话 cookie。cookie 不能调 API。
3. **没有 token 就只听回环**；监听内网又没面板口令 → 后台一律 401。
4. **`data/` 绝不入库**：里面有 API token、面板口令、真实记忆库与快照。`.gitignore` 已挡住，别用 `git add -f`。
5. **schema 只增不减**：新列必须 `add_column_if_missing`，新表要进 `TABLES`（`counts()` 与快照校验按它走）。
6. **时间轴唯一主轴是 `at_ms`（现实 UTC 毫秒）**；游戏日 `g<N>` 只是 MC 渠道的标签。
7. **身份绑定必须人工确认**；场景称呼的权威入口是「身份」页，关系页只读展示。
8. **不跨机器共享 `.db`**；多机共享只能走 HTTP。
9. **注释/文档里的转义别写成真实字符**（历史上 `\r\n` 被写进源码把注释劈成两行）。

## 5. 工作流：**每完成一小步就 commit**

这个仓库曾经没有版本控制，被并发会话覆盖过。现在：

```bash
git status            # 改之前先看一眼：有没有别人的未提交改动
# …改…
git add -A && git commit -m "一句话说清这步做了什么"
```

- **一小步 = 一个能编译、能说清楚的改动**（一个功能点 / 一次文档修订），不要攒到最后。
- 提交信息用中文短句，写「为什么」而不只是「改了什么」。
- 改共享文件前先 `git status`；看到不是自己改的未提交内容，先提交或问清楚，别覆盖。
- 判据改动（tests/）与实现改动**同一个提交**里带上，方便回滚。
- 交付前跑 `./scripts-verify.sh`，不要只跑 `cargo build`。

### 并发会话警告

同一账号下可能同时跑多个 DSH 会话，**它们会同时改这个仓库**（已经发生过：一个会话在改 `/v1/context`，另一个在改面板）。
多窗口并行时：

- 用「精确文本替换 + 断言命中」改文件，**不要整文件覆盖**；改前重读；
- 提交前 `git status`，把不属于自己的改动单独 commit 或先沟通；
- 每次提交粒度小，冲突时损失才小。

## 5.0 面板改动的验收（真踩过两次）

前端是**单文件 + 内联脚本**，没有编译期检查、后端测试也覆盖不到。
已经出过两次「后端全绿、用户打开页面就报错」：

- 2026-10-30：`render()` 漏了 `replay` 分派 → 注入回放页只剩一行计数。
- 2026-11-01：往导航加了 `scenes` 项却没在 `PAGES` 里定义 →
  `PAGES[k].icon` 抛 TypeError，**整个侧栏都渲染不出来**。

所以改面板后必须跑：

```bash
node tools/panecheck-all.mjs                              # 假数据，68 条
node tools/panecheck-all.mjs --live /tmp/live_data.json   # 真数据（从线上接口拉）
node tools/panecheck.mjs                                  # 关系/身份的交互细节，34 条
```

`panecheck-all` 做的事：**每个导航页都真渲染一遍** + 导航项与 `PAGES` 一一对应 +
**模拟点击每页的可点元素**（行/卡片/按钮）+ 检查关键渲染函数存在。
`scripts-verify.sh` 的第 6 步已经把它接进来了。

> 写这类判据时最容易犯的错：**重新 parse 一遍 HTML 去点**。
> 那样拿到的是新对象，页面代码挂在上面的 `onclick` 一个都没有，
> 于是「点了 0 个元素」而误报通过。必须用页面自己缓存的那批元素。

**另外**：面板用 `<img src>` / `<audio src>` 直接引的接口
（`/v1/image/{id}/file|/thumb`、`/v1/voice/{id}/file`）
**必须同时接受会话 cookie** —— 浏览器给媒体元素发请求时带不了自定义 header。
这类接口统一用 `check_token_or_session`。

## 5.05 记忆画面：**用户的数据不是测试数据**（真出过事故）

2026-11-01 用户发现线上的 `image` 表被清空。查下来 38 条全是被**逻辑删除**的，
其中真正的用户数据是这 3 条：

| 创建 | 内容 | 谁删的 |
| --- | --- | --- |
| 10:18 | 浏览器上传的图 | 用户自己在面板删的 |
| 10:53 | 在湖边钓上来的一条大鱼 | 待查（疑为验接口时的批量清理） |
| 10:36 | 鲸鲸 人设 | 待查 |

**更严重的是文件**：`data/store/images/` 里这三张的原图**已经不存在**，
而快照（`data/store/snapshots/*.db`）**只备份数据库、不备份图片文件**。
所以记录能恢复、图恢复不了 —— 用户丢的是图本身。

### 规矩（按严重度排序）

1. **绝不「清空表来回到干净状态」。** 要清就清**自己刚写的那几条 id**，
   一条一条删。`DELETE /v1/image/{id}` 是逻辑删除、会写 `rollback_log`；
   但**文件不会因此回来**，删之前先想清楚。
2. **动手前先 `GET /v1/image` 看清楚有哪些不是你的。** 判断"是不是我的探针"
   看 `caption`（`测试图`/`b64multi`/`E2E 探针图`/`探针` 是我的）**和 `created_ms`**
   —— 早于我这次会话开始时间的，一律**不是**我的。
3. **跑测试实例必须用独立数据目录**，而且要用**正确的变量名**：
   `BLUEWHALE_DATA_DIR`（不是 `DATA_DIR`）和 `BLUEWHALE_PORT`（不是 `--port`）。
   写错的话它**静默**落到 `~/.bluewhale-memory` 或**直接抢线上 25590** ——
   后者实测撞见过一次 `Address already in use`，只是运气好没连上线上库。
4. **`pkill -f "bluewhale-memory serve"` 会杀掉线上实例。**
   它有 `KeepAlive` 会自动拉起（这是运气，不是安全网）。要停测试实例，
   记下 PID 精确 kill，不要按名字模糊匹配。
5. **改 schema / 重编二进制前先 `cp` 一份 db 出来**，并且**在副本上验迁移**，
   再让 launchd 用新二进制重启。schema 19 那次就是迁移顺序错（索引建在
   `ADD COLUMN` 之前）导致连崩 7 次。

### 为什么单独写一条而不是塞进 §5.1

§5.1 讲的是「探针行忘了删，会污染读取」——那是**噪声**，删掉就好。
这一条讲的是「把用户数据当探针删了」——那是**丢失**，删了可能再也回不来。
两者代价差一个量级，规矩也不同（后者要求"先看清楚有哪些不是你的"）。

## 5.1 探针数据：写完必须删（真踩过）

2026-11-01 用 `POST /v1/diary {"body":"x"}` 验证接口连通性，忘了删 → 这篇 `x`
因为 `window_end_ms` 默认取「现在」，成了**最新一篇**，第二天用户看到
「昨天的日记是个 x」。同一批还留下 `journal summary='x'`、两条 live 探针弹幕。

规矩：

- 探针**优先用 `GET /v1/health` 和只读接口**，不要写业务表；
- 真要写，就用可识别的值（`dedup_key: "probe-<ts>"`、`content: "探针测试…"`），
  并在**同一次操作里**删掉（`DELETE /v1/{表}/{id}`，逻辑删除，可回滚）；
- 忘记删时的补救：`select` 出来 → `DELETE` 接口逐条删，别直接 `sqlite3 delete`
  （逻辑删除会写 `rollback_log`，能撤销；物理删除不能）。

线上现有探针行的清理记录见 `rollback_log` 里 note = 「接口探针残留」。

## 6. 数据模型（schema 11）

| 表 | 用途 |
| --- | --- |
| `person` / `person_account` | 它认识的人 + 各渠道账号（MC UUID / QQ 号 / 直播 uid） |
| `person_scene` | 同一个人在各场景里的称呼（mc / qq / qq_group / live） |
| `event` / `fact` / `journal` / `diary` | 经历 / 信念 / 手帐 / 日记 |
| `self_narrative` / `mood` / `trait` | 自我认知（带版本）/ PAD 情绪 / 人格特质 |
| `relation` / `promise` | 和某人的关系（熟悉/信任/亲密度/备注）/ 承诺 |
| `channel_message` | QQ/直播原话（按 `scene` 区分群聊与私聊） |
| `recall_log` / `injection_replay` | 检索审计 / 每轮注入回放 |
| `kb` / `kb_doc` / `kb_chunk` | 知识库（外部资料，与记忆分开存） |
| `snapshot` / `rollback_log` / `panel_session` | 运维 |

## 7. 已知未做（README「待办」是权威清单）

- 自我认知的**生成方**未定：Hub 只存不生成（设计上不调模型），谁每晚改写还没接。
- 直播弹幕渠道适配器：平台未定。
- Linux / Windows 打包：目前只有 macOS DMG。
