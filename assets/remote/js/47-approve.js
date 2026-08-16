// ─── 承認キュー (スマホ) ────────────────────────────────────────────────
//
// GitHub Mobile は通知とレビューは強いが**エージェントを操れない**。ここは
// 「手元の PC で動いているエージェントへ、スマホから承認だけ返す」ための画面。
//
// ## 置き場所
// 下部ナビは 4 つのまま増やさない。エージェントタブの中のビュー切替
// (端末 / 待ち / デッキ / 看板) に 5 つ目として「✅ 承認」を足す。
//
// `70-boards.js` は共有ファイルなので 1 バイトも触らない。代わりに
// **関数宣言の巻き上げ**を使って `renderSeg` / `setAView` / `pollTerm` /
// `pollState` を包む。連結された 1 本のスクリプトでは、どの文が動くより先に
// 全ての関数宣言が束縛されるので、番号がより小さいこのファイルからでも
// 「後ろのファイルで宣言された関数」を掴んで差し替えられる。
// (`let aview` / `let termTimer` は逆に **TDZ** なので、トップレベルでは
//  絶対に触らない — 関数の中からだけ読む)
//
// ## サーバが未実装でも壊れない
// `GET /api/approvals` / `POST /api/approve` は**古い PC には無い**。
// 404 を掴んだら `apprApi = false` にして二度と叩かず、`state.agents` の
// `attention` (PC が判定したもの) から一覧を組み、既存の `/api/agent_act` の
// `approve` へ静かに落ちる。種別も根拠も無いので**「不明」と正直に出す**。
//
// ## アイドルのコストはゼロ (設計原則 3)
// * 承認ビューを見ている間だけ `/api/approvals` を叩く。間隔は既存の
//   `pollTerm` (1.5 秒) に相乗りするので、**ポーリングの本数は増えない**
//   (端末を見ていないなら `/api/term` は叩かれていない)
// * 件数バッジは既存の `pollState` (2.5 秒) に相乗り。しかも
//   **エージェントタブを開いていて、動いているエージェントが 1 体以上いる**
//   ときだけ。誰も動いていなければ承認要求は生まれないので叩かない
// * 未対応の版 (`apprApi === false`) では 1 回も叩かない
//
// ## 危険側の守り方 (詳細は各関数のコメント)
// * 承認は 1 タップ。ただし取り返しのつかない種別
//   (管理者権限の昇格 / ファイル削除) だけ確認を挟む
// * 拒否は fail-closed なので確認しない (安全側を押しづらくしない)
// * 「全部承認」は必ず確認 + **何件・どの種別に効くかを先に見せる**。
//   `privilege` は `approvals.rs` の `auto_approvable()` と同じ立場で除外する
// * `⚡ 全自動` への切り替えも確認を挟む
//   (いまはコマンドタブで確認なしの 1 タップ = より安全な停止より危険)
// * `♾ 常に許可` (`act: "always"`) は**設定に残る**ので、必ず確認し、
//   **何が残るのか (PC の config.toml のポリシー) を文面に書く**。
//   `privilege` には出さない — `approvals.rs` の `auto_approvable()` が
//   false を返す種別に「常に許可」を出すのは、**効かない約束を見せる嘘**になる
// * 「いつも拒否」(`always_deny`) は出さない。拒否は 1 件ずつでも安全側なので、
//   永続化して増えるのは誤爆の代償だけ (PC のネイティブ UI の仕事)
//
// ## 横から決着が付くことがある
// 承認は**この画面以外からも**返る (PC のパネル / `notify::webhook` の ntfy
// 通知に載ったボタン)。だから
// * 一覧は毎回サーバの答えで作り直す (手元で足し引きしない)
// * 送った先が既に無くても「失敗」と出さない。取り直して**消えていたら
//   「すでに決着していました」**と正しく言う

