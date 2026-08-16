// ─── 変更一覧 (PC の「変更をまとめて開く」= Zed の project diff 相当) ─────────
//
// 下部ナビは **4 つのまま**。ファイルタブの中を「\u{1F4C1} ファイル / \u{1F4DD} 変更」の
// 2 面に分ける (エージェントタブの .seg と同じ作法)。到達経路を増やさない。
//
// * DOM も CSS も**このモジュールが自分で作る** — body.html / style.css は触らない
// * `/api/changes` `/api/diff` が**まだ無いサーバ**へ繋がることがある。
//   404 でも未知の形でも画面を壊さず、空状態のカード 1 枚を出して降りる
// * **自動ポーリングをしない** (設計原則 3)。開いた 1 回と、明示的な更新
//   (\u{1F504} ボタン / 引っぱって更新) だけ取りに行く
// * diff の中身は外部由来なので **必ず textContent**。innerHTML には入れない
const ZC_CSS = `
  /* ファイルタブの中の面切替。指の当たり判定は 44px 以上 */
  #chseg button { min-height:44px; }
  #chbar { border-top:none; border-bottom:1px solid #21262d; }
  #chbar .btn { min-height:44px; }
  #chbar .t { flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;
              font-size:12.5px; color:#c9d1d9; font-weight:700; }
  #chbar .sub { flex:none; font-size:11px; color:#8b949e; }
  #chwrap { flex:1; overflow-y:auto; -webkit-overflow-scrolling:touch; }
  /* 引っぱって更新の帯。既定は高さ 0 — 何も無いのに場所を取らない */
  #chpull { height:0; overflow:hidden; display:flex; align-items:center; justify-content:center;
            color:#8b949e; font-size:12px; }
  #chbody { padding:8px 10px 12px; }
  /* 空状態は利用可能領域の中央に 1 枚 (CLAUDE.md の UI 原則) */
  #chbody.chmid { min-height:100%; display:flex; align-items:center; justify-content:center; padding:16px; }
  .chbadge { flex:none; width:20px; height:20px; border-radius:5px; font-size:11px; font-weight:800;
             display:flex; align-items:center; justify-content:center; color:#0d1117; }
  .chnum { flex:none; font:11px ui-monospace,SFMono-Regular,Menlo,monospace; color:#8b949e; }
  .chnum .a { color:#3fb950; }
  .chnum .d { color:#f85149; }
  .chdir { margin-top:2px; font-size:11px; color:#8b949e;
           overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  .chnote { margin:0 0 8px; padding:9px 11px; border-radius:8px;
            background:#3a2c12; border:1px solid #d29922; color:#f2dfb4; font-size:12px; line-height:1.6; }
  .chhd { font:12px/1.4 ui-monospace,SFMono-Regular,Menlo,monospace !important; }
  .chhd.op { border-radius:10px 10px 0 0; }
  .chhd .h { flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  /* 横スクロールは **このブロックの中だけ**。ページ本体は横に流さない */
  .chhunk { overflow-x:auto; -webkit-overflow-scrolling:touch;
            border:1px solid #21262d; border-top:none; border-radius:0 0 10px 10px; }
  .chrow { display:flex; width:max-content; min-width:100%; background:#0d1117; }
  /* 追加 / 削除の地色は既存のアクセント (#3fb950 / #f85149) を暗く落としたもの。
     行番号の桁を横スクロール中も貼り付けておくため、透明にはできない */
  .chrow.add { background:#132a1c; }
  .chrow.del { background:#2d1618; }
  .chrow:active { background:#1f3a5f; }
  .chg { position:sticky; left:0; flex:none; display:flex; background:inherit;
         border-right:1px solid #21262d; }
  .chln { width:3.4em; padding:0 4px; text-align:right; color:#6e7681;
          font:10.5px/1.7 ui-monospace,SFMono-Regular,Menlo,monospace; }
  .chsg { width:1.2em; text-align:center; font:11.5px/1.7 ui-monospace,SFMono-Regular,Menlo,monospace; }
  .chrow.add .chsg { color:#3fb950; }
  .chrow.del .chsg { color:#f85149; }
  .chtx { white-space:pre; padding:0 12px 0 6px; color:#e6edf3;
          font:11.5px/1.7 ui-monospace,SFMono-Regular,Menlo,monospace; }
`;
(function zcInjectStyle() {
  const st = document.createElement('style');
  st.textContent = ZC_CSS;
  document.head.appendChild(st);
})();

