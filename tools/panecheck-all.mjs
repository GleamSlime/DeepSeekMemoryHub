// 面板全量判据：把 webui.html 的脚本跑在假 DOM 上，**逐页真渲染一遍**。
//
// # 为什么必须有它
//
// 面板是单文件 + 内联脚本，没有编译期检查、也没有服务端模板校验。
// 2026-11-01 我往导航里加了一个 `scenes` 项，却忘了在 `PAGES` 里定义它 ——
// 结果是 `PAGES['scenes'].icon` 抛 TypeError，**整个侧栏都渲染不出来**，
// 用户看到的就是「WEB 页面报错」。而后端 421 条测试全绿、`node --check` 也过，
// 因为语法没问题、只是运行时数据缺了。
//
// 所以这个脚本做三件事：
//   1. **每个导航页都真 render 一遍**，任何异常 / 「渲染失败」都算不通过；
//   2. **导航项与 PAGES 必须一一对应**（多一个、少一个都报）；
//   3. **模拟点击**：每页的可点元素（行、卡片、按钮）都点一遍，
//      抽屉能打开、不抛异常。
//
// 用法：node tools/panecheck-all.mjs
// 依赖：node >= 18，无第三方包。

import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const html = fs.readFileSync(path.join(ROOT, 'src/webui.html'), 'utf8');
const js = html.split('<script>')[1].split('</script>')[0];

