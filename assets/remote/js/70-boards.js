// 下部ナビ (エディタ/ファイル/エージェント/コマンド) は増やさず、この中で切り替える。
//   'term'   … 端末 (従来どおり)
//   'wait'   … 人の手が要るもの (返事待ち・承認・停滞) だけを縦に並べる
//   'deck'   … PC のデッキ相当。スマホなので 1 列固定 (横スクロールを作らない)
//   'kanban' … PC の看板相当。レーンは横に並べず「見出し + カード」の縦積み
// レーンの定義・状態ラベルは **PC 側 (kanban.rs) が決めたものをそのまま出す**。
// スマホ側で画面文字から状態を決め直さない (設計原則 4)。
let aview = 'term', alist = [], alanes = [], laneOpen = {};
const AVIEWS = [
  ['term', () => T('remote.view_term', '\u{1F5A5} 端末')],
  ['wait', () => T('remote.view_wait', '⏳ 待ち')],
  ['deck', () => T('remote.view_deck', '\u{1F0CF} デッキ')],
  ['kanban', () => T('remote.view_kanban', '\u{1F4CB} 看板')],
];
// 待ち件数は **PC が数えた state.waiting** をそのまま出す。一覧を開いていない
// 間も /api/state だけで最新になるので、バッジのために余分に叩かない。
function waitCount() { return (state && state.waiting) || 0; }
function renderSeg() {
  const el = $('aseg');
  el.innerHTML = '';
  AVIEWS.forEach(([k, lab]) => {
    const b = document.createElement('button');
    if (aview === k) b.className = 'act';
    b.appendChild(document.createTextNode(lab()));
    const n = k === 'wait' ? waitCount() : 0;
    if (n) {
      const s = document.createElement('span');
      s.className = 'badge'; s.textContent = n;
      b.appendChild(s);
    }
    b.onclick = () => setAView(k);
    el.appendChild(b);
  });
}
function setAView(k) {
  if (aview === k) return;
  aview = k;
  const term = k === 'term';
  // 端末と一覧は同じ場所を使う。切り替えで**上下のバーは動かさない**ので、
  // 画面が突然作り替わったようには見えない
  $('scr').style.display = term ? '' : 'none';
  $('keys').style.display = term ? '' : 'none';
  $('alist').classList.toggle('show', !term);
  renderSeg();
  renderList();
  pollTerm();   // 切り替えた瞬間に取りに行く (1 テンポ空白にしない)
}
function laneIcon(i) { const L = alanes.find(x => x.i === i); return L ? L.icon : ''; }
// 空状態は利用可能領域の中央に 1 枚のカードで出す (CLAUDE.md の UI 原則)
function emptyCard(k) {
  const d = document.createElement('div');
  d.className = 'mid-card';
  const b = document.createElement('span');
  b.className = 'big';
  b.textContent = alist.length === 0 ? '\u{1F916}' : (k === 'wait' ? '✅' : '\u{1F4A4}');
  d.appendChild(b);
  const t = document.createElement('span');
  t.textContent = alist.length === 0
    ? T('remote.no_agents_hint', 'エージェントがいません — ＋ 起動 から始められます')
    : (k === 'wait'
      ? T('remote.wait_empty', '待っているエージェントはいません — 全員動いています')
      : T('remote.list_empty', '表示できるエージェントがいません'));
  d.appendChild(t);
  return d;
}
function cardBtn(label, cls, fn) {
  const b = document.createElement('button');
  b.className = 'btn' + (cls ? ' ' + cls : '');
  b.textContent = label;
  b.onclick = e => { e.stopPropagation(); fn(); };
  return b;
}
// 1 枚のカード: 名前 / 状態 / 直近出力の末尾 2 行 / 経過時間 + その場の操作。
// 3 つのビュー (待ち・デッキ・看板) で同じカードを使う — 見た目を作り分けない。
function agentCard(a) {
  const c = document.createElement('div');
  c.className = 'card' + (a.active ? ' act' : '');
  const hd = document.createElement('div'); hd.className = 'hd';
  const ic = document.createElement('span'); ic.textContent = a.icon; hd.appendChild(ic);
  const nm = document.createElement('span'); nm.className = 'nm';
  nm.textContent = (a.running ? (a.attention ? '\u{1F514} ' : a.unread ? '● ' : '') : '○ ') + a.title;
  hd.appendChild(nm);
  const st = document.createElement('span'); st.className = 'st';
  st.textContent = laneIcon(a.lane) + ' ' + a.state + (a.running ? ' · ' + a.uptime : '');
  hd.appendChild(st);
  c.appendChild(hd);
  if (a.preview) {
    const p = document.createElement('pre'); p.className = 'pv'; p.textContent = a.preview;
    c.appendChild(p);
  }
  const ax = document.createElement('div'); ax.className = 'ax';
  // 承認は「人の手が要る」ときだけ出す (いつも並ぶ押せないボタンを作らない)
  if (a.attention) ax.appendChild(cardBtn(T('remote.card_approve', '✅ 承認'), 'pri', () => agentAct(a, 'approve')));
  if (a.running) ax.appendChild(cardBtn(T('remote.card_send', '✏ 指示'), '', () => openAgent(a, true)));
  if (a.running) ax.appendChild(cardBtn(T('remote.card_stop', '⏹ 停止'), 'warn', () => agentAct(a, 'stop')));
  if (ax.childNodes.length) c.appendChild(ax);
  // タップ = そのエージェントへ入る
  c.onclick = () => openAgent(a, false);
  return c;
}
// カードをタップ = 選んで端末へ入る。[✏ 指示] は一覧に留まったまま宛先だけ移す
// (「一覧で見つけて、その場で 1 行送る」を 1 タップで終わらせる)
function openAgent(a, stay) {
  bulkMode = 'one';
  api('/api/cmd', {name:'agent_focus', arg:a.idx}).then(pollState).catch(() => renderAgents());
  if (stay) { $('ti').focus(); } else { setAView('term'); }
}
// 行内の操作。承認キーは PC 側 (エージェントのカタログ) が知っているので、
// スマホから当て推量の文字を送らない
async function agentAct(a, act) {
  try {
    const r = await api('/api/agent_act', {id: a.id, act: act});
    if (!r.ok) { toast(r.error || T('remote.bulk_failed', '送信できませんでした')); return; }
    toast((act === 'approve'
      ? T('remote.approved', '✅ {agent} を承認しました')
      : T('remote.stopped_one', '⏹ {agent} を止めました')).replace('{agent}', a.title));
    pollTerm();
  } catch (e) {}
}
function renderList() {
  if (aview === 'term') return;
  const el = $('alist');
  // 1.5 秒ごとに作り直すので、読んでいる位置を必ず戻す
  // (戻さないと、スクロールした瞬間に毎回先頭へ跳ね上がる)
  const keep = el.scrollTop;
  el.innerHTML = '';
  el.classList.remove('mid');
  // 「待ち」は PC 側の判定 (remote::is_waiting_lane) で印が付いたものだけ
  const rows = aview === 'wait' ? alist.filter(a => a.waiting) : alist;
  if (!rows.length) { el.classList.add('mid'); el.appendChild(emptyCard(aview)); return; }
  if (aview === 'kanban') renderKanban(el, rows);
  else rows.forEach(a => el.appendChild(agentCard(a)));
  el.scrollTop = keep;
}
// 看板: レーン見出し + その下にカードの縦積み。空のレーンは見出しごと出さない
// (常に 0 と出る見出しを 8 本並べない)。見出しをタップで畳める。
function renderKanban(el, rows) {
  let shown = 0;
  alanes.forEach(L => {
    const mem = rows.filter(a => a.lane === L.i);
    if (!mem.length) return;
    shown += mem.length;
    const open = laneOpen[L.i] !== false;
    const box = document.createElement('div'); box.className = 'lane';
    const hd = document.createElement('button'); hd.className = 'lhd';
    const car = document.createElement('span'); car.textContent = open ? '▾' : '▸';
    hd.appendChild(car);
    const t = document.createElement('span'); t.textContent = L.icon + ' ' + L.title;
    hd.appendChild(t);
    const n = document.createElement('span'); n.className = 'n'; n.textContent = mem.length;
    hd.appendChild(n);
    hd.onclick = () => { laneOpen[L.i] = !open; renderList(); };
    box.appendChild(hd);
    if (open) {
      const body = document.createElement('div'); body.className = 'body';
      mem.forEach(a => body.appendChild(agentCard(a)));
      box.appendChild(body);
    }
    el.appendChild(box);
  });
  if (!shown) { el.classList.add('mid'); el.appendChild(emptyCard('kanban')); }
}
let termTimer = null;
// ポーリングは**ビューが増えても 1 本のまま**。端末を見ているときは /api/term、
// 一覧を見ているときは /api/agents を同じ間隔で叩く (合計回数は増えない)。
// 見ていないビューのために PTY を読ませない。
async function pollTerm() {
  clearTimeout(termTimer);
  if (view !== 'agent') return;
  try {
    if (aview === 'term') {
      const r = await api('/api/term');
      const el = $('scr');
      if (r.ok) {
        const stick = el.scrollTop + el.clientHeight >= el.scrollHeight - 24;
        el.classList.remove('empty');
        el.textContent = r.text;
        if (stick) el.scrollTop = el.scrollHeight;
      } else {
        el.classList.add('empty');
        el.textContent = T('remote.no_agents_hint', 'エージェントがいません — ＋ 起動 から始められます');
      }
    } else {
      const r = await api('/api/agents');
      if (r.ok) { alist = r.agents || []; alanes = r.lanes || []; renderList(); }
    }
  } catch (e) {}
  termTimer = setTimeout(pollTerm, 1500);
}
// 送信 = テキスト + Enter。入れる = テキストのみ (PC 側で内容を見て Enter)
// 宛先は bulkMode が決める。1 体宛て / 全員 / 待機だけ のどれも同じ入口を通る
// ので、「1 体には届くのに一括だけ挙動が違う」が起きない。
async function sendInput(submit) {
  const v = $('ti').value.trim();
  if (!v) return;
  if (!bulkCount()) { toast(T('remote.bulk_none', '送れる宛先がいません')); return; }
  if (bulkMode === 'one' && voiceAgent >= 0) {
    // 音声モード中は、選んだエージェントへ確実に届くようフォーカスし直す
    await api('/api/cmd', {name:'agent_focus', arg:voiceAgent}).catch(() => {});
  }
  let r = null;
  try { r = await api('/api/bulk', {text: v, mode: bulkMode, submit: submit}); } catch (e) { return; }
  if (!r.ok) { toast(r.error || T('remote.bulk_failed', '送信できませんでした')); return; }
  $('ti').value = ''; lastInterim = '';
  if (bulkMode === 'one') {
    toast(submit
      ? T('remote.sent', '送信しました')
      : T('remote.put_done', 'PC の入力欄に入れました (Enter で送信)'));
  } else {
    toast((submit
      ? T('remote.bulk_sent', '\u{1F4E3} {n} 体へ送信しました')
      : T('remote.bulk_put', '\u{1F4E3} {n} 体の入力欄に入れました')).replace('{n}', r.sent));
  }
}
$('tsend').onclick = () => sendInput(true);
$('tput').onclick = () => sendInput(false);
$('ti').addEventListener('keydown', e => { if (e.key === 'Enter') sendInput(true); });

// ─── コマンド ───
