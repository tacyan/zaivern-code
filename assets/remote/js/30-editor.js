async function loadFile() {
  try {
    const f = await api('/api/file');
    if (!f.ok) { $('ta').value = ''; $('meta').textContent = ''; taTab = -1; return; }
    $('ta').value = f.text;
    // 文字コードは UTF-8 以外のときだけ出す (PC 側のステータスバーと同じ扱い)。
    // 保存でどう書かれるかがスマホからも分かるようにするため。
    $('meta').textContent =
      f.title + '  ·  ' + f.lang + (f.encoding ? '  ·  ' + f.encoding : '');
    taTab = (f.index === undefined || f.index === null) ? -1 : f.index;
    dirty = false;
  } catch (e) {}
}
$('ta').addEventListener('input', () => { dirty = true; });
$('reload').onclick = () => { dirty = false; loadFile().then(() => toast(T('remote.reloaded', '再読込しました'))); };
$('save').onclick = async () => {
  try {
    // 適用+保存を 1 リクエストで原子的に行う。タブ不一致はサーバ側で拒否される
    const r = await api('/api/text', {text: $('ta').value, index: taTab, save: true});
    if (r.ok) {
      dirty = false;
      // 元の文字コードで表せない文字を足すと UTF-8 へ切り替わる。
      // 黙って変わると「他のツールで読めなくなった」原因が分からないので必ず伝える
      if (r.promoted) {
        toast(T('remote.encoding_promoted', '{enc} では表せない文字があるため UTF-8 で保存しました')
          .replace('{enc}', r.was));
        loadFile();
      } else {
        toast(T('remote.saved', 'PC 側で保存しました ✅'));
      }
    } else {
      toast(r.error || T('remote.save_failed', '保存に失敗しました'));
    }
  } catch (e) { toast(T('remote.save_failed', '保存に失敗しました')); }
};

// ─── ファイル ───
