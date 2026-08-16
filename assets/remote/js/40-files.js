async function loadFiles() {
  try {
    const r = await api('/api/files');
    files = r.files || [];
    renderFiles();
  } catch (e) {}
}
function renderFiles() {
  const q = $('filter').value.toLowerCase();
  const el = $('flist');
  el.innerHTML = '';
  const hit = files.filter(f => f.toLowerCase().includes(q)).slice(0, 400);
  if (!hit.length) {
    const e0 = document.createElement('div');
    e0.className = 'empty'; e0.textContent = T('remote.no_match', '該当なし');
    el.appendChild(e0); return;
  }
  hit.forEach(f => {
    const d = document.createElement('div');
    // Windows のパスは `src\app.rs` と区切りが `\` で来る。
    // `/` だけを見ていると全体がファイル名として出てフォルダ行が空になる
    const i = Math.max(f.lastIndexOf('/'), f.lastIndexOf('\\'));
    d.innerHTML = '<span></span><br><span class="dir"></span>';
    d.children[0].textContent = i >= 0 ? f.slice(i + 1) : f;
    d.children[2].textContent = i >= 0 ? f.slice(0, i) : '';
    d.onclick = async () => {
      await api('/api/open', {path: f});
      dirty = false;
      toast(T('remote.opened', '{path} を開きました').replace('{path}', f));
      document.querySelector('nav button[data-v=editor]').click();
      await pollState();
    };
    el.appendChild(d);
  });
}
$('filter').addEventListener('input', renderFiles);

// ─── エージェント ───
