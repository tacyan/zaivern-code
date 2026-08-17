// 'one'     … いま選んでいる 1 体 (既定)
// 'all'     … 起動中の全エージェント (PC 側の「📣 全エージェントへブロードキャスト」)
// 'stalled' … 止まっている (待機中の) ものだけ (PC 側の「止まっているものへまとめて送る」)
//
// 既定をいちばん狭い 'one' にするのは、画面を開いた瞬間が全員宛てだと
// 打ち込んだ 1 行がそのまま全機へ飛ぶため。
// 件数は **PC 側が数えた state.bulk をそのまま出す**。スマホ側でも数えると
// 数え方が 2 か所になり、「3 体と出ているのに 5 体へ飛んだ」が起こりうる。
let bulkMode = 'one';
function bulkCount(m) { return ((state && state.bulk) || {})[m || bulkMode] || 0; }
function bulkModeLabel(m) {
  return m === 'all' ? T('remote.bulk_all', '\u{1F4E3} 全員')
    : m === 'stalled' ? T('remote.bulk_stalled', '⏸ 待機')
    : T('remote.bulk_one', '\u{1F916} 選択中');
}
// 宛先行: 何体へ届くのかを送信前に必ず見せる + 一括停止をここから届かせる。
// エージェントが 0 体なら行ごと消す (中身の無い帯で高さを取らない)。
function renderBulk() {
  const el = $('btgt');
  el.innerHTML = '';
  const agents = (state && state.agents) || [];
  const n = bulkCount();
  el.classList.toggle('show', agents.length > 0);
  if (agents.length) {
    const lab = document.createElement('span');
    lab.className = 'grow';
    lab.textContent = T('remote.bulk_target', '宛先: {mode} — {n} 体')
      .replace('{mode}', bulkModeLabel(bulkMode)).replace('{n}', n);
    el.appendChild(lab);
    const stop = document.createElement('button');
    stop.className = 'btn warn';
    stop.textContent = T('remote.bulk_stop', '⏹ 停止');
    stop.title = T('remote.bulk_stop_title', '宛先へ Esc を送っていまの作業を止める');
    stop.disabled = n === 0;
    stop.onclick = bulkStop;
    el.appendChild(stop);
  }
  // 宛先 0 体では送れない。押しても届かないボタンは押させない
  $('tsend').disabled = n === 0;
  $('tput').disabled = n === 0;
}
// 一括停止 = 宛先へ Esc。セッションを殺すのは PC 側の確認モーダルに任せる
// (スマホから殺すと、誰も押せないダイアログが PC に開いたままになる)。
async function bulkStop() {
  if (!bulkCount()) return;
  try {
    const r = await api('/api/bulk_stop', {mode: bulkMode});
    toast(r.ok
      ? T('remote.bulk_stopped', '⏹ {n} 体へ停止 (Esc) を送りました').replace('{n}', r.sent)
      : (r.error || T('remote.bulk_failed', '送信できませんでした')));
  } catch (e) {}
}
// チップ (エージェント) を押したときの入口。**押した瞬間にその端末へ入る**。
//
// 以前は `agent_focus` の応答 → `pollState` → 次のポーリング (最大 1.5 秒後)
// を待って初めて画面が変わっていた。しかも履歴を遡っている / 選択している間は
// 追従が止まっていて次のポーリングが 1 本も無いので、**押しても永久に
// 切り替わらない**ことがあった (実際に「Codex を選んだのに Claude Code の
// 画面のまま」として報告された)。
//
// 宛先は手元で先に切り替える (`state.agent_active`)。`/api/scrollback` は
// エージェント番号を明示で受けるので、PC 側の応答を待たずに正しい端末が出る。
function selectAgent(i) {
  bulkMode = 'one';
  if (state) state.agent_active = i;
  renderAgents();
  setAView('term');   // 一覧・承認キューから押しても端末へ入る
  pollTerm();         // 追従を止めていても、ここで必ず取り直す
  api('/api/cmd', {name: 'agent_focus', arg: i}).then(pollState).catch(() => renderAgents());
}
function renderAgents() {
  const el = $('achips');
  el.innerHTML = '';
  const agents = state.agents || [];
  if (voiceAgent >= agents.length) stopVoice0();
  // 1 体以下なら「全員 / 待機」を選ぶ余地が無いので出さない (到達経路を増やさない)。
  // 減ったときは宛先を 1 体へ戻す — 消えた宛先のまま送らせない
  if (agents.length < 2 && bulkMode !== 'one') bulkMode = 'one';
  if (agents.length >= 2) {
    ['all', 'stalled'].forEach(m => {
      const c = document.createElement('button');
      c.className = 'chip' + (bulkMode === m ? ' act' : '');
      c.textContent = bulkModeLabel(m) + ' ' + bulkCount(m);
      c.onclick = () => { bulkMode = m; renderAgents(); };
      el.appendChild(c);
    });
  }
  agents.forEach((a, i) => {
    const c = document.createElement('button');
    c.className = 'chip' + (bulkMode === 'one' && i === state.agent_active ? ' act' : '');
    c.textContent = (a.running ? (a.attention ? '\u{1F514} ' : a.stalled ? '⏸ ' : '● ') : '○ ') + a.icon + ' ' + a.title;
    // 1 体を選んだら宛先も 1 体へ戻す (全員宛てのまま個別チップを押して誤爆しない)
    c.onclick = () => selectAgent(i);
    el.appendChild(c);
    const m = document.createElement('button');
    m.className = 'chip mic' + (i === voiceAgent ? ' rec' : '');
    m.textContent = i === voiceAgent ? T('remote.stop', '⏹ 停止') : '\u{1F3A4}';
    m.title = T('remote.mic_title', '{agent} へ音声入力').replace('{agent}', a.title);
    m.onclick = () => (i === voiceAgent ? stopVoice() : startVoice(i));
    el.appendChild(m);
  });
  const plus = document.createElement('button');
  plus.className = 'chip'; plus.textContent = T('remote.launch', '＋ 起動');
  plus.onclick = () => {
    const names = (state.presets || []).map((p, i) => i + ': ' + p.icon + ' ' + p.name).join('\n');
    const v = prompt(T('remote.launch_prompt', '起動するプリセット番号') + '\n' + names, '0');
    if (v !== null) api('/api/cmd', {name:'agent_launch', arg:parseInt(v) || 0}).then(pollState).catch(() => {});
  };
  el.appendChild(plus);
  renderBulk();
}
// ─── エージェントタブの中のビュー切替 ───────────────────────────