// ---------------------------------------------------------------- 假数据
//
// 每个接口都给一份**最小但形状正确**的数据。形状错了这里就会炸，
// 那正是我们要抓的（后端改了字段名、面板没跟上）。
const DATA = {
  'api/admin/overview': {
    total_rows: 100, writes: 5, dedup_hits: 1, wal_bytes: 1024, forgotten_rows: 2,
    tables: [['event', 10], ['fact', 2], ['journal', 5], ['diary', 3], ['person', 2],
             ['snapshot', 1], ['rollback_log', 0]],
    by_channel: [['mc', 5], ['qq', 3]], daily: [['10-31', 4], ['11-01', 2]],
    latest_snapshot_ms: 1, snapshot_count: 1, rollback_count: 0,
  },
  'api/admin/session': { login_required: true, user: 'admin', active_sessions: 1 },
  'api/admin/settings': {
    bind: '0.0.0.0:25590', loopback_only: false, data_dir: '/d', db_path: '/d/x.db',
    snapshot_dir: '/d/s', snapshot_keep_days: 30, snapshot_check_seconds: 3600,
    max_concurrency: 10, max_body_bytes: 100000, request_timeout_seconds: 30,
    token_configured: true, panel_login: true, panel_user: 'admin',
    weight: { importance_exp: 1, half_life_days: 30, decay_floor: 0.1,
              reinforce_gain: 0.05, reinforce_cap: 2, channel_trust_mc: 1,
              channel_trust_qq: 0.9, channel_trust_live: 0.8, channel_trust_system: 1 },
  },
  'api/admin/channels': [
    { tag: 'mc', label: 'Minecraft', trust: 1, builtin: true, events: 5 },
    { tag: 'qq', label: 'QQ', trust: 0.9, builtin: true, events: 3 },
  ],
  'api/admin/llm': {
    llm: { base_url: 'https://api.deepseek.com/v1', model: 'deepseek-chat',
           timeout_seconds: 90, max_tokens: 1200, temperature: 0.8, retries: 2,
           api_key: { secret: true, set: true }, ready: true },
    generate: { tick_enabled: true, tick_interval_seconds: 600, tick_batch: 8,
                tick_min_importance: 0.35, diary_at: '04:00', weekly_at: '04:30',
                weekly_day: 1, write_back: true },
  },
  'api/admin/generate/log': {
    items: [
      { id: 1, at_ms: 1, purpose: 'tick', ok: true, trigger: 'batch_full',
        model: 'deepseek-chat', prompt_tokens: 1200, completion_tokens: 300,
        elapsed_ms: 6700, result: '情绪:平静 · 手帐 3 条', error: null },
      { id: 2, at_ms: 2, purpose: 'diary', ok: false, trigger: 'schedule',
        model: 'deepseek-chat', prompt_tokens: 0, completion_tokens: 0,
        elapsed_ms: 500, result: null, error: '401 鉴权失败' },
    ],
    consecutive_failures: { tick: 0, diary: 2, weekly: 0 },
    tick_state: { pending: 3, last_run_ms: 1 },
  },
  'api/admin/personas': [
    { id: 'per1', name: 'flash 版', model_regex: 'deepseek.*flash', body: '你是小模型版。',
      priority: 10, enabled: true, note: null, created_ms: 1, updated_ms: 1, regex_ok: true },
  ],
  'api/admin/traits': { items: [
    { trait_: 'warmth', name: '温和', desc: '待人亲不亲。', value: 0.71,
      updated_ms: 1, word: '偏强' },
  ] },
  'api/admin/relations': { items: [
    { person_id: 'p1', display_name: '姆姆', familiarity: 5, trust: 3, closeness: 20,
      note: '一起挖过矿', first_met_ms: 1, last_met_ms: 2, stage: '认识',
      scenes: [{ scene: 'mc', label: 'MC 里', name: 'GleamSlime', source: 'manual', hint: null }] },
  ] },
  'api/admin/persons': { items: [
    { person_id: 'p1', display_name: '姆姆', status: 'confirmed', created_ms: 1,
      note: '服主', ignored: false,
      accounts: [{ channel: 'qq', account: '478889187', seen_name: '啥子猫',
                   first_seen_ms: 1, last_seen_ms: 2 }],
      scenes: [{ scene: 'mc', label: 'MC 里', name: 'GleamSlime', source: 'manual', hint: null }] },
  ], total: 1, page: 1, pages: 1, size: 50 },
  'api/admin/promises': { items: [
    { id: 'pr1', person_id: 'p1', description: '帮你修屋顶', status: 'open',
      created_ms: 1, due_ms: 2, updated_ms: 1, blocked_reason: null },
  ] },
  'api/admin/moods': { items: [{ valence: 0.2, arousal: 0.3, dominance: 0.1,
                                label: '雀跃', reason: '有人说话', intensity: 0.4,
                                updated_ms: 1, events: [] }] },
  'api/admin/self': { items: [{ id: 1, body: '我是鲸鲸', model: 'm', created_ms: 1,
                                superseded_ms: null, current: true }] },
  'api/admin/plans': [
    { id: 'plan1', planning_day_id: 'g1', summary: '今天的打算', model: 'm',
      game_day: 1, created_ms: 1, updated_ms: 1, total: 3, done: 1 },
  ],
  'api/admin/timeline': { days: [{ day: '11-01', items: [] }] },
  'api/admin/snapshots': [
    { id: 1, taken_ms: 1, path: '/d/s/1.db', bytes: 100, rows: 10, sha256: 'ab', note: null },
  ],
  'api/admin/rollbacks': { items: [
    { id: 1, at_ms: 1, target_ms: 1, mode: 'delete', affected_json: '[]', note: '测试' },
  ] },
  'api/admin/injections': { items: [
    { id: 1, at_ms: 1, lane: 'qq-group', query: '在吗', chars: 100, injected: true,
      replay: 'plan: 已注入 | memory: 已注入', full_text: '# 你是谁\n你是鲸鲸' },
  ] },
  'api/admin/recall-traces': { items: [] },
  'api/admin/messages': { items: [
    { id: 1, at_ms: 1, channel: 'qq', group_id: 'g1', account: '1',
      sender_name: '啥子猫', content: '你好', consumed: 0 },
  ], total: 1, page: 1, pages: 1, size: 50 },
  'api/admin/events': { items: [
    { id: 1, at_ms: 1, kind: 'died', channel: 'mc', summary: '被炸死了',
      importance: 0.83, scope: 'companion', person_id: 'p1', channel_day: null,
      dedup_key: null, importance_source: 'auto',
      importance_reason: '类型 died → 基底 0.78' },
  ], total: 1, page: 1, pages: 1, size: 50 },
  'api/admin/facts': { items: [
    { id: 'f1', person_id: 'p1', key: '爱吃的', value: '烤鱼', scope: 'companion',
      status: 'confirmed', tier: 'curated', confidence: 0.9, importance: 0.8,
      created_ms: 1, updated_ms: 1, expires_ms: null, superseded_by: null, recall_count: 3 },
  ], total: 1, page: 1, pages: 1, size: 50 },
  'api/admin/journal': { items: [
    { id: 'j1', at_ms: 1, type_: 'task_done', source: 'auto', summary: '做完了',
      details: null, importance: 0.5, confidence: 0.5, channel: 'mc',
      planning_day_id: 'g1', recall_count: 0 },
  ], total: 1, page: 1, pages: 1, size: 50 },
  'api/admin/diaries': { items: [
    { id: 'd1', kind: 'day', window_start_ms: 1, window_end_ms: 2, title: '标题',
      body: '正文', model: 'm', created_ms: 1, updated_ms: 1 },
  ], total: 1, page: 1, pages: 1, size: 50 },
  'api/admin/kb': [{ id: 'kb1', name: '默认', doc_count: 1, chunk_count: 3, enabled: true }],
  'api/admin/kb/health': { ok: true, enabled: true, embedder: 'none' },
  'api/admin/persona/preview': { model: 'x', matched: { id: 'per1', name: 'flash 版' }, body: 'x' },
  'v1/health': { ok: true, version: '0.1.0', loopback_only: false, tables: [],
                 schema_version: 18, uptime_ms: 1000, bind: 'x', writes: 1,
                 dedup_hits: 0, wal_bytes: 0 },
  'v1/voice': [
    { id: 'v1', file: 'a.m4a', duration_ms: 1000, bytes: 7954, text: '你好',
      speaker: 'default', voice: 'feiyu', style: 'cute', rating: 5, note: '好',
      channel: 'mc', created_ms: 1, updated_ms: 1 },
  ],
  'v1/image': [
    { id: 'i1', file: 'a.png', thumb: 'a.thumb.jpg', width: 64, height: 48, bytes: 157,
      caption: '一条大鱼', keywords: '钓鱼,湖', sender: '姆姆', channel: 'mc',
      person_ids: 'p1', recall_count: 1, last_recall_ms: 1, created_ms: 1, updated_ms: 1 },
  ],
  'v1/promise/list': [],
};