const APPR = 'appr';
// 承認ビューを見ている間の取り直し間隔。**既存の pollTerm と同じ値**を使う
// (新しい間隔を増やさない)。
const APPR_POLL_MS = 1500;
// 1 タップでは通さない種別。approvals.rs の「権限昇格は決して自動承認しない」と
// 「rm -rf は shell ではなく file_delete へ落とす」に合わせてある。
const APPR_DANGER = ['privilege', 'file_delete'];
// 自動承認 (まとめて承認 / 常に許可) を許さない種別。
// **approvals.rs の `ApprovalKind::auto_approvable()` と同じ表**にしてある —
// ここが食い違うと「スマホでは常に許可にできたのに PC が無視する」になる。
const APPR_NO_AUTO = ['privilege'];
// 種別 (安定 ID) → アイコンと表示名。**種別を決めるのは PC 側** (approvals.rs) で、
// ここでやっているのは受け取った ID の翻訳だけ。画面文字から種別を作らない。
// 表に無い ID は生の ID をそのまま出す (勝手に狭い種別へ丸めない)。
const APPR_KINDS = {
  file_read: ['\u{1F441}', () => T('remote.appr_kind_file_read', 'ファイル読み取り')],
  file_write: ['✏', () => T('remote.appr_kind_file_write', 'ファイル書き込み')],
  file_delete: ['\u{1F5D1}', () => T('remote.appr_kind_file_delete', 'ファイル削除')],
  shell_command: ['⌘', () => T('remote.appr_kind_shell_command', 'コマンド実行')],
  network_access: ['\u{1F310}', () => T('remote.appr_kind_network_access', 'ネットワーク接続')],
  git_operation: ['\u{1F33F}', () => T('remote.appr_kind_git_operation', 'git 操作')],
  package_install: ['\u{1F4E6}', () => T('remote.appr_kind_package_install', 'パッケージ導入')],
  privilege: ['\u{1F6E1}', () => T('remote.appr_kind_privilege', '管理者権限の昇格')],
  other: ['❓', () => T('remote.appr_kind_other', 'その他の承認')],
};
// 承認モードの選択肢。**いちばん危険な ⚡ 全自動を最後**に置く (親指の定位置から遠い)。
const APPR_MODES = [
  ['ask', () => T('remote.appr_mode_ask', '\u{1F6E1} 都度確認'), 'approval_ask'],
  ['agent', () => T('remote.appr_mode_agent', '\u{1F916} Agent優先'), 'approval_agent'],
  ['auto', () => T('remote.appr_mode_auto', '⚡ 全自動'), 'approval_auto'],
];

// `null` = まだ確かめていない / `true` = 使える / `false` = この PC の版には無い
let apprApi = null;
let apprItems = [];

