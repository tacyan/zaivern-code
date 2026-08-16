const CMDS = [
  [T('remote.save', '\u{1F4BE} 保存'), 'save'],
  [T('remote.cmd_new', '\u{1F4C4} 新規ファイル'), 'new'],
  [T('remote.cmd_close_tab', '❌ タブを閉じる'), 'close_tab'],
  [T('remote.cmd_terminal', '\u{1F5A5} ターミナル'), 'terminal'],
  [T('remote.cmd_sidebar', '\u{1F4C1} サイドバー'), 'sidebar'],
  [T('remote.cmd_cockpit', '\u{1F39b} Cockpit'), 'cockpit'],
  [T('remote.cmd_zoom_in', '\u{1F50D} ズーム +'), 'zoom_in'],
  [T('remote.cmd_zoom_out', '\u{1F50D} ズーム −'), 'zoom_out'],
  [T('remote.cmd_zoom_reset', '\u{1F50D} ズーム 100%'), 'zoom_reset'],
  [T('remote.cmd_tree', '\u{1F332} ツリー更新'), 'tree'],
  [T('remote.cmd_approval_ask', '\u{1F6e1} 承認モード'), 'approval_ask'],
  [T('remote.cmd_approval_auto', '⚡ 全自動モード'), 'approval_auto'],
  [T('remote.cmd_approval_agent', '\u{1F916} Agent優先モード'), 'approval_agent'],
  [T('remote.cmd_permission_cycle', '\u{1F6e1} 権限切替(全Agent)'), 'permission_cycle'],
];
function renderCmds() {
  const el = $('cmds');
  if (el.childElementCount) return;
  CMDS.forEach(([label, name]) => {
    const b = document.createElement('button');
    b.className = 'btn' + (name === 'approval_auto' ? ' warn' : '');
    b.textContent = label;
    b.onclick = () => api('/api/cmd', {name: name, arg: 0})
      .then(r => toast(r.ok
        ? T('remote.cmd_done', '{label} を実行').replace('{label}', label)
        : (r.error || T('remote.failed', '失敗しました'))))
      .catch(() => {});
    el.appendChild(b);
  });
}

applyI18n();
renderSeg();
pollState();
setInterval(pollState, 2500);
