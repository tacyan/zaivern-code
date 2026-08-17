// マイクボタンでトグル。話した内容は下の入力欄に溜まっていくだけで、
// 自動送信はしない。送るのは [⤵ 入れる] か [送信] を押したときだけ。
// 無音で認識が切れてもモードが ON なら自動で録音を再開する。
// voiceFatal = 復帰不能なエラーで止めた印。これが立っている間は onend で再開しない
// (network 等で無限リスタートし、画面上は無反応のまま壊れるのを防ぐ)
let voiceAgent = -1, recog = null, lastInterim = '', voiceFatal = false;
function speechAPI() { return window.SpeechRecognition || window.webkitSpeechRecognition; }
// 音声認識が使えるかを事前判定する。使えない理由コードを返す:
//   'unsupported' … SpeechRecognition がそもそも無い
//   ''            … 少なくとも**試せる** (使えるかどうかは試して決める)
//
// **`isSecureContext` で先回りして諦めない。**
//
// 実測 (Chrome / `http://<LAN の IP>:<port>/`):
//   {"isSecureContext":false,"hasSpeechRecognition":true,"hasMediaDevices":false}
// つまり Chrome は平文でも `webkitSpeechRecognition` を**持っている**。
// ここで「http だから」と隠すと、**動く端末の利用者からも機能を取り上げる**。
//
// 逆に WebKit (iOS Safari) は IDL に `[SecureContext]` が付いているので
// 平文では**構造ごと存在しない** — `speechAPI()` が偽になり 'unsupported' へ
// 落ちる。つまり「存在するか」だけを見れば、両方のブラウザで正しく分かれる。
//
// 実際に動くかは `start()` の結果で決める (`onerror` の `not-allowed` /
// `service-not-allowed` で案内へ落とす)。**推測ではなく結果で判定する。**
function speechBlockReason() {
  if (!speechAPI()) return 'unsupported';
  return '';
}
// OS キーボードのディクテーション (Gboard の 🎤 / iOS 音声入力) への案内文。
// キーボード側の音声入力は https でなくても、ページ側の権限も要らずに使える。
// 原因と、いま何をすればいいかの両方を必ず書く。
function dictationHint(reason) {
  // 実際に待ち受けているポートをそのまま案内する (既定 8899 とは限らない)。
  // /voice の API はトークンを要るので、いま持っているものを付けて渡す
  const p = location.port || '8899';
  const u = 'http://127.0.0.1:' + p + '/voice' + (TOK ? '?t=' + encodeURIComponent(TOK) : '');
  const how = T('remote.dictation_how',
    'キーボードの \u{1F3A4} を押して、入力欄に話しかけてください（送信は手動 Enter）。'
    + 'PC からは {url} で連続認識が使えます。').replace('{url}', u);
  const why = reason === 'unsupported'
    ? T('remote.speech_unsupported', 'このブラウザは音声認識 (Web Speech API) に未対応です。')
    : reason === 'network'
    ? T('remote.speech_network', '音声認識サーバーに接続できませんでした（http 接続では利用できません）。')
    : T('remote.speech_insecure', 'この接続 (http) ではブラウザの音声認識が使えません。');
  // 「直す道がある」ことまで書く。ブラウザがマイクを渡さないのは端末や
  // ブラウザの都合ではなく**オリジンが http だから**である:
  //   MediaDevices も SpeechRecognition も IDL が [SecureContext] なので、
  //   http://<LAN の IP>:<port>/ では存在すらしない (= MediaRecorder で録って
  //   PC へ送る逃げ道も無い)。https のオリジンにすれば isSecureContext が真に
  //   なり、このページのまま連続認識が動く。
  const fix = reason === 'unsupported'
    ? ''
    : T('remote.speech_https_hint',
        'https で開けば、スマホでもこのページのまま音声認識が使えます'
        + '（例: tailscale serve で TLS を付ける）。');
  return why + how + fix;
}
// 案内を消したことを覚える鍵。消したら二度と出さない (🎤 の長押しで戻す)。
// localStorage はプライベートブラウズで例外を投げることがあるので必ず包む。
const VNOTE_OFF_KEY = 'zv_vnote_off';
function noteMuted() {
  try { return localStorage.getItem(VNOTE_OFF_KEY) === '1'; } catch (e) { return false; }
}
function setNoteMuted(on) {
  try { localStorage.setItem(VNOTE_OFF_KEY, on ? '1' : '0'); } catch (e) {}
}
// 案内は消せる。消したら見出しごと消えて高さを 1px も取らない
// (中身の無い帯を残さない)。閉じるボタンは指で押せる大きさ (CSS で 44px)。
function showNote(m) {
  const n = $('vnote');
  if (noteMuted()) { hideNote(); return; }
  n.textContent = '';
  const msg = document.createElement('div');
  msg.className = 'vnote-msg';
  msg.textContent = m;
  const x = document.createElement('button');
  x.type = 'button';
  x.className = 'vnote-x';
  x.textContent = T('remote.vnote_dismiss', '✕ 閉じる');
  const back = T('remote.vnote_restore_hint', '\u{1F3A4} を長押しすると、この案内をまた出せます');
  x.title = back;
  x.setAttribute('aria-label', back);
  x.onclick = ev => {
    ev.stopPropagation();
    setNoteMuted(true);
    hideNote();
    toast(T('remote.vnote_dismissed', '案内を消しました — \u{1F3A4} の長押しで戻せます'));
  };
  n.appendChild(msg);
  n.appendChild(x);
  n.classList.add('show');
}
function hideNote() { const n = $('vnote'); n.textContent = ''; n.classList.remove('show'); }
// 認識が使えないときの代替: 入力欄にフォーカスしてキーボード音声入力へ誘導する。
// 自動送信はしないので、話した内容は入力欄に残ったままになる。
function keyboardDictation(i, reason) {
  if (i >= 0) api('/api/cmd', {name:'agent_focus', arg:i}).then(pollState).catch(() => {});
  const t = $('ti');
  t.focus();
  try { t.setSelectionRange(t.value.length, t.value.length); } catch (e) {}
  t.placeholder = T('remote.dictation_placeholder', '\u{1F3A4} キーボードの音声入力で話しかけてください — 送信は手動');
  showNote(dictationHint(reason));
  toast(T('remote.dictation_toast', 'キーボードの \u{1F3A4} から入力してください'));
}
// 復帰不能なエラー。再開させずに止め、理由を消えない形で残す
function fatalVoiceStop(msg) {
  voiceFatal = true;
  stopVoice0();
  renderAgents();
  showNote(msg);
  toast(msg);
}
function stopVoice0() {
  voiceAgent = -1;
  const r = recog; recog = null;
  if (r) { r.onend = null; try { r.stop(); } catch (e) {} }
  if ($('ti').value === lastInterim) $('ti').value = '';
  lastInterim = '';
  $('ti').placeholder = T('remote.agent_input_placeholder', 'エージェントへ指示を送る…');
}
function stopVoice() { stopVoice0(); hideNote(); renderAgents(); toast(T('remote.voice_mode_off', '\u{1F3A4} 音声入力モード OFF')); }
function startVoice(i) {
  // 使えない端末では死んだエラーを出さず、キーボード音声入力へ逃がす
  const reason = speechBlockReason();
  if (reason) { stopVoice0(); renderAgents(); keyboardDictation(i, reason); return; }
  const C = speechAPI();
  stopVoice0();
  hideNote();
  voiceFatal = false;
  voiceAgent = i;
  api('/api/cmd', {name:'agent_focus', arg:i}).then(pollState).catch(() => {});
  const r = new C();
  recog = r;
  r.lang = speechLang();
  r.continuous = true;
  r.interimResults = true;
  r.onresult = ev => {
    let fin = '', interim = '';
    for (let k = ev.resultIndex; k < ev.results.length; k++) {
      const t = ev.results[k][0].transcript;
      if (ev.results[k].isFinal) fin += t; else interim += t;
    }
    // 途中経過は「入力欄の末尾に仮表示」。確定したらその場で本文に変わる
    const base = $('ti').value.endsWith(lastInterim) && lastInterim
      ? $('ti').value.slice(0, -lastInterim.length)
      : $('ti').value;
    fin = fin.trim();
    if (fin) {
      $('ti').value = (base + (base && !base.endsWith(' ') ? ' ' : '') + fin).trim();
      lastInterim = '';
    } else {
      $('ti').value = base + interim;
      lastInterim = interim;
    }
  };
  r.onerror = ev => {
    const e = ev.error;
    if (e === 'no-speech') return;              // 無音だけ: onend の自動再開に任せる
    if (e === 'not-allowed' || e === 'service-not-allowed') {
      // 平文で断られたのなら、原因は「ブラウザの設定」ではなく**オリジンが
      // http だから**。設定を見に行かせても直らないので、案内のほうへ落とす
      // (https にすればこのページのまま動く、という直し方まで書いてある)。
      if (!window.isSecureContext) {
        voiceFatal = true;
        stopVoice0(); renderAgents();
        keyboardDictation(i, 'insecure');
        return;
      }
      fatalVoiceStop(T('remote.mic_not_allowed', 'マイクが許可されていません（ブラウザ設定を確認）'));
    } else if (e === 'network') {
      // 認識サーバーへ到達できない = http 経由ではほぼ復帰しない。案内して終わる
      voiceFatal = true;
      stopVoice0(); renderAgents();
      keyboardDictation(i, 'network');
    } else if (e === 'audio-capture') {
      fatalVoiceStop(T('remote.mic_not_found', 'マイクが見つかりません'));
    } else if (e === 'aborted') {
      stopVoice0(); renderAgents();            // 明示停止・画面遷移。黙って終わる
    }
  };
  r.onend = () => {
    if (voiceFatal) return;                    // 致命的エラー後は再開しない
    if (recog === r && voiceAgent === i) {
      try { r.start(); } catch (e) { stopVoice(); }
    }
  };
  try { r.start(); } catch (e) { toast(T('remote.voice_start_failed', '音声入力を開始できません')); stopVoice0(); renderAgents(); return; }
  $('ti').placeholder = T('remote.voice_placeholder', '\u{1F3A4} 話した内容がここに溜まります — 送信はボタンで');
  renderAgents();
  const a = (state.agents || [])[i];
  toast(T('remote.voice_mode_on', '\u{1F3A4} 音声入力モード ON → {agent} (自動送信はしません)')
    .replace('{agent}', a ? a.title : ''));
}
// ─── 消した案内を戻す (🎤 の長押し) ───
// 🎤 のボタンを描いているのは別のファイルなので、こちらからは document 側で
// **捕捉フェーズ**に拾う (60-bulk.js が onclick を直に代入しているため、
// 長押しの後に続くクリックはここで止めないと「押した」ことになってしまう)。
let micHoldTimer = 0, micHoldFired = false, micHoldX = 0, micHoldY = 0;
function micChipOf(t) { return t && t.closest ? t.closest('.chip.mic') : null; }
function cancelMicHold() { clearTimeout(micHoldTimer); micHoldTimer = 0; }
document.addEventListener('pointerdown', ev => {
  if (!micChipOf(ev.target)) return;
  micHoldFired = false;
  micHoldX = ev.clientX; micHoldY = ev.clientY;
  cancelMicHold();
  micHoldTimer = setTimeout(() => {
    micHoldFired = true;
    setNoteMuted(false);
    if (navigator.vibrate) { try { navigator.vibrate(15); } catch (e) {} }
    const reason = speechBlockReason();
    // 「使えます」と言い切ってよいのはセキュアコンテキストのときだけ。
    // 平文でも API は在る (Chrome) が、断られることもある — 押せば分かるので
    // ここでは**約束せず**、原因と直し方の案内を出しておく。
    if (reason) showNote(dictationHint(reason));
    else if (!window.isSecureContext) showNote(dictationHint('insecure'));
    else toast(T('remote.voice_available',
      'この接続では音声認識が使えます — \u{1F3A4} を押してください'));
  }, 600);
}, true);
document.addEventListener('pointermove', ev => {
  if (!micHoldTimer) return;
  // 指がずれたらスクロールなので長押しにしない
  if (Math.abs(ev.clientX - micHoldX) > 10 || Math.abs(ev.clientY - micHoldY) > 10) cancelMicHold();
}, true);
['pointerup', 'pointercancel', 'scroll'].forEach(e =>
  document.addEventListener(e, cancelMicHold, true));
document.addEventListener('click', ev => {
  if (!micHoldFired) return;
  micHoldFired = false;
  if (!micChipOf(ev.target)) return;
  ev.stopPropagation();
  ev.preventDefault();
}, true);

// ─── 一括操作 (宛先の粒度) ───