/// 真实数据（可选）：`node tools/panecheck-all.mjs --live /tmp/live_data.json`
/// 把线上接口的真实返回喂进来，能抓到「字段名对不上」这类假数据掩盖的问题。
let LIVE = null;
{
  const idx = process.argv.indexOf('--live');
  if (idx > 0 && process.argv[idx + 1]) {
    LIVE = JSON.parse(fs.readFileSync(process.argv[idx + 1], 'utf8'));
    console.log(`（使用线上真实数据：${Object.keys(LIVE).length} 个接口）`);
  }
}

/// 按 URL **最后一段** 或包含关系匹配数据源。
function fake(url) {
  const u = String(url).split('?')[0];
  const src = LIVE || DATA;
  // 长的 key 先匹配，避免 `api/admin/journal` 抢走 `v1/journal`
  for (const k of Object.keys(src).sort((a, b) => b.length - a.length)) {
    if (u.endsWith(k)) return src[k];
  }
  return { items: [], total: 0, page: 1, pages: 1, size: 50 };
}

// ---------------------------------------------------------------- 假 DOM
const els = new Map();
const CREATED = [];

function mkEl(id = '') {
  const o = {
    _h: '', _v: '', _cls: '',
    id,
    get innerHTML() { return this._h; },
    set innerHTML(v) { this._h = v; scanIds(v); },
    get value() { return this._v; },
    set value(v) { this._v = String(v); },
    textContent: '', placeholder: '', style: {}, disabled: false, files: [],
    dataset: {},
    classList: {
      add(c) { if (!o._cls.split(/\s+/).includes(c)) o._cls += ' ' + c; },
      remove(c) { o._cls = o._cls.split(/\s+/).filter((x) => x && x !== c).join(' '); },
      toggle() {},
      contains(c) { return o._cls.split(/\s+/).includes(c); },
    },
    appendChild() {}, addEventListener() {},
    querySelector(sel) {
      const all = o.querySelectorAll('button');
      if (sel && sel.includes('.on')) return all.find((b) => b.classList.contains('on')) || null;
      return all[0] || mkEl('q');
    },
    querySelectorAll(sel) {
      if (o._btnsFor !== o._h) {
        o._btnsFor = o._h;
        o._btns = parseClickable(o._h || '');
      }
      const all = o._btns;
      if (sel && sel.includes(':not(.dis)')) return all.filter((b) => !b.classList.contains('dis'));
      // [data-xxx] 选择器：只返回带该属性的
      const m = sel && sel.match(/^\[data-([a-z-]+)\]$/);
      if (m) {
        const key = m[1].replace(/-([a-z])/g, (_, c) => c.toUpperCase());
        return all.filter((b) => b.dataset[key] !== undefined);
      }
      if (sel && sel.startsWith('[data-')) {
        const key = sel.slice(6, -1).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
        return all.filter((b) => b.dataset[key] !== undefined);
      }
      return all;
    },
    setAttribute() {}, getAttribute() { return null; },
    focus() {}, blur() {}, onclick: null,
    click() { if (typeof o.onclick === 'function') o.onclick(); },
  };
  CREATED.push(o);
  return o;
}

