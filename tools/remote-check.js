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

const NL = String.fromCharCode(10);

const ROOT = path.resolve(__dirname, '..');
const REMOTE = path.join(ROOT, 'assets', 'remote');

// ─────────────────────────────────────────────────────────────────────
// 0. 時限と進捗 — **黙って待ち続けない**
// ─────────────────────────────────────────────────────────────────────
// CI (ubuntu) で 15 分間**標準出力に 1 バイトも出ないまま**ジョブが打ち切られた。
// 原因は「返らない待ち」がどれも予算を持っていなかったこと:
//
//   * CDP の 1 往復に時限が無い    → ブラウザが固まると永久に待つ
//   * WebSocket の握手に時限が無い → 繋がらないまま永久に待つ
//   * 偽サーバの close()           → keep-alive を握られたままだと**返らない**
//   * 全体の時限が無い             → 上のどれかに落ちると誰も打ち切らない
//
// 時限は**入れ子**にする (1 往復 < 1 つの検査 < 全体)。予算は環境変数で
// 変えられる — CI と手元で妥当な値が違うのに、直書きすると片方で必ず嘘になる。
function envMs(name, dflt) {
  const v = parseInt(process.env[name] || '', 10);
  return Number.isFinite(v) && v > 0 ? v : dflt;
}
const BUDGET = {
  total: envMs('ZV_DEADLINE_MS', 12 * 60 * 1000), // 全体の上限 (ジョブの timeout より必ず短く)
  stall: envMs('ZV_STALL_MS', 120 * 1000),        // **進捗が止まって**からの猶予
  launch: envMs('ZV_LAUNCH_MS', 45 * 1000),       // ブラウザが CDP の口を開くまで
  ws: envMs('ZV_WS_MS', 15 * 1000),               // WebSocket の握手
  cdp: envMs('ZV_CDP_MS', 30 * 1000),             // CDP の 1 往復
  load: envMs('ZV_LOAD_MS', 20 * 1000),           // ページの読み込み
  close: envMs('ZV_CLOSE_MS', 5 * 1000),          // 後始末 (1 つあたり)
  beat: envMs('ZV_HEARTBEAT_MS', 20 * 1000),      // 生存の合図 (0 で止める)
};

const T0 = Date.now();
const elapsed = () => ((Date.now() - T0) / 1000).toFixed(1) + 's';
// いま何をしているか。時限が撃ったときに**どこで止まったか**を言うために持つ。
let PHASE = '起動前';
let LAST_PROGRESS = Date.now();
// **固定の予算だけでは必ず破綻する** (CLAUDE.md: 進捗が観測できる限り待ちを延ばす)。
// 遅いランナーで正しく働いているだけの実行を殺さないため、時限は 2 段にする:
//   touch()  … 進んだ。止まってからの猶予 (BUDGET.stall) を数え直す
//   絶対上限 … それでも終わらない生きたループ用 (BUDGET.total)
function touch() { LAST_PROGRESS = Date.now(); }
function step(msg) {
  PHASE = msg;
  touch();
  process.stdout.write('  [' + elapsed() + '] ' + msg + '\n');
}

// **process.exit() の直前は同期で書く。** stdout がパイプ (CI の tee や
// `$(...)`) のとき process.stdout.write は非同期で、exit がバッファを
// 捨てる。黙って消えると、いちばん要る 1 行だけが失われる。
function sayNow(msg) {
  try { fs.writeSync(1, msg); } catch (e) {
    try { process.stdout.write(msg); } catch (x) { /* 書けないなら諦める */ }
  }
}

// 時限が撃ったときの言い分。**理由を 1 行書いて赤で終わる** — 黙って消えない。
function fireDeadline(why) {
  sayNow(NL + '\u001b[1;31m✗ 時限切れ — ' + why + '\u001b[0m' + NL
    + '  最後の段: ' + PHASE + ' (経過 ' + elapsed() + ')' + NL
    + '  予算は ZV_DEADLINE_MS / ZV_STALL_MS / ZV_CDP_MS で変えられます' + NL);
  runCleanup();
  process.exit(3);
}