// ─── 自分の DOM とスタイルは自分で作る (body.html / style.css を触らない) ───
(function apprInstall() {
  const css = document.createElement('style');
  css.textContent = [
    '#zv-appr { flex:1; min-height:0; display:none; flex-direction:column; }',
    '#zv-appr.show { display:flex; }',
    // 承認モードの帯。狭い端末では折り返す (どの幅でも見切れない)
    '#zv-appr .zv-am { flex:none; display:flex; flex-wrap:wrap; align-items:center;',
    '  gap:6px; padding:7px 10px; background:#161b22; border-bottom:1px solid #21262d;',
    '  font-size:12px; color:#8b949e; }',
    // 指の当たり判定は 44px 以上。padding だけで足すと文字を小さくした途端に
    // 割るので、min-height で床を打っておく
    '#zv-appr .zv-am .btn { min-height:44px; padding:6px 10px; font-size:12px; min-width:0;',
    '  display:inline-flex; align-items:center; justify-content:center;',
    '  overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }',
    '#zv-appr .zv-am .btn.act { background:#1f3a5f; border-color:#7ee1ff; color:#7ee1ff; }',
    '#zv-appr .zv-body { flex:1; overflow-y:auto; -webkit-overflow-scrolling:touch;',
    '  padding:8px 10px 12px; }',
    // 空状態は利用可能領域の中央に 1 枚 (下や上に取り残さない)
    '#zv-appr .zv-body.mid { display:flex; align-items:center; justify-content:center; padding:16px; }',
    '#zv-appr .zv-kind { margin-top:5px; font-size:12px; color:#c9d1d9; }',
    '#zv-appr .zv-why { margin:4px 0 0; font:11px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;',
    '  color:#8b949e; word-break:break-all; max-height:3em; overflow:hidden; }',
    '#zv-appr .zv-note { margin:0 0 8px; padding:9px 11px; border-radius:8px;',
    '  background:#3a2c12; border:1px solid #d29922; color:#f2dfb4; font-size:12px; line-height:1.65; }',
    // まとめて承認。中身が無いときは高さも取らない
    '#zv-appr .zv-foot { flex:none; display:none; gap:8px; padding:8px 10px;',
    '  background:#161b22; border-top:1px solid #21262d; }',
    '#zv-appr .zv-foot.show { display:flex; }',
    '#zv-appr .zv-foot .btn { flex:1; min-width:0; min-height:44px; padding:6px 8px; font-size:13px;',
    '  overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }',
    // 行内の 承認 / 拒否 / 常に許可。共有 CSS (.card .ax .btn) は 41px しかないので、
    // 自分の入れ物の中だけ 44px の床を足す (style.css は触らない)
    '#zv-appr .card .ax .btn { min-height:44px; padding:6px 4px; font-size:12.5px; }',
  ].join('\n');
  document.head.appendChild(css);
})();

const apprBox = document.createElement('div');
apprBox.id = 'zv-appr';
const apprModeRow = document.createElement('div');
apprModeRow.className = 'zv-am';
const apprBody = document.createElement('div');
apprBody.className = 'zv-body';
const apprFoot = document.createElement('div');
apprFoot.className = 'zv-foot';
apprBox.appendChild(apprModeRow);
apprBox.appendChild(apprBody);
apprBox.appendChild(apprFoot);
// 端末 (#scr) / 一覧 (#alist) と同じ場所を使う。上のセグメントと下のバーは
// 動かさないので、切り替えても画面が突然作り替わったようには見えない。
$('v-agent').insertBefore(apprBox, $('keys'));

// ─── データ ──────────────────────────────────────────────────────────

// 承認キューが無い版のための代替。判定 (`attention`) は **PC が付けた印**を
// そのまま使う — 画面文字から状態を作り直さない (設計原則 4)。
// 種別も根拠も無いので、行には「不明」と正直に出す。
function apprLegacyItems() {
  return ((state && state.agents) || [])
    .filter(a => a.running && a.attention)
    .map(a => ({id: a.id, agent: a.icon + ' ' + a.title, kind: '', detail: '', since: null, legacy: true}));
}
function apprPending() {
  return apprApi === false ? apprLegacyItems() : apprItems;
}
function apprCount() {
  return apprPending().length;
}
// 動いているエージェントが 1 体もいなければ承認要求は生まれない。
// 叩く理由が無いので叩かない (アイドルのコストはゼロ)。
function apprAnyRunning() {
  return ((state && state.agents) || []).some(a => a.running);
}

// `api()` は 404 も 500 も切断も全部 `throw 0` に畳んでしまうので、ここだけ
// 素の fetch で取る。**「この版には無い (404)」と「今たまたま失敗した」を
// 混ぜると、一度の瞬断で永久に代替表示へ落ちてしまう**。
async function apprFetch() {
  let r;
  try {
    r = await fetch('/api/approvals', {headers: {'X-Token': TOK}});
  } catch (e) {
    return false;   // 切断 — 判定は変えない (次の周回でまた試す)
  }
  if (r.status === 401) { toast(T('remote.auth_error', '認証エラー: QRコードを読み直してください')); return false; }
  if (r.status === 404 || r.status === 405 || r.status === 501) { apprApi = false; apprItems = []; return false; }
  if (!r.ok) return false;
  let j;
  try { j = await r.json(); } catch (e) { return false; }
  apprApi = true;
  apprItems = Array.isArray(j.items) ? j.items : [];
  return true;
}