const unesc = (v) => v
  .replace(/&quot;/g, '"').replace(/&#39;/g, "'")
  .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');

/// 从 HTML 里抠出所有「可点」的东西：button / 带 data-* 的 tr 和 div。
function parseClickable(html) {
  const out = [];
  const re = /<(button|tr|div)([^>]*)>/g;
  let m;
  while ((m = re.exec(html))) {
    const tag = m[1];
    const attrs = m[2];
    if (tag !== 'button' && !/data-[a-z-]+=/.test(attrs)) continue;
    const cls = (attrs.match(/class="([^"]*)"/) || [])[1] || '';
    const dataset = {};
    for (const am of attrs.matchAll(/data-([a-z-]+)(?:='([^']*)'|="([^"]*)")/g)) {
      const key = am[1].replace(/-([a-z])/g, (_, c) => c.toUpperCase());
      dataset[key] = unesc(am[2] !== undefined ? am[2] : am[3]);
    }
    const el = mkEl('clickable');
    el.dataset = dataset;
    el._cls = cls;
    out.push(el);
  }
  return out;
}

function scanIds(html) {
  for (const m of html.matchAll(/id="([^"]+)"/g)) {
    const id = m[1];
    if (!els.has(id)) els.set(id, mkEl(id));
  }
  // 抽屉里的按钮要能被 querySelectorAll 找到
  for (const [boxId, re] of [
    ['relChips', /id="relChips"[^>]*>([\s\S]*?)<\/div>/],
    ['relMode', /id="relMode"[^>]*>([\s\S]*?)<\/span>/],
    ['psStatus', /id="psStatus"[^>]*>([\s\S]*?)<\/span>/],
    ['psIgnored', /id="psIgnored"[^>]*>([\s\S]*?)<\/span>/],
    ['bindCh', /id="bindCh"[^>]*>([\s\S]*?)<\/span>/],
    ['npStatus', /id="npStatus"[^>]*>([\s\S]*?)<\/span>/],
    ['perEnabled', /id="perEnabled"[^>]*>([\s\S]*?)<\/span>/],
    ['vRate', /id="vRate"[^>]*>([\s\S]*?)<\/span>/],
  ]) {
    const m = html.match(re);
    if (m && els.has(boxId)) els.get(boxId)._h = m[1];
  }
}

const get = (id) => {
  if (!els.has(id)) els.set(id, mkEl(id));
  return els.get(id);
};

const FETCHED = [];
const ctx = {
  console, setTimeout, clearTimeout, setInterval, clearInterval,
  document: {
    getElementById: get,
    querySelector: () => mkEl('q'),
    querySelectorAll: () => [],
    addEventListener() {}, createElement: () => mkEl('c'), body: mkEl('body'),
  },
  window: { addEventListener() {}, location: { search: '', pathname: '/' } },
  location: { search: '', pathname: '/', href: 'http://x/' },
  history: { pushState() {}, replaceState() {} },
  localStorage: { getItem: () => null, setItem() {}, removeItem() {} },
  fetch: async (url, opt = {}) => {
    FETCHED.push(String(url));
    const body = fake(url);
    return {
      ok: true, status: 200,
      json: async () => body,
      text: async () => JSON.stringify(body),
    };
  },
  // 浏览器自带的对话框：面板里用它们做二次确认。
  // 不实现的话「点开可点元素」会误报 —— 那是测试环境缺函数，不是页面有毛病。
  prompt: () => '',
  confirm: () => false,
  alert: () => {},
  encodeURIComponent, decodeURIComponent, URLSearchParams,
  Object, Array, JSON, Math, Date, Number, String, Boolean, Promise, Error,
  RegExp, Map, Set, isFinite, parseInt, parseFloat,
};
ctx.globalThis = ctx;
vm.createContext(ctx);
vm.runInContext(js, ctx);

