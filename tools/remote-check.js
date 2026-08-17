#!/usr/bin/env node
'use strict';
//
// スマホ画面 (`assets/remote/`) を **本物のブラウザで描いて**不変条件を確かめる。
//
//   使い方は tools/remote-check.sh から。直接なら:
//     node tools/remote-check.js                  # 通常の検査
//     node tools/remote-check.js --self-test      # 検査そのものを検査する
//     node tools/remote-check.js --inject <名前>  # 欠陥を仕込んで赤を確かめる
//     node tools/remote-check.js --list           # 仕込める欠陥の一覧
//     node tools/remote-check.js --keep           # 失敗時にページを書き出す
//
// ## なぜ「本物のブラウザ」なのか
//
// 0.18 系で漏れたスマホのバグは **CSS の詳細度**だった
// (`#alist.mid` が `#alist{display:none}` を打ち消して、端末と一覧が同時に
// 見えた)。Rust の単体テストは 1 本も落ちていない — 誰も CSS を評価して
// いなかったからで、当然である。**評価していないものは検査できない。**
//
// 手で書いた DOM もどきや jsdom では「カスケードの真実」は出せない
// (jsdom は npm 依存も要る)。Chromium 系を CDP で叩けば
// **実物の CSS エンジンの getComputedStyle** が答えるので嘘が無い。
// WebSocket も握手とフレームだけ自前で書くので、**npm パッケージも新しい
// Node も要らない** (Node 18 以上で動く)。
//
// ## 何を見るか (「既知のバグ」ではなく**不変条件**を見る)
//
//   1. 見えている `.view` はいつでもちょうど 1 つ
//   2. 1 つの `.view` の中で、場所を分け合う主枠 (flex:1 の子) が同時に
//      2 つ以上見えない  ← 今回のバグはここに出る
//   3. 同じ親の子どうしの矩形が重ならない
//   4. 横にはみ出さない (overflow が visible な入れ物で scrollWidth 超過なし)
//   5. 見えているボタンは指で押せる大きさがある (0px の幽霊ボタンを作らない)
//   6. JS 例外・console.error が 1 件も出ない
//   7. エージェント画面を開いている間、ポーリングが**実際に飛んでいる**
//      (「押しても永久に変わらない」の正体)
//   8. エージェントのチップを押したら、端末の中身が**そのエージェントへ変わる**
//
// これを **全ての遷移順序** (下部ナビ × セグメント × チップの総当たりと乱歩) で
// 確かめる。順番を列挙するのは人ではないので、**誰も思い付かなかった順番**が
// 見つかる = 未知のバグが出る。
//
// ## どの環境でも動くこと
//   - パスは全て `__dirname` / `os.tmpdir()` / 環境変数から導く (直書き禁止)
//   - ブラウザが無ければ `[skip]` (理由つき) で降りる。**黙って緑にしない**
//   - 実 `~/.zaivern` には 1 バイトも触らない (PC 側は偽物を立てる)

const fs = require('fs');
const os = require('os');
const path = require('path');
const http = require('http');
const net = require('net');
const crypto = require('crypto');
const { spawn } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
const REMOTE = path.join(ROOT, 'assets', 'remote');

// ─────────────────────────────────────────────────────────────────────
// 1. ページを組む — **build.rs の generate_remote_assets と同じ順序**
// ─────────────────────────────────────────────────────────────────────
// 順序がずれると「本番と違うものを検査して緑」という最悪の嘘になる。
// 材料の名前は Rust 側の番人 (`cli::tests::スマホ画面の検査はbuild_rsと同じ材料を読む`)
// が build.rs と突き合わせている。
function buildPage(opt) {
  const head = fs.readFileSync(path.join(REMOTE, 'page-head.html'), 'utf8');
  const css = fs.readFileSync(path.join(REMOTE, 'style.css'), 'utf8');
  const body = fs.readFileSync(path.join(REMOTE, 'body.html'), 'utf8');
  const jsDir = path.join(REMOTE, 'js');
  const jsFiles = fs.readdirSync(jsDir).filter(f => f.endsWith('.js')).sort();
  if (!jsFiles.length) throw new Error(jsDir + ' に .js がありません');
  const js = jsFiles.map(f => fs.readFileSync(path.join(jsDir, f), 'utf8')).join('');

  let page = head + '<style>\n' + css + '</style>\n</head>\n' + body
    + '<script>\n' + js + (opt.appendJs || '') + '</script>\n</body>\n</html>\n';

  // 言語パックの注入は remote.rs と同じ差し込み口を使う (原文フォールバック
  // だけを見ていると、訳が長い言語での見切れを永久に見逃す)。
  if (opt.lang) {
    const dictAll = JSON.parse(fs.readFileSync(path.join(ROOT, 'locales', opt.lang + '.json'), 'utf8'));
    const dict = {};
    for (const k of Object.keys(dictAll)) if (k.startsWith('remote.')) dict[k] = dictAll[k];
    page = page.replace('/*__ZV_I18N__*/', 'window.ZVI18N=' + JSON.stringify(dict) + ';');
  }
  for (const [from, to] of (opt.replace || [])) {
    if (!page.includes(from)) {
      throw new Error('仕込みの目印が見つかりません (assets/remote/ の中身が変わった可能性): '
        + JSON.stringify(from.slice(0, 60)));
    }
    page = page.split(from).join(to);
  }
  return page;
}

// ─────────────────────────────────────────────────────────────────────
// 2. 偽の PC 側 — 実 zaivern を起こさずに、同じ API を返す
// ─────────────────────────────────────────────────────────────────────
function makeModel(nAgents) {
  const icons = ['\u{1F916}', '\u{1F9E0}', '✨', '\u{1F41D}'];
  const agents = [];
  for (let i = 0; i < nAgents; i++) {
    agents.push({
      id: 'ag' + i, idx: i, title: 'agent-' + i, icon: icons[i % icons.length],
      running: i !== 2, attention: i === 0, unread: i === 1, stalled: i === 2,
      waiting: i === 0, lane: i % 3, state: ['待っています', '動いています', '停まっています'][i % 3],
      uptime: (i + 1) + 'm', preview: 'line A\nline B', active: i === 0,
    });
  }
  return {
    nAgents,
    agent_active: 0,
    lanes: [
      { i: 0, icon: '\u{1F514}', title: '要対応' },
      { i: 1, icon: '▶', title: '実行中' },
      { i: 2, icon: '⏸', title: '停止' },
    ],
    agents,
  };
}

