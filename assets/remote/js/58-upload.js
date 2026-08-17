// ─── 添付 (画像などを宛先のエージェントへ送る) ──────────────────────
//
// CLI エージェントは画像を**パスで**受け取る。だから「スマホから送る」は
//   (a) PC 側の作業フォルダの下へ保存し
//   (b) その `@パス` をエージェントの入力欄へ入れる
// の 2 段になる。(a) は `/api/upload`、(b) は**既存の**一括送信
// (`/api/bulk` + `bulkMode`) をそのまま使う — 宛先の概念をここで増やさない。
//
// このファイルは `assets/remote/js/` へ置くだけで画面に増える (build.rs が
// ファイル名順に連結する)。他の JS は 1 バイトも触っていない。

// 送信中フラグ。二重に押されると同じ添付が 2 回入る
let upBusy = false;

// File → base64 (data: URI の本体だけ)。読めなければ reject。
function upB64(f) {
  return new Promise((res, rej) => {
    const r = new FileReader();
    r.onload = () => {
      const s = String(r.result || '');
      const i = s.indexOf(',');
      if (i < 0) rej(0); else res(s.slice(i + 1));
    };
    r.onerror = () => rej(0);
    r.readAsDataURL(f);
  });
}

// MB 表記 (小数第 1 位まで)。上限は PC 側が state で配る値だけを使う。
function upMB(n) { return Math.round(n / (1024 * 1024) * 10) / 10; }

// 選んだファイルを 1 件ずつ送る。**失敗は必ず理由を出す** (黙って落とさない)。
async function upSend(list, btn, label) {
  if (!list.length || upBusy) return;
  // 宛先 0 体では送れない。件数は PC が数えた値 (`bulkCount`) をそのまま見る
  if (!bulkCount()) { toast(T('remote.attach_no_target', '添付を送れる宛先がいません')); return; }
  upBusy = true;
  btn.disabled = true;
  let ok = 0;
  for (let i = 0; i < list.length; i++) {
    const f = list[i];
    // 進捗: 何件目を送っているかをボタン自身に出す
    btn.textContent = (list.length > 1 ? (i + 1) + '/' + list.length : '…');
    const max = (state && state.upload_max) || 0;
    if (max && f.size > max) {
      toast(T('remote.attach_too_big', '{name} は大きすぎます ({mb}MB まで)')
        .replace('{name}', f.name).replace('{mb}', upMB(max)));
      continue;
    }
    let r;
    try {
      r = await api('/api/upload', {name: f.name, data: await upB64(f)});
    } catch (e) {
      toast(T('remote.attach_failed', '{name} を送れませんでした').replace('{name}', f.name));
      continue;
    }
    if (!r || !r.ok) {
      toast((r && r.error) || T('remote.attach_failed', '{name} を送れませんでした').replace('{name}', f.name));
      continue;
    }
    // (b) 入力欄へ入れる。**既存の一括送信へそのまま合流**させる。
    // submit=false = 入れるだけ — 何をしてほしいかは人が書いて送る。
    try {
      await api('/api/bulk', {text: r.text, mode: bulkMode, submit: false});
    } catch (e) {
      toast(T('remote.attach_put_failed', '{name} を入力欄へ入れられませんでした').replace('{name}', f.name));
      continue;
    }
    ok++;
  }
  btn.textContent = label;
  upBusy = false;
  btn.disabled = $('tsend').disabled;
  if (ok) {
    toast(T('remote.attach_done', '\u{1F4CE} {n} 件を宛先の入力欄へ入れました').replace('{n}', ok));
    // 入れたあとは指示を書く場所へ誘導する (何をしてほしいかは人が書く)
    $('ti').focus();
  }
}

// 入力欄 (#ti) の**左**へボタンを 1 つ生やす。HTML は他の担当が触っている
// ので、差し込みはここから行う (共有ファイルを 1 バイトも触らない)。
(function upInit() {
  const ti = $('ti');
  if (!ti || !ti.parentNode) return;
  const pick = document.createElement('input');
  pick.type = 'file';
  pick.id = 'tupf';
  pick.multiple = true;
  // **accept は付けない。** `image/*` を付けると iOS は「写真ライブラリ」だけに
  // 絞られてカメラもファイルも選べなくなる。無指定なら 3 つとも出るうえ、
  // ログや PDF もそのまま渡せる (「画像等のファイル」を狭めない)。
  const btn = document.createElement('button');
  btn.className = 'btn';
  btn.id = 'tup';
  const label = '\u{1F4CE}';
  btn.textContent = label;
  btn.title = T('remote.attach_title', '画像などのファイルを宛先のエージェントへ送る');
  btn.setAttribute('aria-label', btn.title);
  btn.onclick = () => { if (!upBusy) pick.click(); };
  pick.onchange = () => {
    const fs = Array.prototype.slice.call(pick.files || []);
    pick.value = '';   // 同じファイルをもう一度選べるようにする
    upSend(fs, btn, label);
  };
  ti.parentNode.insertBefore(btn, ti);
  ti.parentNode.insertBefore(pick, ti);
  // 宛先 0 体で押せないのは送信ボタンと同じ。**数え直さず** renderBulk が
  // #tsend へ付けた disabled をそのまま写す (数え方を 2 か所に増やさない)。
  const mirror = () => { btn.disabled = upBusy || $('tsend').disabled; };
  new MutationObserver(mirror).observe($('tsend'), {attributes: true, attributeFilter: ['disabled']});
  mirror();
})();
