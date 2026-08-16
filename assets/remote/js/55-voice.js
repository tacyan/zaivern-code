// マイクボタンでトグル。話した内容は下の入力欄に溜まっていくだけで、
// 自動送信はしない。送るのは [⤵ 入れる] か [送信] を押したときだけ。
// 無音で認識が切れてもモードが ON なら自動で録音を再開する。
// voiceFatal = 復帰不能なエラーで止めた印。これが立っている間は onend で再開しない
// (network 等で無限リスタートし、画面上は無反応のまま壊れるのを防ぐ)
let voiceAgent = -1, recog = null, lastInterim = '', voiceFatal = false;
function speechAPI() { return window.SpeechRecognition || window.webkitSpeechRecognition; }
// 音声認識が使えるかを事前判定する。使えない理由コードを返す:
//   'insecure'    … http 接続 = セキュアコンテキストでない (スマホから見る場合はこれ)
//   'unsupported' … SpeechRecognition が無い (iOS Safari / Firefox など)
//   ''            … 使える
function speechBlockReason() {
  if (!window.isSecureContext) return 'insecure';
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
  return why + how;
}
function showNote(m) { const n = $('vnote'); n.textContent = m; n.classList.add('show'); }
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
// ─── 一括操作 (宛先の粒度) ───