// 状態: 面 ('files' | 'changes') / 一覧 / いま開いているファイル / そのハンク。
// **タイマーは 1 本も持たない** (見ていない画面のために回さない)。
let zcMode = 'files', zcData = null, zcErr = '', zcBusy = false, zcLoaded = false;
let zcFile = '', zcHunks = null, zcDiffErr = '', zcDiffBusy = false, zcOpenHunk = {};
let zcPullH = 0, zcPullY = 0, zcPulling = false;
const ZC_PULL = 64;

// ── 受け取りの正規化 ──────────────────────────────────────────────
// 契約どおりでない応答 (古いサーバ / 途中の実装) でも例外を投げない。
// 「知らない形は既定値へ落とす」で、画面は空状態まで必ず降りられる。
function zcNum(v) { return typeof v === 'number' && isFinite(v) ? Math.round(v) : 0; }
function zcLn(v) { return typeof v === 'number' && isFinite(v) && v > 0 ? Math.floor(v) : 0; }
function zcStr(v) { return typeof v === 'string' ? v : ''; }
function zcErrOf(r) {
  const e = r && typeof r === 'object' ? zcStr(r.error) : '';
  return e;
}
function zcNormChanges(r) {
  const o = r && typeof r === 'object' ? r : {};
  const src = Array.isArray(o.files) ? o.files : [];
  const files = src
    .filter(f => f && typeof f === 'object' && zcStr(f.rel))
    .map(f => ({
      rel: zcStr(f.rel),
      status: (zcStr(f.status) || '?').slice(0, 1).toUpperCase(),
      added: zcNum(f.added),
      removed: zcNum(f.removed),
      binary: f.binary === true,
    }));
  // 合計が来ていなければ行から数え直す (「0 ファイル · +0 −0」と嘘を書かない)
  const sum = k => files.reduce((a, f) => a + f[k], 0);
  return {
    files: files,
    added: typeof o.added === 'number' ? zcNum(o.added) : sum('added'),
    removed: typeof o.removed === 'number' ? zcNum(o.removed) : sum('removed'),
    truncated: o.truncated === true,
  };
}
function zcNormDiff(r) {
  const o = r && typeof r === 'object' ? r : {};
  const src = Array.isArray(o.hunks) ? o.hunks : [];
  return {
    binary: o.binary === true,
    truncated: o.truncated === true,
    hunks: src
      .filter(h => h && typeof h === 'object')
      .map(h => ({
        header: zcStr(h.header) || '@@',
        start: zcLn(h.new_start),
        lines: (Array.isArray(h.lines) ? h.lines : [])
          .filter(l => l && typeof l === 'object')
          .map(l => ({
            // k は ctx|add|del の 3 値のみ。知らない語は文脈として出す
            k: l.k === 'add' || l.k === 'del' ? l.k : 'ctx',
            o: zcLn(l.o),
            n: zcLn(l.n),
            t: zcStr(l.t),
          })),
      })),
  };
}

// ── DOM を自分で組む (body.html を触らない) ───────────────────────
const zcSeg = document.createElement('div');
zcSeg.className = 'seg'; zcSeg.id = 'chseg';
const zcBar = document.createElement('div');
zcBar.className = 'bar'; zcBar.id = 'chbar'; zcBar.style.display = 'none';
const zcWrap = document.createElement('div');
zcWrap.id = 'chwrap'; zcWrap.style.display = 'none';
const zcPullEl = document.createElement('div');
zcPullEl.id = 'chpull';
const zcBody = document.createElement('div');
zcBody.id = 'chbody';
zcWrap.appendChild(zcPullEl);
zcWrap.appendChild(zcBody);
(function zcMount() {
  const host = $('v-files');
  if (!host) return;
  host.insertBefore(zcSeg, host.firstChild);
  host.appendChild(zcBar);
  host.appendChild(zcWrap);
})();

