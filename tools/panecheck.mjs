// 面板离线判据：不启服务器，把 webui.html 的脚本跑在假 DOM 上。
//
// 为什么要有它：面板是单文件 + 内联脚本，没有编译期检查；
// 一次改名（比如把 `relSc_*` 换成 `psSc_*`）在浏览器里只是「点了没反应」，
// 后端测试一条都不会红。这个脚本把「渲染 → 点开抽屉 → 填值 → 保存」
// 真的走一遍，并断言发出的请求体。
//
// 用法：node tools/panecheck.mjs
// 依赖：node >= 18（无第三方包）
//
// 覆盖范围（与「关系 / 身份」两页的职责划分一起改）：
//   - 关系页只读展示各场景称呼；抽屉里没有场景输入框；保存只发数值与备注
//   - 身份页可编辑场景称呼；保存请求体带 scenes
//   - 「去身份页改称呼」会跳到身份页并打开那个人的抽屉
//   - 新建关系的请求体不带 scenes（场景称呼归身份页）

import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const html = fs.readFileSync(path.join(ROOT, 'src/webui.html'), 'utf8');
const js = html.split('<script>')[1].split('</script>')[0];

// ---------------------------------------------------------------- 假数据
const REL = {
  items: [{
    person_id: 'p-gleam', display_name: 'Gleam', familiarity: 5, trust: 3, closeness: 20,
    note: '一起挖过矿', first_met_ms: 1000, last_met_ms: 2000, stage: '认识',
    scenes: [
      { scene: 'mc', label: 'MC 里', name: 'Gleam', source: 'derived', hint: 'uuid-gleam' },
      { scene: 'qq', label: 'QQ 私聊', name: 'Gleam', source: 'derived', hint: '10001' },
      { scene: 'qq_group', label: 'QQ 群', name: '群里的Gleam', source: 'manual', hint: null },
      { scene: 'live', label: '直播间', name: null, source: 'none', hint: null },
    ],
  }],
};
const PERSONS = {
  items: [
    {
      person_id: 'p-gleam', display_name: 'Gleam', status: 'confirmed', created_ms: 1,
      note: '服主',
      accounts: [
        { channel: 'mc', account: 'uuid-gleam', seen_name: 'Gleam' },
        { channel: 'qq', account: '10001', seen_name: 'Gleam' },
      ],
      scenes: REL.items[0].scenes,
    },
    { person_id: 'p-2', display_name: '还没关系的人', status: 'pending', created_ms: 2, note: null, accounts: [], scenes: [] },
  ],
  total: 2, page: 1, pages: 1, size: 200,
};

// ---------------------------------------------------------------- 假 DOM
const els = new Map();
const calls = [];

/// 从一段 innerHTML 里把可点的元素（button / 带 data-* 的 tr）抠出来，
/// 让 querySelectorAll('.chip') / ('[data-ps]') 这类调用有真对象可点、可点开。
function parseButtons(html) {
  const out = [];
  const re = /<(?:button|tr)([^>]*)>/g;
  let m;
  while ((m = re.exec(html))) {
    const attrs = m[1];
    const cls = (attrs.match(/class="([^"]*)"/) || [])[1] || '';
    const dataset = {};
    // 面板里 JSON 值用单引号包（data-ps='{"a":1}'），简单值用双引号。
    // 两种都要认，否则点开抽屉的 data-ps 永远解析不出来。
    // 属性值是 HTML 转义过的（esc 会把引号变成 &quot;），浏览器解析时会还原，
    // 假 DOM 也要还原，否则 JSON.parse(JSON) 永远失败。
    const unesc = (v) => v
      .replace(/&quot;/g, '"').replace(/&#39;/g, "'")
      .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');
    attrs.replace(/data-([a-z_]+)='([^']*)'/g, (_, k, v) => { dataset[k] = unesc(v); return ''; });
    attrs.replace(/data-([a-z_]+)="([^"]*)"/g, (_, k, v) => { dataset[k] = unesc(v); return ''; });
    const el = mkEl('btn');
    el.dataset = dataset;
    el._cls = cls;
    el.classList = {
      add(c) { if (!el._cls.split(/\s+/).includes(c)) el._cls += ' ' + c; },
      remove(c) { el._cls = el._cls.split(/\s+/).filter((x) => x && x !== c).join(' '); },
      toggle() {},
      contains(c) { return el._cls.split(/\s+/).includes(c); },
    };
    out.push(el);
  }
  return out;
}