// 1 件返す。**種別ごとの返し方を知っているのは PC 側**なので、スマホからは
// 当て推量の文字 (y / Enter) を送らない。
async function apprSend(it, act) {
  try {
    if (apprApi === false || it.legacy) {
      // 古い版: 行内の ✅ 承認 と同じ入口。拒否は受け付けないので送らない
      if (act !== 'approve') return false;
      const r = await api('/api/agent_act', {id: it.id, act: 'approve'});
      return !!(r && r.ok);
    }
    const r = await api('/api/approve', {id: it.id, act: act});
    return !!(r && r.ok);
  } catch (e) { return false; }
}

// ─── 描画 ────────────────────────────────────────────────────────────

function apprKind(k) {
  return APPR_KINDS[k] || null;
}
function apprAge(sec) {
  if (typeof sec !== 'number' || !isFinite(sec) || sec < 0) return '';
  if (sec < 60) return T('remote.appr_age_s', '{n} 秒待ち').replace('{n}', Math.floor(sec));
  if (sec < 3600) return T('remote.appr_age_m', '{n} 分待ち').replace('{n}', Math.floor(sec / 60));
  return T('remote.appr_age_h', '{n} 時間待ち').replace('{n}', Math.floor(sec / 3600));
}
function apprMode() {
  return (state && state.approval) || '';
}
function apprBtn(label, cls, fn) {
  const b = document.createElement('button');
  b.className = 'btn' + (cls ? ' ' + cls : '');
  b.textContent = label;
  b.onclick = e => { e.stopPropagation(); fn(); };
  return b;
}
// いまの承認モードを**必ず**見せる。切り替えもここから届く。
function apprRenderMode() {
  const el = apprModeRow;
  el.innerHTML = '';
  const cur = apprMode();
  const lab = document.createElement('span');
  lab.textContent = T('remote.appr_mode', '承認モード');
  el.appendChild(lab);
  APPR_MODES.forEach(([m, text, cmd]) => {
    el.appendChild(apprBtn(text(), cur === m ? 'act' : (m === 'auto' ? 'warn' : ''),
      () => apprSetMode(m, cmd, text())));
  });
}
// `⚡ 全自動` は「これから出る承認要求を全部通す」ので、必ず確認を挟む。
async function apprSetMode(m, cmd, label) {
  if (m === apprMode()) return;
  if (m === 'auto' && !confirm(T('remote.appr_auto_confirm',
    '⚡ 全自動 にすると、これから出る承認要求をすべて自動で通します。よろしいですか？'))) return;
  try {
    const r = await api('/api/cmd', {name: cmd, arg: 0});
    if (!r.ok) { toast(r.error || T('remote.failed', '失敗しました')); return; }
    toast(T('remote.appr_mode_set', '承認モードを {mode} にしました').replace('{mode}', label));
    await pollState();
    apprRender();
  } catch (e) {}
}
// 空状態は利用可能領域の中央に 1 枚のカードで出す (CLAUDE.md の UI 原則)。
function apprEmptyCard() {
  const d = document.createElement('div');
  d.className = 'mid-card';
  const b = document.createElement('span');
  b.className = 'big';
  b.textContent = '✅';
  d.appendChild(b);
  const t = document.createElement('span');
  t.textContent = T('remote.appr_empty', '待っている承認はありません');
  d.appendChild(t);
  return d;
}
// 1 行 = エージェント名 / 種別 / 対象 / 待ち時間 + その場で 承認・拒否。
// 文字列は全部 textContent で入れる (innerHTML に外部由来の文字を入れない)。
function apprCard(it) {
  const c = document.createElement('div');
  c.className = 'card';
  const hd = document.createElement('div');
  hd.className = 'hd';
  const kd = apprKind(it.kind);
  const ic = document.createElement('span');
  ic.textContent = kd ? kd[0] : '❓';
  hd.appendChild(ic);
  const nm = document.createElement('span');
  nm.className = 'nm';
  nm.textContent = it.agent || T('remote.appr_unknown', '不明');
  hd.appendChild(nm);
  const age = apprAge(it.since);
  if (age) {
    const st = document.createElement('span');
    st.className = 'st';
    st.textContent = age;
    hd.appendChild(st);
  }
  c.appendChild(hd);
  // 種別。表に無い ID は生のまま出す (知らないものを既知の種別へ丸めない)
  const kl = document.createElement('div');
  kl.className = 'zv-kind';
  kl.textContent = kd ? kd[1]() : (it.kind || T('remote.appr_unknown', '不明'));
  c.appendChild(kl);
  // 根拠 — どのファイル / どのコマンドか。無ければ「不明」と正直に出す
  const why = document.createElement('div');
  why.className = 'zv-why';
  why.textContent = T('remote.appr_why', '根拠') + ': '
    + (it.detail || T('remote.appr_unknown', '不明'));
  c.appendChild(why);
  const ax = document.createElement('div');
  ax.className = 'ax';
  ax.appendChild(apprBtn(T('remote.appr_approve', '✅ 承認'), 'pri', () => apprAct(it, 'approve')));
  // 古い版は拒否も常に許可も受け付けない。押しても届かないボタンは出さない
  if (!(apprApi === false || it.legacy)) {
    ax.appendChild(apprBtn(T('remote.appr_deny', '⛔ 拒否'), 'warn', () => apprAct(it, 'deny')));
    // 権限昇格には出さない — approvals.rs はどんなポリシーでも自動承認しないので、
    // ここに出すと「効かない約束」を見せることになる
    if (APPR_NO_AUTO.indexOf(it.kind) < 0) {
      ax.appendChild(apprBtn(T('remote.appr_always', '\u{267E} 常に許可'), '', () => apprAct(it, 'always')));
    }
  }
  c.appendChild(ax);
  return c;
}
// 承認は 1 タップ。**取り返しのつかない種別だけ**確認を挟む。
// 拒否は安全側 (fail-closed) なので確認しない — 危ないと思ったときに
// 押しづらいのは本末転倒。`always` は設定に残るので必ず確認する。
async function apprAct(it, act) {
  const kd = apprKind(it.kind);
  const kind = kd ? kd[1]() : (it.kind || T('remote.appr_unknown', '不明'));
  if (act === 'approve' && APPR_DANGER.indexOf(it.kind) >= 0) {
    if (!confirm(T('remote.appr_confirm_danger', '「{kind}」を承認します。取り消せません。\n{detail}')
      .replace('{kind}', kind)
      .replace('{detail}', it.detail || T('remote.appr_unknown', '不明')))) return;
  }
  // **何が残るのかを文面に書く。** この 1 タップだけは、押した後も効き続ける
  if (act === 'always') {
    if (!confirm(T('remote.appr_always_confirm',
      '「{kind}」を自動で承認する設定が PC 側 (config.toml) に残ります。\n' +
      'いまの要求: {agent} — {detail}\n\nPC で外すまで、この種別の確認は出なくなります。よろしいですか？')
      .replace('{kind}', kind)
      .replace('{agent}', it.agent || T('remote.appr_unknown', '不明'))
      .replace('{detail}', it.detail || T('remote.appr_unknown', '不明')))) return;
  }
  const ok = await apprSend(it, act);
  await apprRefresh();
  if (ok) {
    toast(act === 'deny'
      ? T('remote.appr_denied', '⛔ 拒否しました')
      : act === 'always'
        ? T('remote.appr_always_done', '\u{267E} 「{kind}」を常に許可にしました').replace('{kind}', kind)
        : T('remote.appr_approved', '✅ 承認しました'));
    return;
  }
  // 承認は PC のパネルや通知 (ntfy) のボタンからも返る。取り直して消えていたら
  // それは失敗ではなく **横で決着が付いた**ということ。嘘の「失敗」を出さない
  toast(apprPending().some(x => x.id === it.id)
    ? T('remote.appr_failed', '返せませんでした')
    : T('remote.appr_gone', 'すでに決着していました'));
}
// 「全部承認」は必ず確認し、**何件・どの種別に効くのかを先に見せる**。
// 件数だけでは「ファイル書き込み 3 件」と「コマンド実行 3 件」の区別が付かず、
// 押す前に危険度が分からない。管理者権限の昇格は 1 タップでは通さない
// (approvals.rs の `auto_approvable()` と同じ立場)。
async function apprBulk() {
  const all = apprPending();
  const rows = all.filter(it => APPR_NO_AUTO.indexOf(it.kind) < 0);
  const skipped = all.length - rows.length;
  if (!rows.length) { toast(T('remote.appr_bulk_none', 'まとめて承認できるものがありません')); return; }
  // 種別ごとの内訳。並びは出てきた順 (サーバが返した古い順) のまま
  const tally = [];
  rows.forEach(it => {
    const kd = apprKind(it.kind);
    const name = kd ? kd[1]() : (it.kind || T('remote.appr_unknown', '不明'));
    const hit = tally.find(t => t[0] === name);
    if (hit) hit[1]++; else tally.push([name, 1]);
  });
  let msg = T('remote.appr_bulk_confirm', '{n} 件をまとめて承認します。').replace('{n}', rows.length);
  tally.forEach(([name, n]) => {
    msg += '\n' + T('remote.appr_bulk_kind', '・{kind} {n} 件').replace('{kind}', name).replace('{n}', n);
  });
  if (skipped) {
    msg += '\n' + T('remote.appr_bulk_skip',
      '（「{kind}」の {m} 件は含めません — 1 件ずつ確かめてください）')
      .replace('{kind}', APPR_KINDS.privilege[1]()).replace('{m}', skipped);
  }
  msg += '\n\n' + T('remote.appr_bulk_ask', 'よろしいですか？');
  if (!confirm(msg)) return;
  let ok = 0;
  const missed = [];
  for (let i = 0; i < rows.length; i++) {
    if (await apprSend(rows[i], 'approve')) ok++; else missed.push(rows[i].id);
  }
  await apprRefresh();
  // 横 (PC / 通知) で決着した件を「失敗」に数えない
  const left = missed.filter(id => apprPending().some(x => x.id === id)).length;
  toast(T('remote.appr_bulk_done', '✅ {n} 件を承認しました').replace('{n}', ok)
    + (left ? T('remote.appr_bulk_left', '（{m} 件は返せませんでした）').replace('{m}', left) : ''));
}
function apprRender() {
  apprRenderMode();
  const el = apprBody;
  // 1.5 秒ごとに作り直すので、読んでいる位置を必ず戻す
  const keep = el.scrollTop;
  el.innerHTML = '';
  el.classList.remove('mid');
  const rows = apprPending();
  // 承認キューを持たない版であることは、黙らずに 1 行で伝える
  if (apprApi === false) {
    const n = document.createElement('div');
    n.className = 'zv-note';
    n.textContent = T('remote.appr_legacy',
      'この PC の版は承認キューに未対応です — 種別と根拠は出せません（承認のみ）');
    el.appendChild(n);
  }
  if (!rows.length) {
    if (apprApi !== false) el.classList.add('mid');
    el.appendChild(apprEmptyCard());
  } else {
    rows.forEach(it => el.appendChild(apprCard(it)));
  }
  el.scrollTop = keep;
  // 1 件しかないなら行内のボタンで足りる (同じ操作への到達経路を増やさない)
  const bulk = rows.length >= 2;
  apprFoot.classList.toggle('show', bulk);
  apprFoot.innerHTML = '';
  if (bulk) {
    apprFoot.appendChild(apprBtn(
      T('remote.appr_bulk', '✅ 全部承認 ({n})').replace('{n}', rows.length), 'pri', apprBulk));
  }
}
async function apprRefresh() {
  if (apprApi !== false) await apprFetch();
  apprRender();
  renderSeg();
}

