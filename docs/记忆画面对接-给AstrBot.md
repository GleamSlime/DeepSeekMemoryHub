# 记忆画面 · AstrBot 对接说明

> 给接手 `astrbot_plugin_deepseek` 的同学。
> 所有示例都在**线上实例真跑过**（2026-11-01），不是照文档编的。
>
> 服务地址 `http://server.gleamslime.com:25590`
> 鉴权：`X-API-Token: <token>`（在记忆中枢的 `data/.token`）

---

## 0. 一句话讲清这是什么

**图片也是记忆。** 有个表存图，每条图带「描述 + 关键词」。
它解决两件事：

| 需求 | 用哪个接口 | 效果 |
| --- | --- | --- |
| **A. 让它记得那张图** | `GET /v1/context`（自动） | 命中关键词时，上下文里出现「你记得的画面」段 |
| **B. 让它能看到那张图** | `POST /v1/image` 上传 + 把图附进 `req.image_urls` | 多模态模型真的看到图 |
| **C. 发一张图出去** | `GET /v1/image/pick` | 挑一张出来发到群里（表情包 / 回忆） |

**B 是最容易做错的一步**，第 4 节专门讲。

---

## 1. 数据从哪来（谁负责上传）

中枢**不抓图**，全靠接入方上传。目前两个来源：

| 来源 | 谁传 | 时机 |
| --- | --- | --- |
| 群友发的图 | `astrbot_plugin_deepseek` | 收到消息时（`_archive_images`） |
| 机器人自己生成的图 | TTS / 截图插件 | 生成后 |

用 `POST /v1/image`，见第 3 节。

---

## 2. 接口总表

| 方法 | 路径 | 作用 | 鉴权 |
| --- | --- | --- | --- |
| `POST` | `/v1/image` | 上传一张（multipart，或 `base64` 字段） | token |
| `GET` | `/v1/image` | 列表（`?q=` 搜索，`?limit=`） | token |
| `GET` | `/v1/image/{id}` | 单条元数据 | token |
| `GET` | `/v1/image/{id}/file` | **取原图字节**（附图用这个） | token 或面板会话 |
| `GET` | `/v1/image/{id}/thumb` | 取缩略图（列表页用） | token 或面板会话 |
| `PATCH` | `/v1/image/{id}` | 改描述/关键词/关联人 | token 或会话 |
| `DELETE` | `/v1/image/{id}` | 逻辑删除（可回滚） | token 或会话 |
| `GET` | `/v1/image/pick` | **按关键词挑一张**（表情包用） | token |

> `file` / `thumb` 接受**面板会话 cookie**，因为浏览器 `<img src>` 带不了 header。
> 接入方用 token 就行。

---

## 3. 上传：`POST /v1/image`

### 3.1 必填规则（**会 400，必须处理**）

`caption` 和 `keywords` **至少填一个**。

原因：都不填的图命中不了任何句子，它就只是图库、白占空间。
只填 `keywords` 时，**至少要有一个 ≥2 字的词**（单字不算命中）。

```bash
# ✅ 有描述
curl -X POST -H "X-API-Token: $TOK" \
  -F "file=@fishing.png" \
  -F "caption=在湖边钓上来的一条大鱼" \
  -F "keywords=钓鱼,湖边,大鱼" \
  -F "sender=姆姆" -F "channel=qq" \
  http://HOST:25590/v1/image
```

```json
{"bytes":157,"file":"img-1793510464091-057b7140.png","height":48,
 "id":"img-1793510464091-057b7140","ok":true,
 "thumb":"img-1793510464091-057b7140.thumb.jpg","width":64}
```

```json
// ❌ 两个都没填
{"error":"请求不合法：caption 或 keywords 至少填一个 —— 否则这张图永远想不起来，只是占地方",
 "kind":"bad_request","retryable":false}
```

### 3.2 字段表

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `file` | 二选一 | 图片二进制（multipart） |
| `base64` | 二选一 | 裸 base64 或 `data:image/png;base64,…`（不带 file 时用） |
| `caption` | **至少一个** | 一句话描述。**不参与匹配**，只给人看/给模型读 |
| `keywords` | **至少一个** | 逗号分隔的**触发词**。`钓鱼,湖边` |
| `sender` | 否 | 谁发的（昵称）。**用于忽视名单判断** |
| `channel` | 否 | `qq` / `mc` / … 未注册的 tag 会 400 |
| `person_ids` | 否 | 逗号分隔的 person_id |
| `rating`/`note` | 否 | 语音库用，图片忽略 |

### 3.3 去重（**会自动发生**）

按**内容 sha256** 判。同一张图重复上传：

```json
{"duplicate":true,"id":"img-1793510464091-057b7140","ok":true}
```

