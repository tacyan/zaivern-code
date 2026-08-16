$('nav').addEventListener('click', e => {
  const b = e.target.closest('button'); if (!b) return;
  view = b.dataset.v;
  document.querySelectorAll('nav button').forEach(x => x.classList.toggle('act', x === b));
  document.querySelectorAll('.view').forEach(x => x.classList.toggle('act', x.id === 'v' + '-' + view));
  if (view === 'files' && !files.length) loadFiles();
  if (view === 'agent') pollTerm();
});

// ─── 状態ポーリング ───