const ZC_SEGS = [
  ['files', () => T('remote.changes_seg_files', '\u{1F4C1} ファイル')],
  ['changes', () => T('remote.changes_seg', '\u{1F4DD} 変更')],
];
function zcRenderSeg() {
  zcSeg.innerHTML = '';
  ZC_SEGS.forEach(([k, lab]) => {
    const b = document.createElement('button');
    if (zcMode === k) b.className = 'act';
    b.textContent = lab();
    b.onclick = () => zcSetMode(k);
    zcSeg.appendChild(b);
  });
}
// 面の切替。**上下のバーは動かさない** (画面が突然作り替わったように見せない)
function zcSetMode(k) {
  if (zcMode === k) return;
  zcMode = k;
  const ch = k === 'changes';
  const filter = document.querySelector('#v-files .bar');
  if (filter && filter !== zcBar) filter.style.display = ch ? 'none' : '';
  const fl = $('flist');
  if (fl) fl.style.display = ch ? 'none' : '';
  zcBar.style.display = ch ? '' : 'none';
  zcWrap.style.display = ch ? '' : 'none';
  zcRenderSeg();
  if (ch) { zcRender(); if (!zcLoaded) zcLoad(true); }
}

// ── 取得 ──────────────────────────────────────────────────────────
async function zcLoad(quiet) {
  if (zcBusy) return;
  zcBusy = true; zcRender();
  let ok = false;
  try {
    const r = await api('/api/changes');
    const e = zcErrOf(r);
    if (e) { zcData = null; zcErr = e; } else { zcData = zcNormChanges(r); zcErr = ''; ok = true; }
  } catch (e) {
    zcData = null;
    zcErr = T('remote.changes_unavailable', '変更一覧を取得できませんでした — PC 側がこの機能に対応していない可能性があります');
  }
  zcLoaded = true; zcBusy = false;
  zcRender();
  if (ok && !quiet) toast(T('remote.changes_refreshed', '\u{1F504} 変更一覧を更新しました'));
}
async function zcLoadDiff(quiet) {
  if (!zcFile || zcDiffBusy) return;
  zcDiffBusy = true; zcRender();
  let ok = false;
  try {
    const r = await api('/api/diff?path=' + encodeURIComponent(zcFile));
    const e = zcErrOf(r);
    if (e) { zcHunks = null; zcDiffErr = e; } else { zcHunks = zcNormDiff(r); zcDiffErr = ''; ok = true; }
  } catch (e) {
    zcHunks = null;
    zcDiffErr = T('remote.changes_diff_unavailable', '差分を取得できませんでした — PC 側がこの機能に対応していない可能性があります');
  }
  zcDiffBusy = false;
  zcRender();
  if (ok && !quiet) toast(T('remote.changes_refreshed', '\u{1F504} 変更一覧を更新しました'));
}
function zcRefresh(quiet) { return zcFile ? zcLoadDiff(quiet) : zcLoad(quiet); }