function apiPayload(model, url) {
  const p = url.split('?')[0];
  const running = model.agents.filter(a => a.running).length;
  switch (p) {
    case '/api/state':
      return {
        ok: true, workspace: 'zv-demo', file: 'src/lib.rs', dirty: false,
        tabs: [{ title: 'lib.rs', dirty: false }, { title: 'main.rs', dirty: true }],
        active: 0, agent_active: model.agent_active,
        waiting: model.agents.filter(a => a.waiting).length,
        agents: model.agents,
        bulk: { one: Math.min(1, model.nAgents), all: running, stalled: model.agents.filter(a => a.stalled).length },
        presets: [{ icon: '\u{1F916}', name: 'claude' }, { icon: '\u{1F9E0}', name: 'codex' }],
        approval: 'ask',
      };
    case '/api/agents':
      return { ok: true, agents: model.agents, lanes: model.lanes };
    case '/api/term': {
      const a = model.agents[model.agent_active];
      if (!a) return { ok: false };
      return { ok: true, text: termText(a, 40) };
    }
    case '/api/approvals':
      return {
        ok: true, items: model.agents.filter(a => a.attention).map((a, i) => ({
          id: 'ap' + i, agent: a.title, kind: 'edit', detail: 'src/lib.rs を書き換えます', since: 12,
        })),
      };
    case '/api/files':
      return { ok: true, files: ['src/lib.rs', 'src/app.rs', 'docs/i18n.md', 'tools/verify.sh'] };
    case '/api/file':
      return { ok: true, path: 'src/lib.rs', text: 'fn main() {}\n' };
    case '/api/changes':
      return {
        ok: true, added: 3, removed: 1, truncated: false,
        files: [{ rel: 'src/lib.rs', status: 'M', added: 3, removed: 1, binary: false }],
      };
    case '/api/diff':
      return {
        ok: true, binary: false, truncated: false,
        hunks: [{ header: '@@ -1,3 +1,4 @@', new_start: 1, lines: [
          { k: 'ctx', o: 1, n: 1, t: 'fn main() {' },
          { k: 'add', o: 0, n: 2, t: '    println!("hi");' },
          { k: 'del', o: 2, n: 0, t: '    // old' },
        ] }],
      };
    default:
      return { ok: true };
  }
}

// 端末の中身には**必ずエージェント名を混ぜる**。混ぜておかないと
// 「チップを押しても中身が変わらない」を機械が判定できない。
function termText(a, lines) {
  const out = [];
  for (let i = 0; i < lines; i++) out.push('[' + a.title + '] line ' + i);
  return out.join('\n');
}

function startServer(page, model, opt) {
  const hits = Object.create(null);
  const srv = http.createServer((req, res) => {
    const url = req.url || '/';
    const p = url.split('?')[0];
    hits[p] = (hits[p] || 0) + 1;
    if (p === '/' || p === '/index.html') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(page);
      return;
    }
    if (p === '/favicon.ico') { res.writeHead(204); res.end(); return; }
    if (p === '/api/scrollback') {
      // 「この版には無い」経路も本番にある。opt.scrollback=false でそちらを通す。
      if (!opt.scrollback) { res.writeHead(404); res.end('no'); return; }
      const q = new URLSearchParams(url.split('?')[1] || '');
      const want = Math.max(1, Math.min(400, parseInt(q.get('lines') || '80', 10) || 80));
      const total = 400;
      const before = q.get('before') === null ? null : (parseInt(q.get('before'), 10) || 0);
      const idx = q.get('agent') === null ? model.agent_active : (parseInt(q.get('agent'), 10) || 0);
      const a = model.agents[idx] || model.agents[0];
      if (!a) { res.writeHead(400); res.end('none'); return; }
      const end = before === null ? total : before;
      const from = Math.max(0, end - want);
      const rows = [];
      for (let i = from; i < end; i++) {
        rows.push({ spans: [{ t: '[' + a.title + '] line ' + i, fg: null, bg: null }] });
      }
      json(res, { ok: true, from, total, rows });
      return;
    }
    if (p.startsWith('/api/')) {
      let body = '';
      req.on('data', c => { body += c; });
      req.on('end', () => {
        // 宛先の切り替えだけは**状態を持つ**。持たないと「押しても変わらない」を
        // 検出できない (偽物が常に同じ答えを返すので、いつでも緑になる)。
        try {
          const j = body ? JSON.parse(body) : null;
          if (p === '/api/cmd' && j && j.name === 'agent_focus') {
            const n = parseInt(j.arg, 10);
            if (!isNaN(n) && model.agents[n]) model.agent_active = n;
          }
        } catch (e) { /* 本文が無い GET など */ }
        json(res, apiPayload(model, url));
      });
      return;
    }
    res.writeHead(404); res.end('no');
  });
  return new Promise(resolve => {
    // ポートは 0 (空きを OS に選ばせる)。番号を直書きすると同時実行で衝突する。
    srv.listen(0, '127.0.0.1', () => {
      resolve({
        url: 'http://127.0.0.1:' + srv.address().port + '/',
        hits,
        close: () => new Promise(r => srv.close(r)),
      });
    });
  });
}

function json(res, obj) {
  const b = Buffer.from(JSON.stringify(obj));
  res.writeHead(200, { 'Content-Type': 'application/json', 'Content-Length': b.length });
  res.end(b);
}

