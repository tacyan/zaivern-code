async function pollState() {
  try {
    state = await api('/api/state');
    $('dot').classList.add('on');
    $('ws').textContent = state.workspace + (state.file ? ' — ' + state.file + (state.dirty ? ' ●' : '') : '');
    renderTabs(); renderAgents(); renderCmds(); renderSeg();
    if (curTab !== state.active) { curTab = state.active; if (!dirty) loadFile(); }
  } catch (e) { $('dot').classList.remove('on'); }
}
function renderTabs() {
  const el = $('tabs');
  el.innerHTML = '';
  (state.tabs || []).forEach((t, i) => {
    const c = document.createElement('button');
    c.className = 'chip' + (i === state.active ? ' act' : '');
    c.textContent = t.title + (t.dirty ? ' ●' : '');
    c.onclick = async () => { await api('/api/tab', {index:i}); dirty = false; await pollState(); };
    el.appendChild(c);
  });
}

// ─── エディタ ───