// 全体の見張り。**unref しない** — これが生きている限り event loop は
// 回るので、他の待ちを unref しても「誰も居なくなって黙って終了」が起きない。
function armWatchdog() {
  const tick = setInterval(() => {
    if (Date.now() - T0 > BUDGET.total) {
      fireDeadline('全体で ' + Math.round(BUDGET.total / 1000) + ' 秒を超えました');
    }
    if (Date.now() - LAST_PROGRESS > BUDGET.stall) {
      fireDeadline(Math.round(BUDGET.stall / 1000) + ' 秒のあいだ 1 つも進みませんでした');
    }
  }, 1000);
  return tick;
}

// 生きている合図。**これが無いと CI のログは 15 分間まっさらになる** —
// 実際にそうなって「何が起きたか分からない」まま打ち切られた。
function armHeartbeat() {
  if (BUDGET.beat <= 0) return null;
  const t = setInterval(() => {
    const idle = ((Date.now() - LAST_PROGRESS) / 1000).toFixed(1);
    process.stdout.write('  … ' + elapsed() + ' 経過 / ' + PHASE + ' — 最後の進捗から ' + idle + 's\n');
  }, BUDGET.beat);
  if (t.unref) t.unref();
  return t;
}

// プロセスをツリーごと畳む。**直接の子だけ殺すと孫が残る** —
// CI のログの最後に chrome / chrome_crashpad_handler が並んでいたのがそれ。
function killTree(proc) {
  if (!proc || !proc.pid) return;
  const pid = proc.pid;
  if (process.platform === 'win32') {
    try { spawn('taskkill', ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' }); }
    catch (e) { /* もう居ない */ }
    try { proc.kill(); } catch (e) { /* もう居ない */ }
    return;
  }
  // detached で起こしてあるので子はプロセスグループの長。負の PID で
  // グループごと落とす。**pid <= 1 では絶対に撃たない** (kill(-1) は
  // 自分の全プロセスを巻き込む)。
  if (pid > 1) {
    try { process.kill(-pid, 'SIGKILL'); } catch (e) { /* もう居ない */ }
  }
  try { proc.kill('SIGKILL'); } catch (e) { /* もう居ない */ }
}

// 後始末は投げっぱなしにしない。時限や signal で落ちるときも必ず通る。
const CLEANUP = [];
function runCleanup() {
  while (CLEANUP.length) {
    const f = CLEANUP.pop();
    try { f(); } catch (e) { /* 後始末は失敗しても次へ進む */ }
  }
}

// 1 つの待ちに予算を付ける。**超えたら理由を持った Error で返る** (黙らない)。
function withTimeout(p, ms, what) {
  let t = null;
  const guard = new Promise((_res, rej) => {
    t = setTimeout(() => rej(new Error(what + ' が ' + ms + 'ms で返りませんでした')), ms);
    if (t.unref) t.unref();
  });
  return Promise.race([p, guard]).then(
    v => { clearTimeout(t); return v; },
    e => { clearTimeout(t); throw e; },
  );
}

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
    // わざと返さない経路 (--hang server 専用)。**処理中の要求**を 1 本
    // 抱えたままにすると、Node の server.close() はコールバックを呼ばない。
    // 「後始末が返らない」を本物で再現するための仕込み。
    if (p === '/zv-never') { return; }
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
  // **握られたままの接続を数える。** Node の server.close() は
  // 生きている接続が 1 本でもあると**コールバックを呼ばない**。ブラウザは
  // keep-alive を張るので、閉じ損ねると後始末で永久に止まる (無音のまま
  // ジョブが打ち切られた形はこれで説明が付く)。
  const socks = new Set();
  srv.on('connection', sk => {
    socks.add(sk);
    sk.on('close', () => socks.delete(sk));
  });
  const cutAll = () => {
    for (const sk of socks) { try { sk.destroy(); } catch (e) { /* もう閉じている */ } }
    socks.clear();
  };
  return new Promise(resolve => {
    // ポートは 0 (空きを OS に選ばせる)。番号を直書きすると同時実行で衝突する。
    srv.listen(0, '127.0.0.1', () => {
      resolve({
        url: 'http://127.0.0.1:' + srv.address().port + '/',
        hits,
        close: () => new Promise(resolve2 => {
          let done = false;
          const fin = note => { if (done) return; done = true; resolve2(note || null); };
          const late = setTimeout(() => {
            const n = socks.size;
            cutAll();
            fin('後始末: 握られたままの接続 ' + n + ' 本を切って進みました');
          }, BUDGET.close);
          if (late.unref) late.unref();
          srv.close(() => { clearTimeout(late); fin(); });
          // **時限が空回りしていないこと**を確かめるための仕込み
          // (--hang server)。こちらから切らず、後ろの砦だけに任せる。
          if (process.env.ZV_HOLD_SOCKETS === '1') return;
          // 待たずにこちらから切る (Node 18.2+ なら本体の API を使う)。
          if (typeof srv.closeAllConnections === 'function') srv.closeAllConnections();
          else cutAll();
        }),
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
    this.closeHandlers = [];
    sock.on('data', d => { this.buf = Buffer.concat([this.buf, d]); this.drain(); });
    sock.on('error', () => { /* 終了時に閉じるので握り潰す */ });
    // **閉じたことを黙って飲まない。** 待っている往復を予算いっぱい (30 秒)
    // 待たせる代わりに、その場で「ブラウザが落ちた」と言う。
    sock.on('close', () => {
      for (const h of this.closeHandlers) { try { h(); } catch (e) { /* 続ける */ } }
    });
  }

  static connect(url) {
    // **握手にも時限が要る。** 繋がらない / 101 が返らないまま待ち続けると、
    // ここだけで CI のジョブ時間を使い切る (実際にそう見える形で止まった)。
    return new Promise((res, rej) => {
      const u = new URL(url);
      const key = crypto.randomBytes(16).toString('base64');
      let settled = false;
      let sock = null;
      const to = setTimeout(() => {
        if (settled) return;
        settled = true;
        try { if (sock) sock.destroy(); } catch (e) { /* もう閉じている */ }
        rej(new Error('CDP の WebSocket 握手が ' + BUDGET.ws + 'ms で終わりませんでした (' + url + ')'));
      }, BUDGET.ws);
      if (to.unref) to.unref();
      const ok = v => { if (settled) return; settled = true; clearTimeout(to); res(v); };
      const ng = e => {
        if (settled) return;
        settled = true;
        clearTimeout(to);
        try { if (sock) sock.destroy(); } catch (x) { /* もう閉じている */ }
        rej(e);
      };
      sock = net.connect({ host: u.hostname, port: Number(u.port || 80) }, () => {
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
          ng(new Error('WebSocket の握手に失敗: ' + status));
          return;
        }
        const ws = new Ws(sock);
        const rest = head.slice(i + 4);
        ok(ws);
        if (rest.length) { ws.buf = rest; ws.drain(); }
      };
      sock.on('data', onData);
      sock.on('error', e => ng(new Error('CDP へ繋げません (' + url + '): ' + e.message)));
      sock.on('close', () => ng(new Error('CDP の接続が握手の前に閉じました (' + url + ')')));
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
  onClose(fn) { this.closeHandlers.push(fn); }
  close() { try { this.sock.destroy(); } catch (e) { /* 既に閉じている */ } }
}

class Cdp {
  constructor(ws) {
    this.ws = ws; this.id = 0; this.pending = new Map(); this.handlers = [];
    this.dead = null;
    // ブラウザが落ちたら、待っている往復を**その場で**失敗させる。
    ws.onClose(() => {
      this.dead = 'ブラウザとの接続が閉じました';
      for (const [, w] of this.pending) w.rej(new Error(this.dead));
      this.pending.clear();
    });
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
  // **1 往復ごとに予算を持たせる。** ここが無いと、ブラウザが固まった瞬間に
  // 検査は「待っているだけ」になり、外からは死んだのか働いているのか分からない。
  send(method, params, sessionId) {
    if (this.dead) return Promise.reject(new Error(this.dead + ' (' + method + ')'));
    const id = ++this.id;
    const msg = { id, method, params: params || {} };
    if (sessionId) msg.sessionId = sessionId;
    const p = new Promise((res, rej) => this.pending.set(id, { res, rej }));
    try { this.ws.send(JSON.stringify(msg)); } catch (e) {
      this.pending.delete(id);
      return Promise.reject(new Error('CDP へ送れません (' + method + '): ' + e.message));
    }
    return withTimeout(p, BUDGET.cdp, 'CDP ' + method).catch(e => {
      this.pending.delete(id);
      throw e;
    });
  }
  close() { this.dead = this.dead || '検査側から閉じました'; this.ws.close(); }
}

async function launchBrowser(bin) {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'zv-remote-check-'));
  const args = [
    '--headless=new', '--disable-gpu', '--no-sandbox', '--no-first-run',
    '--no-default-browser-check', '--disable-extensions', '--disable-background-networking',
    '--disable-sync', '--disable-default-apps', '--mute-audio', '--hide-scrollbars',
    // ── コンテナ / CI で固まらないため ──────────────────────────────
    // /dev/shm が 64MB しか無い環境 (Docker の既定) では、共有メモリを
    // 使い切ったレンダラが**応答を返さなくなる**。ファイル経由に落とす。
    '--disable-dev-shm-usage',
    // crashpad は**別プロセス**で、親を殺しても残る (CI のログの最後に
    // chrome_crashpad_handler が並んでいた)。そもそも起こさない。
    // **実物で確かめた綴りだけを書く** (CLAUDE.md: フラグを捏造しない)。
    // Linux の chromium 151 を grep -a すると disable-breakpad は 1 件、
    // disable-crash-reporter は **0 件** — 後者は無い綴りなので置かない。
    '--disable-breakpad',
    // ── 隠れたタブの時計を止めさせない ─────────────────────────────
    // headless はタブを「見えていない」と見なすことがある。止まると
    // ポーリング検査が「取りに行っていない」と誤判定し、乱歩は 1 状態も
    // 進まない = 進捗ゼロのまま時限まで待つ、という形で間欠的に落ちる。
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--disable-renderer-backgrounding',
    // 短い間隔で叩くので、IPC の絞り込みに引っかからないようにする。
    '--disable-ipc-flooding-protection',
    '--remote-debugging-port=0', '--user-data-dir=' + profile, 'about:blank',
  ];
  // **detached** で起こして子を独立したプロセスグループの長にする。
  // こうしないと孫 (レンダラ / GPU / crashpad) をまとめて畳めない。
  const proc = spawn(bin, args, {
    stdio: ['ignore', 'ignore', 'pipe'],
    detached: process.platform !== 'win32',
  });
  let closed = false;
  const wipe = () => {
    if (closed) return;
    closed = true;
    killTree(proc);
    try { fs.rmSync(profile, { recursive: true, force: true }); } catch (e) { /* 消せなくても続ける */ }
  };
  CLEANUP.push(wipe);
  const wsUrl = await new Promise((res, rej) => {
    let buf = '';
    const to = setTimeout(
      () => rej(new Error('ブラウザが ' + BUDGET.launch + 'ms で立ち上がりませんでした:\n' + buf)),
      BUDGET.launch);
    if (to.unref) to.unref();
    proc.stderr.on('data', d => {
      // 溜め続けるとログが長い版で膨らむので、末尾だけ持つ。
      buf = (buf + d.toString()).slice(-8192);
      const m = buf.match(/ws:\/\/\S+/);
      if (m) { clearTimeout(to); res(m[0]); }
    });
    proc.on('error', e => { clearTimeout(to); rej(new Error('ブラウザを起こせません: ' + e.message)); });
    proc.on('exit', c => { clearTimeout(to); rej(new Error('ブラウザが即終了しました (rc=' + c + ')\n' + buf)); });
  }).catch(e => { wipe(); throw e; });
  const cdp = await Cdp.connect(wsUrl).catch(e => { wipe(); throw e; });
  return {
    cdp,
    async close() {
      try { cdp.close(); } catch (e) { /* もう閉じている */ }
      const gone = new Promise(r => {
        if (proc.exitCode !== null || proc.signalCode) { r(); return; }
        proc.once('exit', () => r());
      });
      wipe();
      // 死ぬまで待つ。**待たないと次の実行と重なる**が、待ち続けもしない。
      await withTimeout(gone, BUDGET.close, 'ブラウザの終了')
        .catch(() => { process.stdout.write('  … ブラウザが時間内に終わりませんでした (畳み込み済み)\n'); });
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
    step(spec.name + ': ページを開く');
    await br.cdp.send('Page.navigate', { url: srv.url + '?t=zv-test' }, sid);
    await Promise.race([loaded, wait(BUDGET.load)]);

    // 起動直後の /api/state が返るまで待つ (返る前に叩くと空の画面を検査する)
    for (let i = 0; i < 60; i++) {
      if (await evalJs(br.cdp, sid, 'typeof state !== "undefined" && state !== null')) break;
      await wait(100);
    }

    // (7) ポーリングが**実際に飛んでいる**か - 判定はページの中ではなく
    //     サーバ側の受信数で行う (ページの中の変数はいくらでも嘘をつける)。
    const pollV = [];
    step(spec.name + ': ポーリングが飛んでいるか');
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
    step(spec.name + ': 総当たり + 乱歩');
    evalJs(br.cdp, sid, src, true).then(v => { out = v; done = true; }, e => { err = e; done = true; });
    let last = -1, blind = 0, cur = '?';
    const t0 = Date.now();
    // **止まった時間は「回した数 × 500ms」ではなく壁時計で数える。**
    // 1 往復が時限まで返らない局面では 1 周が 30 秒かかるので、回数で
    // 数えると 25 秒の猶予が実際には 25 分になる (静かな嘘)。
    let moved = Date.now();
    while (!done) {
      await wait(500);
      let seen = false;
      try {
        cur = await evalJs(br.cdp, sid, 'window.__zv ? window.__zv.steps + "|" + window.__zv.label : "?"');
        blind = 0;
        seen = true;
      } catch (e) {
        // **応えないのは「進んでいない」ではなく「壊れている」。**
        // 読めなかったことを進捗と数えない (数えると永久に緑のまま待つ)。
        cur = '? (' + e.message + ')';
        if (++blind >= 3) {
          throw new Error('ブラウザが進み具合に ' + blind + ' 回続けて応えません: ' + e.message);
        }
      }
      if (seen) {
        const n = parseInt(String(cur).split('|')[0], 10) || 0;
        if (n !== last) { last = n; moved = Date.now(); touch(); }
      }
      if (Date.now() - moved >= spec.stallMs) {
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
    // **ここが返らないと、検査は無音のまま止まる。** 予算つきで畳む。
    const note = await srv.close();
    if (note) process.stdout.write('  … ' + note + '\n');
  }
}

// ─────────────────────────────────────────────────────────────────────
// 8. 入口
// ─────────────────────────────────────────────────────────────────────
function parseArgs(argv) {
  const o = {
    selfTest: false, inject: null, list: false, keep: false, help: false, hang: null,
    settle: 20, walk: 120, seed: 20260817, tapMin: 24, timeout: 240000, stallMs: 25000, lang: null,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--self-test') o.selfTest = true;
    else if (a === '--list') o.list = true;
    else if (a === '--keep') o.keep = true;
    else if (a === '--inject') o.inject = argv[++i];
    // **時限が空回りしていないことを確かめるための仕込み。**
    // 「入れました」だけの時限は、効かないまま何年も残る。
    else if (a === '--hang') o.hang = argv[++i];
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
    console.log('使い方: node tools/remote-check.js [--self-test|--inject <名前>|--hang <名前>|--list]'
      + ' [--seed N] [--walk N] [--lang ja] [--keep]');
    console.log('時限 (ms) は環境変数で変えられます:');
    console.log('  ZV_DEADLINE_MS 全体の上限 / ZV_STALL_MS 進捗が止まってからの猶予');
    console.log('  ZV_CDP_MS 1 往復 / ZV_WS_MS 握手 / ZV_LAUNCH_MS 起動 / ZV_LOAD_MS 読み込み');
    console.log('  ZV_CLOSE_MS 後始末 / ZV_HEARTBEAT_MS 生存の合図 (0 で止める)');
    return 0;
  }
  if (o.list) {
    for (const k of Object.keys(INJECTIONS)) console.log(k.padEnd(14) + ' ' + INJECTIONS[k].desc);
    console.log('--- わざと固める仕込み (--hang) ---');
    for (const k of Object.keys(HANGS)) console.log(k.padEnd(14) + ' ' + HANGS[k]);
    return 0;
  }
  // ここから先は「待つ」処理しかない。**見張りを立ててから**入る。
  const watchdog = armWatchdog();
  const beat = armHeartbeat();
  try {
    return await run(o);
  } finally {
    clearInterval(watchdog);
    if (beat) clearInterval(beat);
    runCleanup();
  }
}

// ─────────────────────────────────────────────────────────────────────
// 8-2. わざと固める仕込み — **空回りする時限を残さない**
// ─────────────────────────────────────────────────────────────────────
// 「時限を入れました」だけでは、効いているかどうか誰も知らない。時限は
// 普段 1 度も撃たないので、**壊れても気付けない**。だから固まる形を用意して、
// 自己検査から毎回実際に撃つ。
const HANGS = {
  deadline: '全体の見張り: 何も進まないまま待ち続ける',
  cdp: 'CDP の 1 往復: 返らない評価を撃つ',
  server: '後始末: keep-alive を握ったまま偽サーバを閉じる',
};

// 偽サーバを keep-alive で握ったまま閉じる。ブラウザは要らない。
// **CI で無音のまま止まった形はこれ** (Node の server.close() は生きている
// 接続が 1 本でもあるとコールバックを呼ばない)。
// 返らない要求を 1 本抱えた偽サーバを畳む。**2 段とも試す**:
//   A: 普段の道 (自分から接続を切る) — すぐ返ること
//   B: その道を塞いで**時限だけ**に任せる — 時限が空回りしていないこと
async function hangServer() {
  let rc = 0;
  const round = async (label, hold) => {
    if (hold) process.env.ZV_HOLD_SOCKETS = '1';
    else delete process.env.ZV_HOLD_SOCKETS;
    step('偽サーバ (' + label + '): 返らない要求を 1 本抱えさせる');
    const srv = await startServer(
      buildPage({ lang: null, appendJs: '', replace: [] }), makeModel(1), { scrollback: true });
    const sock = net.connect({ host: '127.0.0.1', port: Number(new URL(srv.url).port) });
    await new Promise((res, rej) => { sock.once('connect', res); sock.once('error', rej); });
    sock.on('error', () => { /* 畳むときに切れる */ });
    sock.write('GET /zv-never HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n');
    await wait(300);
    const t0 = Date.now();
    step('後始末に入ります (' + label + ')');
    const note = await srv.close();
    const ms = Date.now() - t0;
    console.log('  … ' + label + ': ' + ms + 'ms で返った'
      + (note ? ' / ' + note : ' / 自分から接続を切りました'));
    if (hold && !note) {
      console.log('✗ 時限が撃っていません (後ろの砦が空回りしています)');
      rc = 1;
    }
    try { sock.destroy(); } catch (e) { /* もう閉じている */ }
  };
  await round('普段の道', false);
  await round('時限だけ', true);
  step('後始末は 2 段とも返ってきました');
  return rc;
}

async function hangWith(br, kind) {
  if (kind === 'deadline') {
    step('わざと止まります (進捗を止めて見張りを試す)');
    await new Promise(() => { /* 永久に返らない */ });
    return 1;
  }
  if (kind === 'cdp') {
    step('わざと返らない CDP を撃ちます');
    const t = await br.cdp.send('Target.createTarget', { url: 'about:blank' });
    const sid = (await br.cdp.send(
      'Target.attachToTarget', { targetId: t.targetId, flatten: true })).sessionId;
    await br.cdp.send('Runtime.evaluate',
      { expression: 'new Promise(function () {})', awaitPromise: true }, sid);
    console.log('✗ 返らないはずの CDP が返ってきました (仕込みが効いていない)');
    return 1;
  }
  console.error('知らない仕込み: ' + kind + ' (' + Object.keys(HANGS).join(' / ') + ')');
  return 64;
}

// 自分自身を子プロセスとして起こす。**予算を小さくして** 待たずに済ませる。
function runChild(args, env, capMs) {
  return new Promise(resolve => {
    const p = spawn(process.execPath, [__filename].concat(args), {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: Object.assign({}, process.env, env),
      detached: process.platform !== 'win32',
    });
    let out = '';
    const grab = d => { out = (out + d.toString()).slice(-16384); };
    p.stdout.on('data', grab);
    p.stderr.on('data', grab);
    const t0 = Date.now();
    let cut = false;
    const cap = setTimeout(() => { cut = true; killTree(p); }, capMs);
    p.on('error', e => {
      clearTimeout(cap);
      resolve({ code: -1, out: out + ' (' + e.message + ')', ms: Date.now() - t0, over: true });
    });
    p.on('exit', code => {
      clearTimeout(cap);
      resolve({ code: code === null ? -1 : code, out, ms: Date.now() - t0, over: cut });
    });
  });
}

// 仕込んだハングが**全部**時限に捕まること。1 つでも捕まらなければ赤。
async function deadlinesBite() {
  const cases = [
    {
      kind: 'deadline', red: true, cap: 90000,
      env: { ZV_STALL_MS: '5000', ZV_DEADLINE_MS: '60000', ZV_HEARTBEAT_MS: '2000' },
      want: '1 つも進みませんでした',
    },
    { kind: 'cdp', red: true, cap: 90000, env: { ZV_CDP_MS: '3000' }, want: 'ms で返りませんでした' },
    { kind: 'server', red: false, cap: 90000, env: { ZV_CLOSE_MS: '1500' }, want: '握られたままの接続' },
  ];
  let ok = true;
  for (const c of cases) {
    step('時限の検査: --hang ' + c.kind);
    const r = await runChild(['--hang', c.kind], c.env, c.cap);
    const said = r.out.indexOf(c.want) >= 0;
    const ended = !r.over;
    const redOk = c.red ? r.code !== 0 : r.code === 0;
    if (said && ended && redOk) {
      console.log('✓ 時限 [' + c.kind + '] — ' + (r.ms / 1000).toFixed(1) + 's で'
        + (c.red ? ' 赤 (rc=' + r.code + ') で' : ' 正常に') + '終わった: ' + HANGS[c.kind]);
    } else {
      ok = false;
      console.log('✗ 時限 [' + c.kind + '] — 効いていない (rc=' + r.code + ' / '
        + (r.ms / 1000).toFixed(1) + 's / 打ち切り=' + r.over + ' / 言い分=' + said + ')');
      for (const l of r.out.split(NL).slice(-8)) console.log('    ' + l);
    }
  }
  return ok;
}

async function run(o) {
  // ブラウザが要らない仕込みは、探す前に済ませる。
  if (o.hang === 'server') return hangServer();
  const bin = findBrowser();
  if (!bin) {
    console.log('[skip] Chromium 系のブラウザが見つかりません '
      + '(Chrome / Chromium / Edge / Brave のどれかを入れるか、ZV_BROWSER に実行ファイルを指定)');
    return 2;
  }
  console.log('ブラウザ: ' + bin);
  step('ブラウザを起こす');
  const br = await launchBrowser(bin);
  step('ブラウザが CDP の口を開きました');
  let rc = 0;
  try {
    if (o.hang) return await hangWith(br, o.hang);
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
      // (3) **時限そのものが効くか。** 入れただけの時限は、効かなくなっても
      //     普段 1 度も撃たないので誰も気付けない。毎回わざと固めて確かめる。
      if (!(await deadlinesBite())) rc = 1;
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
      step('検査: ' + s.name);
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

// 途中で止められても**ブラウザを置き去りにしない**。孫まで畳んでから降りる。
for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(sig, () => {
    sayNow(NL + sig + ' を受けました — ブラウザを畳んで降ります' + NL);
    runCleanup();
    process.exit(130);
  });
}

main().then(c => { runCleanup(); process.exit(c); }).catch(e => {
  console.error('検査そのものが失敗しました: ' + (e && e.stack ? e.stack : e));
  runCleanup();
  process.exit(1);
});