// ─────────────────────────────────────────────────────────────────────
// 3. ブラウザを探す (無ければ理由を出して降りる)
// ─────────────────────────────────────────────────────────────────────
function findBrowser() {
  if (process.env.ZV_BROWSER) {
    return fs.existsSync(process.env.ZV_BROWSER) ? process.env.ZV_BROWSER : null;
  }
  const env = process.env;
  const cands = [];
  if (process.platform === 'darwin') {
    const apps = ['Google Chrome', 'Chromium', 'Microsoft Edge', 'Brave Browser', 'Google Chrome Canary'];
    for (const base of [path.sep + 'Applications', path.join(env.HOME || '', 'Applications')]) {
      for (const a of apps) cands.push(path.join(base, a + '.app', 'Contents', 'MacOS', a));
    }
  } else if (process.platform === 'win32') {
    const roots = [env.PROGRAMFILES, env['PROGRAMFILES(X86)'], env.LOCALAPPDATA].filter(Boolean);
    for (const r of roots) {
      cands.push(path.join(r, 'Google', 'Chrome', 'Application', 'chrome.exe'));
      cands.push(path.join(r, 'Microsoft', 'Edge', 'Application', 'msedge.exe'));
      cands.push(path.join(r, 'Chromium', 'Application', 'chrome.exe'));
    }
  }
  for (const c of cands) if (c && fs.existsSync(c)) return c;
  // PATH からも探す (Linux / 手で入れた環境)。which に頼らず PATH を自分で歩く。
  const names = process.platform === 'win32'
    ? ['chrome.exe', 'msedge.exe']
    : ['google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser', 'microsoft-edge', 'brave-browser'];
  for (const dir of (env.PATH || '').split(path.delimiter)) {
    if (!dir) continue;
    for (const n of names) {
      const p = path.join(dir, n);
      try { fs.accessSync(p, fs.constants.X_OK); return p; } catch (e) { /* 次 */ }
    }
  }
  return null;
}

// ─────────────────────────────────────────────────────────────────────
// 4. CDP クライアント (npm 依存ゼロ)
// ─────────────────────────────────────────────────────────────────────
// WebSocket を自前で話す。Node 21 以上なら組み込みの `WebSocket` があるが、
// **GitHub のランナーや配布 OS の Node は 18 / 20 のことがある**。そこで
// `[skip]` にすると「CI では永久に何も描かない」になるので、握手とフレームだけ
// 自分で書く (CDP に必要なのはテキストフレームと ping への返事だけ)。
class Ws {
  constructor(sock) {
    this.sock = sock;
    this.buf = Buffer.alloc(0);
    this.handlers = [];
    this.frag = [];
    this.fragOp = 0;
    sock.on('data', d => { this.buf = Buffer.concat([this.buf, d]); this.drain(); });
    sock.on('error', () => { /* 終了時に閉じるので握り潰す */ });
  }

  static connect(url) {
    return new Promise((res, rej) => {
      const u = new URL(url);
      const key = crypto.randomBytes(16).toString('base64');
      const sock = net.connect({ host: u.hostname, port: Number(u.port || 80) }, () => {
        sock.write('GET ' + u.pathname + u.search + ' HTTP/1.1\r\n'
          + 'Host: ' + u.host + '\r\n'
          + 'Upgrade: websocket\r\n'
          + 'Connection: Upgrade\r\n'
          + 'Sec-WebSocket-Key: ' + key + '\r\n'
          + 'Sec-WebSocket-Version: 13\r\n\r\n');
      });
      let head = Buffer.alloc(0);
      const onData = d => {
        head = Buffer.concat([head, d]);
        const i = head.indexOf('\r\n\r\n');
        if (i < 0) return;
        sock.removeListener('data', onData);
        const status = head.slice(0, i).toString('latin1').split('\r\n')[0];
        if (status.indexOf(' 101') < 0) {
          sock.destroy();
          rej(new Error('WebSocket の握手に失敗: ' + status));
          return;
        }
        const ws = new Ws(sock);
        const rest = head.slice(i + 4);
        res(ws);
        if (rest.length) { ws.buf = rest; ws.drain(); }
      };
      sock.on('data', onData);
      sock.on('error', e => rej(new Error('CDP へ繋げません (' + url + '): ' + e.message)));
    });
  }

  drain() {
    for (;;) {
      const b = this.buf;
      if (b.length < 2) return;
      const fin = (b[0] & 0x80) !== 0;
      const op = b[0] & 0x0f;
      const masked = (b[1] & 0x80) !== 0;
      let len = b[1] & 0x7f;
      let off = 2;
      if (len === 126) {
        if (b.length < 4) return;
        len = b.readUInt16BE(2); off = 4;
      } else if (len === 127) {
        if (b.length < 10) return;
        len = Number(b.readBigUInt64BE(2)); off = 10;
      }
      const maskAt = off;
      if (masked) off += 4;
      if (b.length < off + len) return;
      let pay = b.slice(off, off + len);
      if (masked) {
        const m = b.slice(maskAt, maskAt + 4);
        const un = Buffer.allocUnsafe(len);
        for (let i = 0; i < len; i++) un[i] = pay[i] ^ m[i % 4];
        pay = un;
      }
      this.buf = b.slice(off + len);
      if (op === 0x8) { this.close(); return; }
      if (op === 0x9) { this.frame(0xa, pay); continue; }   // ping には pong
      if (op === 0xa) continue;
      if (op === 0x0) this.frag.push(pay);
      else { this.frag = [pay]; this.fragOp = op; }
      if (fin) {
        const full = Buffer.concat(this.frag);
        this.frag = [];
        if (this.fragOp === 0x1) {
          const s = full.toString('utf8');
          for (const h of this.handlers) h(s);
        }
      }
    }
  }

  frame(op, payload) {
    const len = payload.length;
    let head;
    if (len < 126) { head = Buffer.alloc(2); head[1] = 0x80 | len; }
    else if (len < 65536) { head = Buffer.alloc(4); head[1] = 0x80 | 126; head.writeUInt16BE(len, 2); }
    else { head = Buffer.alloc(10); head[1] = 0x80 | 127; head.writeBigUInt64BE(BigInt(len), 2); }
    head[0] = 0x80 | op;
    // クライアント → サーバのフレームは必ずマスクする (RFC 6455)
    const mask = crypto.randomBytes(4);
    const out = Buffer.allocUnsafe(len);
    for (let i = 0; i < len; i++) out[i] = payload[i] ^ mask[i % 4];
    this.sock.write(Buffer.concat([head, mask, out]));
  }