/// 抽屉 HTML 里出现的 id 要能再被 getElementById 取到；
/// relChips / relMode 里的按钮还要能被 querySelectorAll 找到并点。
function scanIds(html) {
  for (const m of html.matchAll(/id="([^"]+)"/g)) {
    const id = m[1];
    if (!els.has(id)) els.set(id, mkEl(id));
  }
  const chips = html.match(/id="relChips"[^>]*>([\s\S]*?)<\/div>/);
  if (chips && els.get('relChips')) els.get('relChips')._h = chips[1];
  const mode = html.match(/id="relMode"[^>]*>([\s\S]*?)<\/span>/);
  if (mode && els.get('relMode')) els.get('relMode')._h = mode[1];
}

function mkEl(id) {
  return {
    id, _h: '', _v: '',
    get innerHTML() { return this._h; },
    set innerHTML(v) { this._h = v; scanIds(v); },
    get value() { return this._v; },
    set value(v) { this._v = String(v); },
    textContent: '', placeholder: '', style: {}, disabled: false,
    classList: { add() {}, remove() {}, toggle() {}, contains() { return false; } },
    dataset: {}, appendChild() {}, addEventListener() {},
    querySelector(sel) {
      const all = this.querySelectorAll ? this.querySelectorAll('button') : [];
      if (sel && sel.includes('.on')) return all.find((b) => b.classList.contains('on')) || null;
      return all[0] || mkEl('q');
    },
    querySelectorAll(sel) {
      // 同一段 HTML 只解析一次：重复解析会产出新对象，代码挂上去的 onclick 就丢了。
      if (this._btnsFor !== this._h) {
        this._btnsFor = this._h;
        this._btns = parseButtons(this._h || '');
      }
      const all = this._btns;
      if (sel && sel.includes(':not(.dis)')) return all.filter((b) => !b.classList.contains('dis'));
      return all;
    },
    setAttribute() {}, getAttribute() { return null; }, focus() {}, blur() {}, onclick: null,
    // 真实 DOM 的 .click() 会触发 onclick；面板里用 `hit.click()` 自动打开抽屉。
    click() { if (typeof this.onclick === 'function') this.onclick(); },
  };
}

const get = (id) => {
  if (!els.has(id)) els.set(id, mkEl(id));
  return els.get(id);
};

const ctx = {
  console, setTimeout, clearTimeout,
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
    calls.push({ url: String(url), method: opt.method || 'GET', body: opt.body });
    const u = String(url).split('?')[0];
    const last = u.substring(u.lastIndexOf('/') + 1);
    let data = { ok: true, items: [], total: 0, page: 1, pages: 1, size: 20 };
    if (last === 'relations') data = REL;
    else if (last === 'persons') data = PERSONS;
    return { ok: true, status: 200, json: async () => data, text: async () => JSON.stringify(data) };
  },
  encodeURIComponent, decodeURIComponent, URLSearchParams,
  Object, Array, JSON, Math, Date, Number, String, Boolean, Promise, Error, RegExp, Map, Set,
};
ctx.globalThis = ctx;
vm.createContext(ctx);
vm.runInContext(js, ctx);

// ---------------------------------------------------------------- 断言
let pass = 0; let fail = 0;
const ok = (cond, msg) => { if (cond) { pass++; console.log('  ✓', msg); } else { fail++; console.log('  ✗', msg); } };

// 1. 关系列表
await vm.runInContext(`(async()=>{ state.page='relations'; state.params={}; await render(); })()`, ctx);
const list = get('content').innerHTML;
ok(list.includes('Gleam'), '关系列表渲染出人名');
ok(list.includes('各场景里的称呼'), '表头有「各场景里的称呼」');
ok(list.includes('群里的Gleam'), '列表显示 QQ 群名片');
ok(list.includes('+ 新建关系'), '列表有新建按钮');

// 2. 关系抽屉：只读场景 + 只改数值/备注
vm.runInContext(`openRelationEdit(__rel)`, Object.assign(ctx, { __rel: REL.items[0] }));
const drawer = get('drawerBody').innerHTML;
ok(drawer.includes('id="relClo"'), '抽屉有亲密度输入');
ok(!drawer.includes('id="relName"'), '抽屉不再提供显示名输入（归身份页）');
ok(!drawer.includes('id="relSc_'), '抽屉不再提供场景称呼输入（归身份页）');
ok(drawer.includes('群里的Gleam'), '场景称呼仍然只读展示');
ok(get('drawerFoot').innerHTML.includes('去身份页改称呼与账号'), '有跳去身份页的按钮');