- 不会重复占盘，不会新建记录；
- **逻辑删除之后可以重新上传**（索引只对未删除的行生效）；
- 所以接入方要处理 `duplicate: true` 这个分支（别当失败）。

### 3.4 忽视名单

`sender` 命中一个被标「忽视」的人时，**图片直接跳过**，不报错：

```json
{"ignored":true,"ok":true,"skipped":true}
```

注意：他的**消息/事件照常入库**（忽视 ≠ 当他没说过话），只有图被拒 ——
因为图会被检索进上下文，那才算「处理」。

---

## 4. ⚠️ 让它看到图：**不能把 base64 塞进 system_prompt**

### 4.1 这是踩过的坑

```
❌ 错误做法
system_prompt += "![描述](data:image/png;base64,iVBOR...)"
```

**模型只会把这段当乱码文本读**，图等于没给。

多模态模型看图的唯一途径是消息 `content` 数组里的结构：

```json
{"role":"user","content":[
  {"type":"text","text":"你说的那张图"},
  {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBOR..."}}
]}
```

AstrBot 对应的字段是 **`ProviderRequest.image_urls`**。
（源码：`astrbot/core/provider/sources/openai_source.py` 的 `assemble_context`
把它拼成 content block；`_resolve_image_part` 对 `data:` 开头的地址**原样透传**。）

### 4.2 正确流程

```
① POST /v1/context  →  文本里有「（图 id: img-xxx）」
② GET  /v1/image/img-xxx/file  →  图片字节
③ base64 编码  →  data:image/png;base64,…
④ req.image_urls.append(那个 data URL)
```

**为什么第 ② 步要插件自己取**：`image_urls` 里的地址是**模型服务端**去拉的，
而中枢在内网、还要 token —— 模型那边根本取不到。所以必须先取好、转 data URL。

### 4.3 参考实现（插件里已经有，别重写）

```python
# 从上下文里解析 id —— 正则要限定在「画面段」内，别把别处的 img-xxx 也捞进来
idx = text.find("# 你记得的画面")
seg = text[idx:]
nxt = seg.find("\n# ", 1)
if nxt > 0:
    seg = seg[:nxt]
ids = re.findall(r"图 id: (img-[A-Za-z0-9_-]+)", seg)

for i in ids[:max_n]:
    data = GET f"{HUB}/v1/image/{i}/file"        # 带 X-API-Token
    data_url = f"data:{ctype};base64,{base64.b64encode(data).decode()}"
    req.image_urls.append(data_url)
```

| 配置项 | 默认 | 作用 |
| --- | --- | --- |
| `image_attach` | `true` | 总开关。关掉仍保留文字描述，只是不附图 |
| `image_attach_max` | `1` | 最多几张。一张图约 **1–2k token**，多附会把记忆段挤掉 |

代码位置：`main.py` 的 `_context_images()` 与 `_fetch_image_data_url()`，
调用点在 `inject_memory` 里（拿到 `req` 之后、发请求之前）。上限 2 MB。

### 4.4 失败时怎么办

- 取图失败 / 超 2 MB → **只记日志，不附图，正常回答**（图是锦上添花，不能拖垮回复）；
- 模型不支持看图 → 它会说「看不到图」，那是**网关/模型**的事，不是这里格式错了；

---

## 5. 上下文里的「画面段」长什么样

`GET /v1/context?q=去钓鱼吗&scene=group`

```markdown
# 你记得的画面
这些图你见过、记得内容（要看图得由接入方附上）：
- 湖边的鱼（关键词：钓鱼,湖边）（图 id: img-1793510464091-057b7140 ）
```

**触发条件（两个都试）**：

1. 当前这句 `q` 命中某张图的关键词；
2. **最近 3 条消息**命中 —— 群里常是「图 → 哈哈这啥 → 你说呢」的接力，
   只看当前句会漏掉图。

**不命中就不出现**：`parts` 里没有 `images` 这一段（实测确认过）。

命中会让 `recall_count += 1`（面板上「想起 N 次」）。

> 注意：`caption` **不参与匹配**。它只描述「这张图是什么」。
> 早期版本拿 caption 做 2 字滑窗，结果「今天天气不错」「我们走吧」「一起吃饭吗」
> 全都能命中 caption「今天我们一起去湖边钓鱼」—— 现在不会了。

---

## 6. 发图：`GET /v1/image/pick`

```bash
# ① 表情包：发得少的优先（别反复刷同一张）
curl -H "X-API-Token: $TOK" "HOST:25590/v1/image/pick?q=开心,笑"

# ② 回忆：用得多的优先（常说起的才有代表性）
curl -H "X-API-Token: $TOK" "HOST:25590/v1/image/pick?q=开心&mode=recall"
```