  send(s) { this.frame(0x1, Buffer.from(s, 'utf8')); }
  onMessage(fn) { this.handlers.push(fn); }
  close() { try { this.sock.destroy(); } catch (e) { /* 既に閉じている */ } }
}

class Cdp {
  constructor(ws) {
    this.ws = ws; this.id = 0; this.pending = new Map(); this.handlers = [];
    ws.onMessage(txt => {
      let m;
      try { m = JSON.parse(txt); } catch (e) { return; }
      if (m.id !== undefined) {
        const w = this.pending.get(m.id);
        if (!w) return;
        this.pending.delete(m.id);
        if (m.error) w.rej(new Error(JSON.stringify(m.error)));
        else w.res(m.result);
      } else {
        for (const h of this.handlers) h(m);
      }
    });
  }
  static async connect(url) { return new Cdp(await Ws.connect(url)); }
  on(fn) { this.handlers.push(fn); }
  send(method, params, sessionId) {
    const id = ++this.id;
    const msg = { id, method, params: params || {} };
    if (sessionId) msg.sessionId = sessionId;
    this.ws.send(JSON.stringify(msg));
    return new Promise((res, rej) => this.pending.set(id, { res, rej }));
  }
  close() { this.ws.close(); }
}

async function launchBrowser(bin) {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'zv-remote-check-'));
  const args = [
    '--headless=new', '--disable-gpu', '--no-sandbox', '--no-first-run',
    '--no-default-browser-check', '--disable-extensions', '--disable-background-networking',
    '--disable-sync', '--disable-default-apps', '--mute-audio', '--hide-scrollbars',
    '--remote-debugging-port=0', '--user-data-dir=' + profile, 'about:blank',
  ];
  const proc = spawn(bin, args, { stdio: ['ignore', 'ignore', 'pipe'] });
  const wsUrl = await new Promise((res, rej) => {
    let buf = '';
    const to = setTimeout(() => rej(new Error('ブラウザが 20 秒で立ち上がりませんでした:\n' + buf)), 20000);
    proc.stderr.on('data', d => {
      buf += d.toString();
      const m = buf.match(/ws:\/\/\S+/);
      if (m) { clearTimeout(to); res(m[0]); }
    });
    proc.on('exit', c => { clearTimeout(to); rej(new Error('ブラウザが即終了しました (rc=' + c + ')\n' + buf)); });
  });
  const cdp = await Cdp.connect(wsUrl);
  return {
    cdp,
    async close() {
      cdp.close();
      try { proc.kill('SIGKILL'); } catch (e) { /* 既に死んでいる */ }
      try { fs.rmSync(profile, { recursive: true, force: true }); } catch (e) { /* 消せなくても続ける */ }
    },
  };
}