// ── 描画 ──────────────────────────────────────────────────────────
function zcBtn(label, title, fn) {
  const b = document.createElement('button');
  b.className = 'btn'; b.textContent = label; b.title = title;
  b.onclick = fn;
  return b;
}
function zcCard(big, text) {
  const d = document.createElement('div');
  d.className = 'mid-card';
  const b = document.createElement('span');
  b.className = 'big'; b.textContent = big;
  d.appendChild(b);
  const t = document.createElement('span');
  t.textContent = text;
  d.appendChild(t);
  return d;
}
function zcNote(text) {
  const d = document.createElement('div');
  d.className = 'chnote'; d.textContent = text;
  return d;
}
function zcRenderBar() {
  zcBar.innerHTML = '';
  if (zcFile) {
    zcBar.appendChild(zcBtn(T('remote.changes_back', '← 一覧'),
      T('remote.changes_back_title', '変更ファイルの一覧へ戻る'),
      () => { zcFile = ''; zcHunks = null; zcDiffErr = ''; zcRender(); }));
  }
  const t = document.createElement('span');
  t.className = 't';
  t.textContent = zcFile || T('remote.changes_title', '\u{1F4DD} 変更');
  zcBar.appendChild(t);
  if (!zcFile && zcData) {
    const s = document.createElement('span');
    s.className = 'sub';
    s.textContent = T('remote.changes_summary', '{n} ファイル · +{a} −{d}')
      .replace('{n}', zcData.files.length).replace('{a}', zcData.added).replace('{d}', zcData.removed);
    zcBar.appendChild(s);
  }
  zcBar.appendChild(zcBtn(T('remote.changes_refresh', '\u{1F504}'),
    T('remote.changes_refresh_title', '最新の変更を取り直す'), () => zcRefresh(false)));
}
const ZC_STATUS = {
  M: ['#d29922', () => T('remote.changes_st_modified', '変更済み')],
  A: ['#3fb950', () => T('remote.changes_st_added', '追加済み')],
  D: ['#f85149', () => T('remote.changes_st_deleted', '削除済み')],
  R: ['#7ee1ff', () => T('remote.changes_st_renamed', '改名')],
};
function zcFileRow(f) {
  const c = document.createElement('div');
  c.className = 'card';
  const hd = document.createElement('div'); hd.className = 'hd';
  const st = ZC_STATUS[f.status] || ['#8b949e', () => T('remote.changes_st_untracked', '追跡外')];
  const bg = document.createElement('span');
  bg.className = 'chbadge'; bg.style.background = st[0];
  bg.textContent = f.status; bg.title = st[1]();
  hd.appendChild(bg);
  const nm = document.createElement('span'); nm.className = 'nm';
  // Windows のパスは区切りが `\` で来る (40-files.js と同じ扱い)
  const i = Math.max(f.rel.lastIndexOf('/'), f.rel.lastIndexOf('\\'));
  nm.textContent = i >= 0 ? f.rel.slice(i + 1) : f.rel;
  hd.appendChild(nm);
  const num = document.createElement('span'); num.className = 'chnum';
  if (f.binary) {
    num.textContent = T('remote.changes_binary', 'バイナリ');
  } else {
    const a = document.createElement('span'); a.className = 'a'; a.textContent = '+' + f.added;
    const d = document.createElement('span'); d.className = 'd'; d.textContent = ' −' + f.removed;
    num.appendChild(a); num.appendChild(d);
  }
  hd.appendChild(num);
  c.appendChild(hd);
  if (i >= 0) {
    const dir = document.createElement('div');
    dir.className = 'chdir'; dir.textContent = f.rel.slice(0, i);
    c.appendChild(dir);
  }
  c.onclick = () => zcOpenFile(f.rel);
  return c;
}
function zcOpenFile(rel) {
  zcFile = rel; zcHunks = null; zcDiffErr = ''; zcOpenHunk = {};
  zcWrap.scrollTop = 0;
  zcLoadDiff(true);
}
function zcRow(l, h) {
  const r = document.createElement('div');
  r.className = 'chrow' + (l.k === 'add' ? ' add' : l.k === 'del' ? ' del' : '');
  const g = document.createElement('span'); g.className = 'chg';
  const o = document.createElement('span'); o.className = 'chln'; o.textContent = l.o ? String(l.o) : '';
  const n = document.createElement('span'); n.className = 'chln'; n.textContent = l.n ? String(l.n) : '';
  const s = document.createElement('span'); s.className = 'chsg';
  s.textContent = l.k === 'add' ? '+' : l.k === 'del' ? '−' : ' ';
  g.appendChild(o); g.appendChild(n); g.appendChild(s);
  r.appendChild(g);
  const tx = document.createElement('span'); tx.className = 'chtx';
  tx.textContent = l.t;
  r.appendChild(tx);
  // 削除行はもう本文に無いので、そのハンクの先頭 (新側) へ寄せて開く
  const line = l.n || h.start || 0;
  if (line > 0) r.onclick = () => zcOpenAt(line);
  return r;
}
function zcHunkBlock(h, idx) {
  const open = zcOpenHunk[idx] !== false;
  const box = document.createElement('div'); box.className = 'lane';
  const hd = document.createElement('button');
  hd.className = 'lhd chhd' + (open ? ' op' : '');
  const car = document.createElement('span'); car.textContent = open ? '▾' : '▸';
  hd.appendChild(car);
  const t = document.createElement('span'); t.className = 'h'; t.textContent = h.header;
  hd.appendChild(t);
  hd.onclick = () => { zcOpenHunk[idx] = !open; zcRender(); };
  box.appendChild(hd);
  if (open) {
    const body = document.createElement('div'); body.className = 'chhunk';
    h.lines.forEach(l => body.appendChild(zcRow(l, h)));
    box.appendChild(body);
  }
  return box;
}
async function zcOpenAt(line) {
  const path = zcFile;
  try {
    await api('/api/open', {path: path, line: line});
    toast(T('remote.changes_opened', '\u{1F4C4} {path}:{line} を PC で開きました')
      .replace('{path}', path).replace('{line}', line));
  } catch (e) {
    toast(T('remote.changes_open_failed', 'エディタで開けませんでした'));
  }
}
function zcRenderList(box) {
  if (zcBusy && !zcData) {
    box.classList.add('chmid');
    box.appendChild(zcCard('\u{1F504}', T('remote.changes_loading', '読み込み中…')));
    return;
  }
  if (zcErr) {
    box.classList.add('chmid');
    box.appendChild(zcCard('\u{26A0}', zcErr));
    return;
  }
  const fs = (zcData && zcData.files) || [];
  if (!fs.length) {
    box.classList.add('chmid');
    box.appendChild(zcCard('✅', T('remote.changes_empty', '変更はありません')));
    return;
  }
  if (zcData.truncated) {
    box.appendChild(zcNote(T('remote.changes_truncated', '{n} 件で打ち切り — 全ては表示していません')
      .replace('{n}', fs.length)));
  }
  fs.forEach(f => box.appendChild(zcFileRow(f)));
}
function zcRenderDiff(box) {
  if (zcDiffBusy && !zcHunks) {
    box.classList.add('chmid');
    box.appendChild(zcCard('\u{1F504}', T('remote.changes_loading', '読み込み中…')));
    return;
  }
  if (zcDiffErr) {
    box.classList.add('chmid');
    box.appendChild(zcCard('\u{26A0}', zcDiffErr));
    return;
  }
  if (zcHunks && zcHunks.binary) {
    box.classList.add('chmid');
    box.appendChild(zcCard('\u{1F4E6}', T('remote.changes_binary_body', 'バイナリファイルなので差分は表示できません')));
    return;
  }
  const hs = (zcHunks && zcHunks.hunks) || [];
  if (!hs.length) {
    box.classList.add('chmid');
    box.appendChild(zcCard('✅', T('remote.changes_no_hunks', '表示できる差分がありません')));
    return;
  }
  box.appendChild(zcNote(T('remote.changes_open_hint', '行をタップすると PC のエディタでその行を開きます')));
  hs.forEach((h, i) => box.appendChild(zcHunkBlock(h, i)));
  if (zcHunks.truncated) {
    box.appendChild(zcNote(T('remote.changes_hunks_truncated', 'ハンクが多いため一部だけ表示しています')));
  }
}
function zcRender() {
  zcRenderSeg();
  zcRenderBar();
  // 畳む / 開くで作り直すので、読んでいる位置を必ず戻す
  const keep = zcWrap.scrollTop;
  zcBody.innerHTML = '';
  zcBody.classList.remove('chmid');
  if (zcFile) zcRenderDiff(zcBody); else zcRenderList(zcBody);
  zcWrap.scrollTop = keep;
}