```json
{"ok":true,"found":true,"mode":"emoji",
 "id":"img-…","caption":"湖边的鱼","keywords":"钓鱼,湖边",
 "file":"/v1/image/img-…/file","recall_count":0}
```

```json
// 没找到（不是错误，200）
{"found":false,"mode":"emoji","ok":true}
```

### 6.1 `mode` 的两种排序**是相反的**

| `mode` | 用途 | 排序 |
| --- | --- | --- |
| `emoji`（默认） | 发张表情包活跃气氛 | `recall_count` **升序** —— 发得少的优先 |
| `recall` | 「你记得那次…吗」把具体那张翻出来 | `recall_count` **降序** —— 常说起的优先；同次数时最近用过的优先 |

**为什么不合成一个**：只按「发得少」，想回忆时会翻出一张从没用过的；
只按「用得多」，表情包会反复刷同一张。两边的正确答案是相反的。

**参数表**：

| 参数 | 必填 | 说明 |
| --- | --- | --- |
| `q` | ✅ | 关键词，逗号分隔。**每个词都要 ≥2 字**（单字 400） |
| `mode` | | `emoji`（默认）/ `recall` |
| `channel` | | 只在这个渠道的图里挑，如 `qq` |

`mode` 拼错或 `q` 全是单字都会 **400**：

```json
{"error":"请求不合法：mode 只能是 emoji 或 recall，收到 recal"}
```

### 6.2 完整用法

```python
r = GET f"{HUB}/v1/image/pick?q={quote(词)}&mode=emoji"
if r["found"]:
    png = GET f"{HUB}{r['file']}"        # 带 token
    # 发到群里
```

---

## 7. 典型接法：群里收到表情包

```
群友发了一张表情包
  ↓
插件：POST /v1/image（caption/keywords 自己填，或用多模态模型生成描述）
  ↓  入库后
后续有人说「开心」/「笑」
  ↓
GET /v1/image/pick?q=开心&mode=emoji
  ↓
拿到 file → 取字节 → 发群里
```

或者反过来：想**回**一张表情包，就用**刚收到那条消息里的词**去 `pick`。

---

## 8. 面板上的样子

浏览器打开 `http://HOST:25590/` → 左侧「素材 → 记忆画面」。

- 卡片墙 + 缩略图，可搜索（描述/关键词/发送者）；
- 点开：上传 / 改描述关键词 / 关联人 / 删除；
- 显示「想起 N 次」和尺寸大小。

**上传时会强制填描述或关键词** —— 和接口同一套规则。

---

## 9. 排错清单

| 现象 | 原因 | 检查 |
| --- | --- | --- |
| 上传 400「至少填一个」 | 没给 caption/keywords | 如字面 |
| 上传 400「keywords 至少一个 2 字以上」 | 全是单字词 | 换成 `钓鱼` 而不是 `鱼` |
| `duplicate: true` | 同一张图传过了 | **不是错误**，用返回的 id |
| `ignored: true` | 发送者在忽视名单 | 面板「身份」页确认 |
| 上下文里没有画面段 | 关键词没命中 | 用 `/v1/image/pick?q=…` 试那个词；注意**关键词必须 ≥2 字** |
| 模型说「看不到图」 | 图没进 `req.image_urls` | 看插件日志有没有 `已附 N 张图给模型` |
| 面板图裂开 | 只带 cookie 没带 token | `file`/`thumb` 支持 cookie，确认登录态没掉 |

**日志关键字**（生产服务器上）：

```
[deepseek] 上下文里提到 1 张图，附 1 张给模型
[deepseek] 已附 1 张图给模型
```

---

## 10. 现在线上是什么状态（2026-11-01）

| 项 | 值 |
| --- | --- |
| schema | 19 |
| 有效图片 | 见面板（自检数据已清） |
| 单张典型大小 | ~44 KB |
| 存储位置 | `data_dir/images/`（原图 + 缩略图） |
| 去重 | 按 sha256，部分唯一索引 |
| 回滚 | **包含 image 表**（`ROLLBACK_TABLES`） |

---

## 附：源码地图

| 文件 | 内容 |
| --- | --- |
| `src/image.rs` | 存图、缩略图、sha256、关键词匹配、`recent_text` |
| `src/http.rs` | 全部 `/v1/image*` 与 `/v1/context` 的画面段 |
| `src/context.rs` | `render_images_ref()` —— 只有引用，没有 base64 |
| `src/db.rs` | `image` 表 + `image_digest` 索引 |
| `插件/astrbot_plugin_deepseek/main.py` | `_context_images()` / `_fetch_image_data_url()` |