// ─────────────────────────────────────────────────────────────────────
// 5. 検査本体 — **ページの中で走る**。ここだけがブラウザの世界
// ─────────────────────────────────────────────────────────────────────
// この関数は `toString()` してページへ流し込む (文字列で書くと引用符地獄に
// なるので、普通の関数として書いて丸ごと送る)。外側の変数は参照できない。
function pageDriver(opt) {
  const V = [];            // 見つけた違反
  const notes = [];        // 検査できなかった理由 (黙って緑にしないため)
  let steps = 0;           // 確かめた状態の数
  let deep = 0;            // そのうち隅々まで見た数 (見た目が変わったときだけ)

  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const cs = el => getComputedStyle(el);
  // **祖先まで見た「見えている」**を使う。自分の display だけを見ると、
  // display:none の入れ物の中にいるボタンを「0x0 の押せないボタン」として
  // 大量に誤検出する (実際に 40 件出た)。
  const vis = el => (el.checkVisibility
    ? el.checkVisibility({ checkVisibilityCSS: true })
    : (cs(el).display !== 'none' && cs(el).visibility !== 'hidden'
       && (el.offsetWidth > 0 || el.offsetHeight > 0)));
  const nm = el => el.id ? '#' + el.id
    : el.tagName.toLowerCase() + (el.className ? '.' + String(el.className).trim().split(/\s+/).join('.') : '');
  const bad = (label, kind, msg) => { if (V.length < 40) V.push({ label, kind, msg }); };
  const views = () => Array.prototype.slice.call(document.querySelectorAll('main > .view'));

  // **主枠** = そのビューの残りを埋める子。`flex:1` で見分けるのが本筋だが、
  // 大きさで居座っているものも拾う (flex を使わない実装が後から来ても効く)。
  function fillers(view) {
    const vr = view.getBoundingClientRect();
    return Array.prototype.slice.call(view.children).filter(el => {
      if (!vis(el)) return false;
      if (parseFloat(cs(el).flexGrow) >= 1) return true;
      const r = el.getBoundingClientRect();
      return vr.height > 0 && r.height >= vr.height * 0.35;
    });
  }

  // 状態の指紋。**同じ見た目を何百回も深掘りしない**ための鍵。
  // 見えているビュー / 主枠 / 見えている要素の数が変われば別の状態として深く見る。
  const deepSeen = new Set();
  function fingerprint(shown) {
    const v = shown[0];
    if (!v) return 'none';
    return v.id + '|' + fillers(v).map(nm).join(',') + '|'
      + v.querySelectorAll('button').length + '|' + v.querySelectorAll('*').length;
  }

  function check(label) {
    steps++;
    window.__zv = { steps: steps, label: label };
    const shown = views().filter(vis);
    if (shown.length !== 1) {
      bad(label, 'view', '見えている .view が ' + shown.length + ' 個: ' + shown.map(nm).join(', '));
    }
    for (let vi = 0; vi < shown.length; vi++) {
      const v = shown[vi];
      const f = fillers(v);
      if (f.length > 1) {
        bad(label, 'slot', nm(v) + ' の主枠が同時に ' + f.length + ' 個見えている: ' + f.map(nm).join(', '));
      }
      const kids = Array.prototype.slice.call(v.children).filter(vis)
        .map(el => [el, el.getBoundingClientRect()]);
      for (let i = 0; i < kids.length; i++) {
        for (let j = i + 1; j < kids.length; j++) {
          const a = kids[i][1], b = kids[j][1];
          const w = Math.min(a.right, b.right) - Math.max(a.left, b.left);
          const h = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
          if (w > 1 && h > 1) {
            bad(label, 'overlap', nm(kids[i][0]) + ' と ' + nm(kids[j][0])
              + ' が重なっている (' + Math.round(w) + '×' + Math.round(h) + 'px)');
          }
        }
      }
    }
    const fp = fingerprint(shown);
    if (deepSeen.has(fp)) return;   // 同じ見た目は 1 回で足りる
    deepSeen.add(fp);
    deep++;
    const scope = [document.querySelector('header'), document.getElementById('nav')]
      .concat(shown).filter(Boolean);
    for (let si = 0; si < scope.length; si++) {
      const all = [scope[si]].concat(Array.prototype.slice.call(scope[si].querySelectorAll('*')));
      for (let k = 0; k < all.length; k++) {
        const el = all[k];
        if (!vis(el)) continue;
        const s = cs(el);
        // 横スクロールを持つ入れ物 (`.chips` 等) は「はみ出して当然」なので除く。
        // CSS は overflow-y だけ指定しても overflow-x が auto に計算されるため、
        // ここで拾うのは**本当に visible なもの**だけになる。
        if ((s.display === 'block' || s.display === 'flex' || s.display === 'grid')
          && s.overflowX === 'visible' && el.clientWidth > 0
          && el.scrollWidth > el.clientWidth + 1) {
          bad(label, 'overflow', nm(el) + ' が横にはみ出している ('
            + el.scrollWidth + ' > ' + el.clientWidth + 'px)');
        }
        if (el.tagName === 'BUTTON') {
          const r = el.getBoundingClientRect();
          if (r.width < opt.tapMin || r.height < opt.tapMin) {
            bad(label, 'tap', nm(el) + ' のボタンが押せない大きさ ('
              + Math.round(r.width) + '×' + Math.round(r.height) + 'px)');
          }
        }
      }
    }
  }

  const navBtns = () => Array.prototype.slice.call(document.querySelectorAll('#nav button'));
  const activeView = () => views().filter(vis)[0] || null;
  const segBtns = () => {
    const v = activeView();
    return v ? Array.prototype.slice.call(v.querySelectorAll(':scope > .seg > button')) : [];
  };
  async function tap(el, label) {
    if (!el) return;
    el.click();
    await sleep(opt.settle);
    check(label);
  }

  const t0 = Date.now();
  return (async function () {
    check('起動直後');

    // ① 下部ナビ: 順序つきの全ての 2 手
    for (let i = 0; i < navBtns().length; i++) {
      for (let j = 0; j < navBtns().length; j++) {
        if (i === j) continue;
        await tap(navBtns()[i], 'ナビ ' + i);
        await tap(navBtns()[j], 'ナビ ' + i + '→' + j);
      }
    }

    // ② 各ビューのセグメント: 順序つきの全ての 2 手と 3 手
    //    (今回のバグは「一覧 → 端末」の 2 手目で出た。3 手まで見ると
    //     「一覧 → 承認 → 端末」のような、誰も試していない順番が出る)
    for (let n = 0; n < navBtns().length; n++) {
      await tap(navBtns()[n], 'ビュー ' + n);
      const m = segBtns().length;
      for (let i = 0; i < m; i++) {
        for (let j = 0; j < m; j++) {
          if (i === j) continue;
          await tap(segBtns()[i], '面 ' + n + ':' + i);
          await tap(segBtns()[j], '面 ' + n + ':' + i + '→' + j);
        }
      }
      for (let i = 0; i < m; i++) {
        for (let j = 0; j < m; j++) {
          for (let k = 0; k < m; k++) {
            if (i === j || j === k) continue;
            await tap(segBtns()[i], '面3 ' + n);
            await tap(segBtns()[j], '面3 ' + n);
            await tap(segBtns()[k], '面 ' + n + ':' + i + '→' + j + '→' + k);
          }
        }
      }
    }

    // ③ 面を選んだままタブを出て、戻ってくる (2 重表示が出るならここ)
    for (let n = 0; n < navBtns().length; n++) {
      await tap(navBtns()[n], '往復 ' + n);
      const m = segBtns().length;
      for (let i = 0; i < m; i++) {
        await tap(segBtns()[i], '往復 ' + n + ':' + i);
        const other = navBtns()[(n + 1) % navBtns().length];
        await tap(other, '往復 出る');
        await tap(navBtns()[n], '往復 ' + n + ':' + i + ' 戻り');
      }
    }

    // ④ 乱歩 — **種を固定して決定的にする**。失敗したら同じ種で必ず再現する
    let seed = opt.seed >>> 0;
    const rnd = () => { seed = (seed * 1664525 + 1013904223) >>> 0; return seed / 4294967296; };
    for (let s = 0; s < opt.walk; s++) {
      const pool = navBtns().concat(segBtns())
        .concat(Array.prototype.slice.call(document.querySelectorAll('#achips button')))
        .concat(Array.prototype.slice.call(document.querySelectorAll('#alist .lane .lhd')))
        .concat(Array.prototype.slice.call(document.querySelectorAll('#alist .card')))
        .filter(vis);
      if (!pool.length) break;
      await tap(pool[Math.floor(rnd() * pool.length)], '乱歩 ' + s + ' (seed=' + opt.seed + ')');
    }

    // ⑤ チップを押したら、端末の中身がそのエージェントへ変わる
    //    「追従を止めたまま離れると、押しても永久に変わらない」の番人。
    await (async function chipSwitch() {
      const nav = document.querySelector('#nav button[data-v="agent"]');
      if (!nav) { notes.push('エージェントタブが無いのでチップ切替は見ていない'); return; }
      nav.click(); await sleep(opt.settle);
      const segs = segBtns();
      if (segs.length) { segs[0].click(); await sleep(opt.settle); }
      const scr = document.getElementById('scr');
      const chips = Array.prototype.slice.call(document.querySelectorAll('#achips button'))
        .filter(b => !b.classList.contains('mic') && /agent-\d+/.test(b.textContent));
      if (!scr || chips.length < 2) {
        notes.push('チップが ' + chips.length + ' 個しか無いので切替は見ていない');
        return;
      }
      for (let i = 0; i < 60 && !/line \d/.test(scr.textContent); i++) await sleep(50);
      if (!/line \d/.test(scr.textContent)) { notes.push('端末に中身が出ないのでチップ切替は見ていない'); return; }
      // 遡って追従を止める。これをやらないと**バグのある版でも緑になる**
      scr.scrollTop = 0;
      scr.dispatchEvent(new Event('scroll'));
      await sleep(150);
      const target = chips[chips.length - 1];
      const want = (target.textContent.match(/agent-\d+/) || [])[0];
      target.click();
      let ok = false;
      for (let i = 0; i < 80; i++) {
        await sleep(50);
        if (scr.textContent.indexOf('[' + want + ']') >= 0) { ok = true; break; }
      }
      if (!ok) {
        bad('チップ切替', 'stuck',
          want + ' のチップを押しても端末の中身が変わらない (追従が止まったまま固まる)');
      }
      check('チップ切替の後');
    })();

    return { violations: V, notes, steps, deep, seed: opt.seed, ms: Date.now() - t0 };
  })();
}