// ---------------------------------------------------------------- 断言
let pass = 0; let fail = 0;
const ok = (c, m) => { if (c) { pass++; console.log('  ✓', m); } else { fail++; console.log('  ✗', m); } };

// ---- 1. 导航项与 PAGES 一一对应
const navKeys = vm.runInContext('NAV_GROUPS.flatMap(g=>g.items)', ctx);
const pageKeys = vm.runInContext('Object.keys(PAGES)', ctx);
const missing = navKeys.filter((k) => !pageKeys.includes(k));
const orphan = pageKeys.filter((k) => !navKeys.includes(k));
ok(missing.length === 0, `导航里的页面都有定义${missing.length ? '：缺 ' + missing.join(', ') : ''}`);
ok(orphan.length === 0, `PAGES 里的页面都在导航里${orphan.length ? '：多 ' + orphan.join(', ') : ''}`);

// ---- 2. 每个页面都能渲染
const PAGES = vm.runInContext('Object.keys(PAGES)', ctx);
const rendered = {};
for (const p of PAGES) {
  CONTENT_RESET();
  const errs = [];
  const origError = console.error;
  console.error = (...a) => errs.push(a.join(' '));
  try {
    await vm.runInContext(
      `(async()=>{ state.page='${p}'; state.params={}; state.overview=null; await render(); })()`,
      ctx,
    );
    const out = get('content').innerHTML || '';
    const bad = out.includes('渲染失败') || out.includes('页面「');
    ok(!bad && out.length > 0, `${p} 渲染${bad ? '失败' : ` (${out.length} 字)`}`);
    rendered[p] = out;
  } catch (e) {
    ok(false, `${p} 抛异常：${e.message.slice(0, 90)}`);
  } finally {
    console.error = origError;
  }
  const navErr = errs.filter((e) => e.includes('[nav]'));
  if (navErr.length) ok(false, `${p} 导航有缺失项：${navErr[0]}`);
}

// ---- 3. 导航渲染本身不崩
CONTENT_RESET();
try {
  await vm.runInContext(`(async()=>{ state.page='overview'; renderNav(); })()`, ctx);
  const nav = get('nav').innerHTML || '';
  ok(nav.length > 200, `侧栏渲染出内容（${nav.length} 字）`);
  ok(!nav.includes('undefined'), '侧栏里没有 undefined');
} catch (e) {
  ok(false, `侧栏渲染抛异常：${e.message.slice(0, 90)}`);
}

// ---- 4. 点开每一页的可点元素（行 / 卡片 / 按钮），不该抛异常
for (const p of PAGES) {
  if (!rendered[p]) continue;
  CONTENT_RESET();
  await vm.runInContext(
    `(async()=>{ state.page='${p}'; state.params={}; await render(); })()`,
    ctx,
  );
  // 关键：必须用 **content 自己缓存的那批元素** —— 页面代码把 onclick
  // 挂在 `c.querySelectorAll(...)` 返回的对象上。重新 parse 一遍 HTML 会
  // 得到一批新对象、上面一个 onclick 都没有，于是「点了 0 个」而误报通过。
  const clickables = get('content')._btns || [];
  let clicked = 0;
  let threw = null;
  for (const el of clickables) {
    try {
      if (typeof el.onclick === 'function') { el.onclick(); clicked++; }
    } catch (e) {
      threw = `${el.dataset ? JSON.stringify(el.dataset).slice(0, 40) : ''} → ${e.message}`;
      break;
    }
  }
  ok(!threw, `${p} 点开 ${clicked} 个可点元素${threw ? '，某处抛异常：' + threw : ''}`);
}

// ---- 5. 必需的页面函数都存在
for (const fn of [
  'renderOverview', 'renderSystem', 'renderPlans', 'renderMood', 'renderSelf',
  'renderRelations', 'renderTraits', 'renderPromises', 'renderSnapshots',
  'renderRollbacks', 'renderReplay', 'renderKb', 'renderChannels', 'renderPersonas',
  'renderVoices', 'renderImages', 'renderSettings', 'renderTable',
]) {
  const exists = vm.runInContext(`typeof ${fn} === 'function'`, ctx);
  ok(exists, `渲染函数 ${fn} 存在`);
}

function CONTENT_RESET() {
  get('content')._h = '';
  els.delete('drawerBody');
  els.delete('drawerFoot');
  els.delete('drawerTitle');
}

console.log(`\n${pass} 通过 / ${fail} 失败`);
process.exit(fail ? 1 : 0);