// ─── ビューの出入り ──────────────────────────────────────────────────

function apprEnter() {
  if (aview === APPR) return;
  aview = APPR;
  // 端末と一覧が使っていた場所をそのまま借りる。上のセグメントと下のバーは
  // 動かさないので、レイアウトが激変しない
  $('scr').style.display = 'none';
  $('keys').style.display = 'none';
  $('alist').classList.remove('show');
  apprBox.classList.add('show');
  renderSeg();
  apprRender();
  pollTerm();   // 切り替えた瞬間に取りに行く (1 テンポ空白にしない)
}

// ─── 既存の関数を包む (共有ファイルを 1 バイトも触らないため) ─────────

// セグメントの 5 つ目。`aview` が 'appr' のときは AVIEWS のどれとも一致しない
// ので、元の renderSeg はどのボタンにも act を付けない。
const apprOrigRenderSeg = renderSeg;
renderSeg = function () {
  apprOrigRenderSeg();
  const el = $('aseg');
  if (!el) return;
  const b = document.createElement('button');
  if (aview === APPR) b.className = 'act';
  b.appendChild(document.createTextNode(T('remote.appr_view', '✅ 承認キュー')));
  const n = apprCount();
  if (n) {
    const s = document.createElement('span');
    s.className = 'badge';
    s.textContent = n;
    b.appendChild(s);
  }
  b.onclick = apprEnter;
  el.appendChild(b);
};