// ─────────────────────────────────────────────────────────────────────
// 6. わざと壊す — **空振りする検査を残さない**ための自己検査
// ─────────────────────────────────────────────────────────────────────
// 検査は「赤にできること」を示して初めて意味を持つ。ここに並ぶ欠陥は
// 全部「実際に起きた / 起きうる」形で、`--self-test` が**全部捕まること**を
// 毎回確かめる。捕まらなくなったら、その日から検査は飾りになっている。
const INJECTIONS = {
  'history-css': {
    desc: '0.18 で実際に出た: #alist.mid が #alist{display:none} を打ち消す (端末と一覧が同時に見える)',
    expect: 'slot', agents: 0,
    build: {
      replace: [
        ['#alist.show.mid {', '#alist.mid {'],
        ["el.classList.remove('mid');\n    el.classList.remove('show');", "el.classList.remove('show');"],
      ],
    },
  },
  'two-views': {
    desc: '下部ナビを押すと .view が 2 つ act になる',
    expect: 'view', agents: 3,
    build: { appendJs: "\ndocument.getElementById('nav').addEventListener('click',function(){document.getElementById('v-cmds').classList.add('act');});\n" },
  },
  'overflow': {
    desc: '幅の広い要素が入り込んで横に見切れる',
    expect: 'overflow', agents: 3,
    build: { appendJs: "\n(function(){var d=document.createElement('div');d.style.width='4000px';d.style.height='4px';d.textContent='x';document.getElementById('v-agent').appendChild(d);})();\n" },
  },
  'dead-button': {
    desc: '見えているのに指で押せない大きさのボタンが残る',
    expect: 'tap', agents: 3,
    build: { appendJs: "\n(function(){var b=document.querySelector('#nav button');b.style.height='0px';b.style.padding='0px';b.style.overflow='hidden';})();\n" },
  },
  'no-poll': {
    desc: 'エージェント画面を開いてもポーリングが 1 本も飛ばない',
    expect: 'poll', agents: 3,
    build: { appendJs: '\npollTerm = function () {};\n' },
  },
  'chip-dead': {
    desc: 'チップを押しても宛先が切り替わらない',
    expect: 'stuck', agents: 3,
    build: { appendJs: '\nselectAgent = function () {};\n' },
  },
  'js-error': {
    desc: '操作すると JS 例外が飛ぶ (画面はそれらしく見えたまま)',
    expect: 'console', agents: 3,
    build: { appendJs: "\ndocument.getElementById('nav').addEventListener('click',function(){null.x;},true);\n" },
  },
};

// ─────────────────────────────────────────────────────────────────────
// 7. 1 回ぶんの検査を走らせる
// ─────────────────────────────────────────────────────────────────────
const POLL_WINDOW_MS = 4000;   // ポーリング間隔 1500ms の 2 周ぶん + 余裕
const POLL_MIN = 2;            // この窓で最低これだけは飛んでいるはず

async function evalJs(cdp, sid, expr, awaitPromise) {
  const r = await cdp.send('Runtime.evaluate', {
    expression: expr, awaitPromise: !!awaitPromise, returnByValue: true,
  }, sid);
  if (r.exceptionDetails) {
    const e = r.exceptionDetails;
    throw new Error('ページで例外: ' + ((e.exception && e.exception.description) || e.text));
  }
  return r.result.value;
}

const wait = ms => new Promise(r => setTimeout(r, ms));