get('relClo').value = '75';
get('relNote').value = '一起盖过房';
await get('relSave').onclick();
const patch = calls.find((c) => c.method === 'PATCH');
ok(!!patch, '保存发出 PATCH');
const pb = patch ? JSON.parse(patch.body) : {};
ok(pb.closeness === 75, '请求体带亲密度');
ok(pb.note === '一起盖过房', '请求体带关系备注');
ok(pb.scenes === undefined && pb.display_name === undefined, '请求体不再混入身份字段');

// 3. 跳到身份页 → 自动打开那个人的抽屉
vm.runInContext(`state.pendingPerson = 'p-gleam'; state.page='persons'; state.params={};`, ctx);
await vm.runInContext(`(async()=>{ await render(); })()`, ctx);
const idrawer = get('drawerBody').innerHTML;
ok(idrawer.includes('id="psSc_mc"'), '身份抽屉有 MC 称呼输入');
ok(idrawer.includes('id="psSc_qq_group"'), '身份抽屉有 QQ 群称呼输入');
ok(idrawer.includes('群里的Gleam'), '人工设置的名字带出到输入框');
ok(idrawer.includes('当前自动：Gleam'), '自动推导值作为提示');

// 4. 身份抽屉保存：带 scenes
get('psSc_qq_group').value = '';
get('psSc_live').value = '直播间的Gleam';
await get('psSave').onclick();
const ppatch = calls.filter((c) => c.method === 'PATCH').pop();
const ppb = ppatch ? JSON.parse(ppatch.body) : {};
ok(ppatch.url.includes('/api/admin/person/p-gleam'), '身份保存打到 person 接口');
ok(ppb.scenes && ppb.scenes.qq_group === '', '清空的场景传空串（后端回退自动）');
ok(ppb.scenes && ppb.scenes.live === '直播间的Gleam', '新场景称呼进请求体');

// 5. 新建身份：身份页应有「+ 新增」，并在创建时发 POST /api/admin/persons
await vm.runInContext(`(async()=>{ state.page='persons'; state.params={}; await render(); })()`, ctx);
ok(get('content').innerHTML.includes('+ 新增'), '身份页有新增按钮');
await vm.runInContext(`openNewPerson()`, ctx);
const np = get('drawerBody').innerHTML;
ok(np.includes('id="npName"'), '新建身份有显示名输入');
ok(np.includes('id="npStatus"'), '新建身份有状态单选');
ok(np.includes('id="npSc_qq_group"'), '新建身份能填 QQ 群称呼');
get('npName').value = '新登记的人';
get('npNote').value = '还没出现过';
get('npSc_qq_group').value = '群里的新登记';
await get('npCreate').onclick();
const npPost = calls.filter((c) => c.method === 'POST').pop();
const npBody = npPost ? JSON.parse(npPost.body) : {};
ok(npPost.url.endsWith('/api/admin/persons'), 'POST 打到身份接口');
ok(npBody.display_name === '新登记的人', '请求体带显示名');
ok(npBody.status === 'confirmed', '默认状态是已确认');
ok(npBody.scenes && npBody.scenes.qq_group === '群里的新登记', '请求体带场景称呼');

// 6. 新建关系：选身份 + 创建，请求体不带 scenes
await vm.runInContext(`(async()=>{ await openNewRelation(); })()`, ctx);
const nw = get('drawerBody').innerHTML;
ok(nw.includes('relChips'), '新建时有身份选择');
ok(!nw.includes('id="relSc_'), '新建关系不再填场景称呼（去身份页补）');
const chips = get('relChips').querySelectorAll('.chip:not(.dis)');
ok(chips.length === 1, '已有关联的身份被灰掉');
chips[0].onclick();
get('relClo').value = '30';
get('relNote').value = '新朋友';
await get('relCreate').onclick();
const post = calls.filter((c) => c.method === 'POST').pop();
const postBody = post ? JSON.parse(post.body) : {};
ok(postBody.person_id === 'p-2', 'POST 带上选中的身份');
ok(postBody.closeness === 30 && postBody.note === '新朋友', 'POST 带上数值与备注');
ok(postBody.scenes === undefined, 'POST 不带 scenes');

console.log(`\n${pass} 通过 / ${fail} 失败`);
process.exit(fail ? 1 : 0);