// 他のビューへ出るときは自分の入れ物を畳む。元の setAView の
// `if (aview === k) return;` は 'appr' → 他 では素通りするので触らない。
const apprOrigSetAView = setAView;
setAView = function (k) {
  if (aview === APPR && k !== APPR) apprBox.classList.remove('show');
  return apprOrigSetAView(k);
};

// ポーリングは **1 本のまま**。承認ビューを見ている間は /api/term も
// /api/agents も叩かず、/api/approvals だけを同じ間隔で取る。
const apprOrigPollTerm = pollTerm;
pollTerm = function () {
  if (view !== 'agent' || aview !== APPR) return apprOrigPollTerm();
  clearTimeout(termTimer);
  // 未対応の版では 1 回も叩かない。表示は pollState (2.5 秒) の相乗りで更新する
  if (apprApi === false) { apprRender(); return Promise.resolve(); }
  return apprFetch().then(() => {
    if (view !== 'agent' || aview !== APPR) return;   // 待っている間に離れた
    apprRender();
    renderSeg();
    termTimer = setTimeout(pollTerm, APPR_POLL_MS);
  });
};

// 件数バッジは既存の 2.5 秒に相乗りする (新しい間隔を増やさない)。
// 叩くのは **エージェントタブを開いていて / 承認ビューではなく (そちらが
// 自分で取る) / この版が承認キューを持っていて / 動いている体がいる**とき
// だけ。それ以外はネットワークもタイマーも 1 つも増えない。
const apprOrigPollState = pollState;
pollState = async function () {
  await apprOrigPollState();
  if (aview === APPR) { apprRender(); return; }
  if (view !== 'agent' || apprApi === false) return;
  if (!apprAnyRunning()) {
    // 誰も動いていないなら要求は生まれない。残っていた件数は落とす
    if (apprItems.length) { apprItems = []; renderSeg(); }
    return;
  }
  const before = apprCount();
  await apprFetch();
  if (apprCount() !== before) renderSeg();
};