async function runOne(br, spec) {
  const page = buildPage({
    lang: spec.lang,
    appendJs: (spec.build && spec.build.appendJs) || '',
    replace: (spec.build && spec.build.replace) || [],
  });
  const model = makeModel(spec.agents);
  const srv = await startServer(page, model, { scrollback: spec.scrollback !== false });
  const errors = [];
  let sid = null;
  let targetId = null;
  try {
    const t = await br.cdp.send('Target.createTarget', { url: 'about:blank' });
    targetId = t.targetId;
    sid = (await br.cdp.send('Target.attachToTarget', { targetId, flatten: true })).sessionId;

    let dialogs = 0;
    br.cdp.on(m => {
      if (m.sessionId !== sid) return;
      // **`prompt()` / `confirm()` はページの主スレッドを止める。**
      // CDP を繋いだまま放置すると誰も答えないので、検査は永久に固まる
      // (実際に「＋ 起動」チップで固まった)。必ず**打ち消す側**で答える —
      // 承認を勝手に押す検査は、それ自体が事故になる。
      if (m.method === 'Page.javascriptDialogOpening') {
        dialogs++;
        br.cdp.send('Page.handleJavaScriptDialog', { accept: false }, sid).catch(() => {});
        return;
      }
      if (m.method === 'Runtime.exceptionThrown') {
        const e = m.params.exceptionDetails;
        errors.push('例外: ' + ((e.exception && e.exception.description) || e.text));
      } else if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') {
        errors.push('console.error: ' + m.params.args.map(a => a.description || a.value).join(' '));
      } else if (m.method === 'Log.entryAdded' && m.params.entry.level === 'error'
        && m.params.entry.source !== 'network') {
        // 通信の失敗 (`/api/scrollback` が無い版など) は本番でも起きる想定なので
        // ここでは数えない。飛んでいないことは (7) のポーリング検査が見る。
        errors.push('log: ' + m.params.entry.text);
      }
    });

    await br.cdp.send('Runtime.enable', {}, sid);
    await br.cdp.send('Log.enable', {}, sid);
    await br.cdp.send('Page.enable', {}, sid);
    await br.cdp.send('Emulation.setDeviceMetricsOverride', {
      width: spec.width, height: spec.height, deviceScaleFactor: 2, mobile: true,
    }, sid);

    const loaded = new Promise(res => {
      br.cdp.on(m => { if (m.sessionId === sid && m.method === 'Page.loadEventFired') res(); });
    });
    await br.cdp.send('Page.navigate', { url: srv.url + '?t=zv-test' }, sid);
    await Promise.race([loaded, wait(15000)]);

    // 起動直後の /api/state が返るまで待つ (返る前に叩くと空の画面を検査する)
    for (let i = 0; i < 60; i++) {
      if (await evalJs(br.cdp, sid, 'typeof state !== "undefined" && state !== null')) break;
      await wait(100);
    }

    // (7) ポーリングが**実際に飛んでいる**か - 判定はページの中ではなく
    //     サーバ側の受信数で行う (ページの中の変数はいくらでも嘘をつける)。
    const pollV = [];
    await evalJs(br.cdp, sid,
      '(function(){var n=document.querySelector(\'#nav button[data-v="agent"]\');if(n)n.click();'
      + 'var v=document.getElementById("v-agent");var b=v&&v.querySelector(":scope > .seg > button");'
      + 'if(b)b.click();})()');
    let before = (srv.hits['/api/term'] || 0) + (srv.hits['/api/scrollback'] || 0);
    await wait(POLL_WINDOW_MS);
    let got = (srv.hits['/api/term'] || 0) + (srv.hits['/api/scrollback'] || 0) - before;
    if (got < POLL_MIN) {
      pollV.push({
        label: 'ポーリング', kind: 'poll',
        msg: '端末を開いているのに ' + POLL_WINDOW_MS + 'ms で ' + got
          + ' 回しか取りに行っていない (最低 ' + POLL_MIN + ' 回)',
      });
    }
    // 一覧の面でも同じ (面ごとに 1 本ずつ生きていること)
    const segs = await evalJs(br.cdp, sid,
      '(function(){var v=document.getElementById("v-agent");'
      + 'var b=v?v.querySelectorAll(":scope > .seg > button"):[];if(b[1])b[1].click();return b.length;})()');
    if (segs >= 2) {
      before = srv.hits['/api/agents'] || 0;
      await wait(POLL_WINDOW_MS);
      got = (srv.hits['/api/agents'] || 0) - before;
      if (got < POLL_MIN) {
        pollV.push({
          label: 'ポーリング', kind: 'poll',
          msg: '一覧を開いているのに ' + POLL_WINDOW_MS + 'ms で ' + got + ' 回しか取りに行っていない',
        });
      }
    }

    // 総当たり + 乱歩
    const driverOpt = { settle: spec.settle, walk: spec.walk, seed: spec.seed, tapMin: spec.tapMin };
    const src = '(' + pageDriver.toString() + ')(' + JSON.stringify(driverOpt) + ')';
    // **固まったら黙って待たない。** 進み具合をページ側の window.__zv で見て、
    // 一定時間 1 状態も進まなければ「どこで止まったか」を添えて中止する。
    let done = false, out = null, err = null;
    evalJs(br.cdp, sid, src, true).then(v => { out = v; done = true; }, e => { err = e; done = true; });
    let last = -1, still = 0;
    const t0 = Date.now();
    while (!done) {
      await wait(500);
      let cur = null;
      try { cur = await evalJs(br.cdp, sid, 'window.__zv ? window.__zv.steps + "|" + window.__zv.label : "?"'); }
      catch (e) { cur = '?'; }
      const n = parseInt(String(cur).split('|')[0], 10) || 0;
      if (n !== last) { last = n; still = 0; } else { still += 500; }
      if (still >= spec.stallMs) {
        throw new Error('検査が ' + spec.stallMs + 'ms のあいだ 1 状態も進みませんでした (最後: ' + cur + ')');
      }
      if (Date.now() - t0 > spec.timeout) {
        throw new Error('検査が ' + spec.timeout + 'ms で終わりませんでした (最後: ' + cur + ')');
      }
    }
    if (err) throw err;
    const res = out;

    const violations = pollV.concat(res.violations)
      .concat(errors.map(e => ({ label: '通しで', kind: 'console', msg: e })));
    const notes = res.notes.slice();
    if (dialogs) notes.push('ページの問い合わせ (confirm / prompt) を ' + dialogs + ' 回打ち消した');
    return { violations, notes, steps: res.steps, deep: res.deep, ms: res.ms, page };
  } finally {
    if (targetId) {
      try { await br.cdp.send('Target.closeTarget', { targetId }); } catch (e) { /* もう閉じている */ }
    }
    await srv.close();
  }
}