// ── 引っぱって更新 (タイマーを持たない代わりの入口) ────────────────
function zcPullSet(h) {
  zcPullH = h;
  zcPullEl.style.height = h + 'px';
  zcPullEl.textContent = h >= ZC_PULL
    ? T('remote.changes_release', '離すと更新')
    : (h > 0 ? T('remote.changes_pull', '引っぱって更新') : '');
}
zcWrap.addEventListener('touchstart', e => {
  zcPulling = zcWrap.scrollTop <= 0 && e.touches.length === 1;
  if (zcPulling) zcPullY = e.touches[0].clientY;
}, {passive: true});
zcWrap.addEventListener('touchmove', e => {
  if (!zcPulling) return;
  const dy = e.touches[0].clientY - zcPullY;
  if (dy <= 0 || zcWrap.scrollTop > 0) { if (zcPullH) zcPullSet(0); return; }
  e.preventDefault();
  zcPullSet(Math.min(dy * 0.5, ZC_PULL + 16));
}, {passive: false});
function zcPullEnd() {
  if (!zcPulling) return;
  zcPulling = false;
  const hit = zcPullH >= ZC_PULL;
  zcPullSet(0);
  if (hit) zcRefresh(false);
}
zcWrap.addEventListener('touchend', zcPullEnd, {passive: true});
zcWrap.addEventListener('touchcancel', zcPullEnd, {passive: true});

// 「変更」面を選んだままファイルタブへ戻ってきたときの 1 回だけの取得。
// 10-view.js の既定ハンドラとは別に足す (共有ファイルを触らない)。
$('nav').addEventListener('click', e => {
  const b = e.target.closest('button');
  if (b && b.dataset.v === 'files' && zcMode === 'changes' && !zcLoaded) zcLoad(true);
});
zcRenderSeg();
