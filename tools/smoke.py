"""模拟 MC 插件对接 Hub 的完整流程。
不连游戏，直接用 MC 插件会发的那些请求打一遍，看 Hub 能不能全接住。
"""
import json, urllib.request, urllib.error, time, urllib.parse

API = "http://127.0.0.1:25590"
TOK = open('/Users/gleamslime/MemoryHub/data/.token').read().strip()
SUBJECT = "大肥鱼"

def call(method, path, body=None):
    url = API + urllib.parse.quote(path, safe="/?&=%:")
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data,
        headers={"Content-Type":"application/json","X-API-Token":TOK}, method=method)
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.load(e)

ok = fail = 0
def check(name, cond, detail=""):
    global ok, fail
    if cond: ok += 1;   print(f"  ✓ {name}")
    else:    fail += 1; print(f"  ✗ {name}  {detail}")

print("=" * 62)
print("模拟 MC 插件对接流程")
print("=" * 62)

now = int(time.time() * 1000)

# ---- 1. 玩家进服 → 写事件
print("\n[1] 玩家进服，写一条事件")
st, r = call("POST", "/v1/event", {
    "companion_id": SUBJECT, "channel": "mc", "kind": "player.join",
    "summary": "Gleam 进服了", "importance": 0.6,
    "at_ms": now, "dedup_key": f"sim-join-{now}",
})
check("事件写入成功", st == 200 and r.get("id"), f"status={st} {r}")

# ---- 2. 幂等：同一条重发（模拟网络重试）
print("\n[2] 网络重试，同 dedup_key 重发")
st, r = call("POST", "/v1/event", {
    "companion_id": SUBJECT, "channel": "mc", "kind": "player.join",
    "summary": "Gleam 进服了", "importance": 0.6,
    "at_ms": now, "dedup_key": f"sim-join-{now}",
})
check("被判为重复而非新写入", r.get("duplicate") is True, str(r))

# ---- 3. 玩家说话 → 写事件（会推动情绪）
print("\n[3] 挨打事件，验证情绪被推动")
call("POST", "/v1/event", {
    "companion_id": SUBJECT, "channel": "mc", "kind": "attacked",
    "summary": "被苦力怕炸了", "importance": 0.8,
    "dedup_key": f"sim-hit-{now}",
})
time.sleep(0.3)
st, mood = call("GET", f"/v1/mood?companion_id={SUBJECT}")
check("能读到情绪", st == 200 and "label" in mood, str(mood)[:80])
check("挨打后不是平静", mood.get("label") != "平静", f"label={mood.get('label')}")

# ---- 4. 它要说话 → 检索记忆注入提示词
print("\n[4] 检索记忆（注入提示词用）")
st, rows = call("GET", f"/v1/recall?companion_id={SUBJECT}&topk=5")
check("检索返回结果", st == 200 and len(rows) > 0, f"len={len(rows) if isinstance(rows,list) else rows}")
if isinstance(rows, list) and rows:
    check("每条带权重与来源", all("weight" in x and "why" in x for x in rows))
    print(f"      最高权重: {rows[0]['text'][:38]} (w={rows[0]['weight']:.2f})")

# ---- 5. 关键词检索
print("\n[5] 关键词检索「钻石」")
st, rows = call("GET", f"/v1/recall?companion_id={SUBJECT}&q=钻石&topk=3")
hit = [x for x in rows if "钻石" in x.get("text","")] if isinstance(rows,list) else []
check("能按关键词命中", len(hit) > 0, f"hits={len(hit)}")

# ---- 6. 记一条长期事实
print("\n[6] 记事实（同一 key 写两次，验证旧版本被取代）")
for v in ["鱼", "烤鱼"]:
    st, r = call("POST", "/v1/fact", {
        "companion_id": SUBJECT, "channel": "mc", "key": "sim爱吃的",
        "value": v, "tier": "curated", "importance": 0.8,
    })
    check(f"写事实 {v}", st == 200, str(r)[:60])

# ---- 7. 身份绑定
print("\n[7] 绑定玩家 UUID → person")
st, r = call("POST", "/v1/person/bind", {
    "channel": "mc", "account": "sim-uuid-gleam",
    "person_id": "sim-p-gleam", "seen_name": "Gleam",
})
check("绑定成功", st == 200 and r.get("ok"), str(r)[:60])

# ---- 8. 读计划（MC 侧要按计划行动）
print("\n[8] MC 插件读计划 API 是否可用")
st, plans = call("GET", f"/api/admin/plans")  # 后台接口，会话鉴权会拒
check("后台接口对 token 拒绝（预期）", st == 401, f"status={st}")

# ---- 9. 快照
print("\n[9] 拍快照")
st, r = call("POST", "/v1/snapshot", {"note": "模拟测试"})
check("快照成功", st == 200 and r.get("bytes",0) > 0, str(r)[:60])

print("\n" + "=" * 62)
print(f"结果：{ok} 通过 / {fail} 失败")
print("=" * 62)