// ─────────────────────────────────────────────────────────────────────
// 8. 入口
// ─────────────────────────────────────────────────────────────────────
function parseArgs(argv) {
  const o = {
    selfTest: false, inject: null, list: false, keep: false, help: false,
    settle: 20, walk: 120, seed: 20260817, tapMin: 24, timeout: 240000, stallMs: 25000, lang: null,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--self-test') o.selfTest = true;
    else if (a === '--list') o.list = true;
    else if (a === '--keep') o.keep = true;
    else if (a === '--inject') o.inject = argv[++i];
    else if (a === '--seed') o.seed = parseInt(argv[++i], 10) || o.seed;
    else if (a === '--walk') o.walk = parseInt(argv[++i], 10);
    else if (a === '--lang') o.lang = argv[++i];
    else if (a === '-h' || a === '--help') o.help = true;
    else { console.error('知らない引数: ' + a); process.exit(64); }
  }
  return o;
}

// 通常の検査で回す組み合わせ。**空状態と多言語を必ず含める** -
// 中身が詰まった日本語だけを見ていると、いちばん壊れやすい場所を見ない。
function baseSpecs(o) {
  const base = { settle: o.settle, walk: o.walk, seed: o.seed, tapMin: o.tapMin, timeout: o.timeout, stallMs: o.stallMs };
  const spec = (name, extra) => Object.assign({ name: name }, base, extra);
  if (o.lang) {
    return [spec('3 体 / 390x844 / ' + o.lang, { agents: 3, width: 390, height: 844, lang: o.lang })];
  }
  return [
    spec('3 体 / 320x568 / ja (いちばん狭い)', { agents: 3, width: 320, height: 568 }),
    spec('0 体 / 390x844 / ja (空状態)', { agents: 0, width: 390, height: 844 }),
    spec('3 体 / 390x844 / en (訳が長い)', { agents: 3, width: 390, height: 844, lang: 'en' }),
    spec('3 体 / 430x932 / scrollback 無しの旧 PC', { agents: 3, width: 430, height: 932, scrollback: false }),
  ];
}

function printViolations(v) {
  const seen = new Set();
  for (const x of v) {
    const key = x.kind + ' ' + x.msg;
    if (seen.has(key)) continue;
    seen.add(key);
    console.log('    [' + x.kind + '] ' + x.msg + '  <- ' + x.label);
  }
}

async function main() {
  const o = parseArgs(process.argv.slice(2));
  if (o.help) {
    console.log('使い方: node tools/remote-check.js [--self-test|--inject <名前>|--list]'
      + ' [--seed N] [--walk N] [--lang ja] [--keep]');
    return 0;
  }
  if (o.list) {
    for (const k of Object.keys(INJECTIONS)) console.log(k.padEnd(14) + ' ' + INJECTIONS[k].desc);
    return 0;
  }
  const bin = findBrowser();
  if (!bin) {
    console.log('[skip] Chromium 系のブラウザが見つかりません '
      + '(Chrome / Chromium / Edge / Brave のどれかを入れるか、ZV_BROWSER に実行ファイルを指定)');
    return 2;
  }
  console.log('ブラウザ: ' + bin);
  const br = await launchBrowser(bin);
  let rc = 0;
  try {
    if (o.selfTest) {
      const common = { settle: o.settle, walk: 40, seed: o.seed, tapMin: o.tapMin, timeout: o.timeout, stallMs: o.stallMs };
      // (1) まず素の状態が緑であること (赤しか出せない検査は検査ではない)
      const clean = await runOne(br, Object.assign(
        { name: '素の状態', agents: 3, width: 390, height: 844 }, common));
      if (clean.violations.length) {
        console.log('✗ 素の状態で違反が出た (自己検査の前に本体を直すこと)');
        printViolations(clean.violations);
        rc = 1;
      } else {
        console.log('✓ 素の状態 — 違反なし (' + clean.steps + ' 状態)');
      }
      // (2) 仕込んだ欠陥が **1 つ残らず** 捕まること
      for (const key of Object.keys(INJECTIONS)) {
        const inj = INJECTIONS[key];
        let r;
        try {
          r = await runOne(br, Object.assign(
            { name: key, agents: inj.agents, width: 390, height: 844, build: inj.build }, common));
        } catch (e) {
          console.log('✗ ' + key + ' — 仕込めなかった: ' + e.message);
          rc = 1;
          continue;
        }
        const kinds = new Set(r.violations.map(x => x.kind));
        if (kinds.has(inj.expect)) {
          console.log('✓ ' + key + ' — [' + inj.expect + '] が捕まえた: ' + inj.desc);
        } else {
          console.log('✗ ' + key + ' — 捕まえられなかった (期待 [' + inj.expect + ']、出たのは ['
            + Array.from(kinds).join(',') + ']): ' + inj.desc);
          printViolations(r.violations);
          rc = 1;
        }
      }
      return rc;
    }

    let specs;
    if (o.inject) {
      const inj = INJECTIONS[o.inject];
      if (!inj) { console.error('知らない仕込み: ' + o.inject + ' (--list で一覧)'); return 64; }
      specs = [{
        name: '仕込み: ' + o.inject, agents: inj.agents, width: 390, height: 844, build: inj.build,
        settle: o.settle, walk: o.walk, seed: o.seed, tapMin: o.tapMin, timeout: o.timeout, stallMs: o.stallMs,
      }];
    } else {
      specs = baseSpecs(o);
    }

    for (const s of specs) {
      const r = await runOne(br, s);
      for (const n of r.notes) console.log('  ... ' + n);
      if (r.violations.length) {
        console.log('✗ ' + s.name + ' — 違反 ' + r.violations.length + ' 件 (' + r.steps + ' 状態 / うち隅々まで ' + r.deep + ' / ' + r.ms + 'ms)');
        printViolations(r.violations);
        if (o.keep) {
          const f = path.join(os.tmpdir(), 'zv-remote-' + Date.now() + '.html');
          fs.writeFileSync(f, r.page);
          console.log('    再現用のページ: ' + f);
        }
        rc = 1;
      } else {
        console.log('✓ ' + s.name + ' — 違反なし (' + r.steps + ' 状態 / うち隅々まで ' + r.deep + ' / ' + r.ms + 'ms)');
      }
    }
  } finally {
    await br.close();
  }
  return rc;
}

main().then(c => process.exit(c)).catch(e => {
  console.error('検査そのものが失敗しました: ' + (e && e.stack ? e.stack : e));
  process.exit(1);
});
