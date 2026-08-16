// 文言はサーバが <head> の window.ZVI18N へ 1 回だけ注入する。
// 第 2 引数 d は日本語の原文フォールバック — 辞書が届かなくても画面は壊れない。
const T = (k, d) => (window.ZVI18N && window.ZVI18N[k]) || d;
// 静的な文言は HTML 側に data-i18n 属性で宣言しておき、起動時に一括で差し込む。
// 差し込み先は属性ごとに分ける (本文 / placeholder / title)。
// 訳が無いときは HTML に書いてある原文をそのまま残す (上書きしない)。
function applyI18n() {
  document.querySelectorAll('[data-i18n]').forEach(el => {
    const v = T(el.dataset.i18n, ''); if (v) el.textContent = v;
  });
  document.querySelectorAll('[data-i18n-ph]').forEach(el => {
    const v = T(el.dataset.i18nPh, ''); if (v) el.placeholder = v;
  });
  document.querySelectorAll('[data-i18n-title]').forEach(el => {
    const v = T(el.dataset.i18nTitle, ''); if (v) el.title = v;
  });
}
// 音声認識の言語は画面の言語に合わせる (英語 UI なのに日本語を聞き取ろうとしない)。
// ja だけは地域つきの ja-JP が明確に良いので特別扱いする。
function speechLang() {
  const l = document.documentElement.lang || 'ja';
  return l === 'ja' ? 'ja-JP' : l;
}
let view = 'editor', dirty = false, files = [], state = null, curTab = -1;
let taTab = -1;  // textarea の内容がどのタブのものか (誤上書き防止)

function toast(m) {
  const t = $('toast'); t.textContent = m; t.classList.add('show');
  clearTimeout(t._h); t._h = setTimeout(() => t.classList.remove('show'), 1800);
}
async function api(path, body) {
  const opt = body
    ? { method:'POST', headers:{'Content-Type':'application/json','X-Token':TOK}, body:JSON.stringify(body) }
    : { headers:{'X-Token':TOK} };
  const r = await fetch(path, opt);
  if (r.status === 401) { toast(T('remote.auth_error', '認証エラー: QRコードを読み直してください')); throw 0; }
  if (!r.ok) throw 0;
  return r.json();
}

// ─── ビュー切替 ───
