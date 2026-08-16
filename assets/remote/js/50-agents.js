const ESC = '\u001b';
const KEYS = [
  ['Enter', '\r'], ['Esc', ESC], ['^C', '\u0003'],
  ['↑', ESC + '[A'], ['↓', ESC + '[B'],
  ['Tab', '\t'], [T('remote.key_shift_tab_perm', '⇧Tab 権限'), ESC + '[Z'],
  ['1', '1'], ['2', '2'], ['3', '3'], ['y', 'y'],
];
KEYS.forEach(([label, seq]) => {
  const b = document.createElement('button');
  b.className = 'key'; b.textContent = label;
  b.onclick = () => api('/api/term', {text: seq, raw: true}).catch(() => {});
  $('keys').appendChild(b);
});
// ─── 音声入力モード (エージェント毎) ───
