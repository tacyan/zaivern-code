// ─── 端末を「本物の端末」にする ─────────────────────────────────────
//
// VS Code for the Web はブラウザ内でターミナルを **1 つも出せない**。
// こちらは PC 側に本物の PTY があるので、スマホはその窓になれる。
// この 1 ファイルで足すのは 5 つ:
//
//   1. 上へ遡れる (`/api/scrollback` の無限スクロール。継ぎ足しても**読んでいた行が動かない**)
//   2. 色が出る   (ANSI は**ブラウザに解釈させない**。構造で受けて textContent で入れる)
//   3. 端末内検索 (強調 + 前/次 + 件数)
//   4. 選択とコピー (長押し選択。選択を始めたら追従を止める)
//   5. 折り返しの切替 (長い行を折り返す / 横スクロール)
//
// ## 触るファイルはこれ 1 つ
// `build.rs` が `assets/remote/js/*.js` をファイル名順に連結するので、
// このファイルを置くだけで画面が増える。既存の端末ビュー (`70-boards.js`) は
// **1 バイトも編集しない** — `pollTerm` / `setAView` を実行時に包んで拡張する。
// 番号が 45 なので**このファイルの先頭コードは 70-boards.js より先に走る**。
// 関数宣言は巻き上げられるので包むのは安全だが、`let aview` 等はまだ TDZ なので
// **初期化コードから読まないこと** (関数の中で、呼ばれる時に読む)。
//
// ## 落ち方
// `/api/scrollback` はサーバ側がまだ無いことがある (並行実装中)。404 / 405 /
// 400 / 501 / JSON でない応答のどれでも `sbOff` を立てて、**いまの `/api/term`
// の挙動へ静かに落ちる**。落ちた側でも描画はこちらが持つので、検索・コピー・
// 折り返しはそのまま使える (遡れないのは「無いものは遡れない」ため)。
//
// ## アイドルの費用はゼロ (設計原則 3)
// * 端末ビューを見ていない間はタイマーを 1 本も持たない
// * **遡っている間は追いかけの取得を止める** (帯域も電池も使わない)。
//   下端へ戻った瞬間 (スクロール / [⬇ 最新]) に再開する
// * ポーリングは既存と同じ 1 本 (`termTimer`) を使い回す — 合計回数を増やさない
(function () {
  try {
    const SCR = $('scr');
    const SEC = document.getElementById('v-agent');
    if (!SCR || !SEC || typeof pollTerm !== 'function') return;

    // ── 定数 (画面サイズや行数を決め打ちしない値はここに集める) ──
    const TAIL = 120;      // 追従で取り直す末尾の行数
    const CHUNK = 300;     // 上へ遡るときの 1 塊
    const MAX_ROWS = 3000; // 端末に載せる最大行数 (DOM を無限に伸ばさない)
    const POLL_MS = 1500;  // 追従の間隔 (既存の /api/term と同じ)
    const NEAR_BOTTOM = 24;
    const NEAR_TOP = 8;
    const FIND_WAIT = 180; // 入力が止まってから検索するまで
    // style 属性へ入れてよいのは #rrggbb だけ。サーバの値をそのまま流すと
    // `red;background:url(...)` のような細工で CSS を書き足せてしまう
    const HEX = /^#[0-9a-fA-F]{3,8}$/;
    const WRAP_KEY = 'zv_term_wrap';

    // ── 状態 ──
    let buf = [];        // [{spans, txt, low, sig}] — 読み込み済みの行
    let bufFrom = 0;     // buf[0] の絶対行番号
    let total = 0;       // サーバが持っている全行数
    let follow = true;   // 下端に居る = 出力を追いかける
    let sbOff = false;   // /api/scrollback が無いサーバ
    let loading = false; // 上への継ぎ足し中
    let noTop = false;   // これ以上前は無い
    let curAgent = -2;   // 表示中のエージェント (変わったら作り直す)
    let findOn = false, findQ = '', findTimer = null;
    // 一致はいまの buf 内の添字で持つが、**いま選んでいる 1 件だけは絶対行で覚える**。
    // 上へ継ぎ足すと添字が全部ずれるので、添字のまま持つと選択位置が飛ぶ
    let hits = [], byRow = new Map(), cur = 0, curAbs = null;
    let plainSig = '';   // /api/term 落ちのときの差分検出

    // ── 自分の CSS は自分で足す (style.css は編集しない) ──
    const css = document.createElement('style');
    css.textContent = [
      /* 行の位置を測るために #scr を基準にする (scrollIntoView に頼らない) */
      '#scr{position:relative;-webkit-user-select:text;user-select:text;-webkit-touch-callout:default;}',
      '.zvrow{display:block;min-height:1.45em;white-space:pre;}',
      '#scr.zvwrap .zvrow{white-space:pre-wrap;overflow-wrap:anywhere;word-break:break-all;}',
      /* 空状態は利用可能領域の中央に 1 枚のカードで (UI 原則) */
      '#scr.zvmid{display:flex;align-items:center;justify-content:center;white-space:normal;padding:16px;}',
      '.zvhit{background:#3a2c12;color:#f2dfb4;}',
      '.zvhit.zvcur{background:#d29922;color:#0d1117;}',
      '#zvbar,#zvfind{flex:none;display:flex;align-items:center;gap:4px;padding:0 8px;',
      'background:#161b22;border-bottom:1px solid #21262d;-webkit-user-select:none;user-select:none;}',
      '#zvbar button,#zvfind button{min-width:44px;min-height:44px;background:none;border:none;',
      'color:#8b949e;font-size:12.5px;font-weight:600;padding:0 6px;border-radius:8px;}',
      '#zvbar button.on,#zvfind button.on{color:#7ee1ff;}',
      '#zvbar button.pri{color:#7ee1ff;}',
      '#zvbar button:active,#zvfind button:active{background:#1f3a5f;}',
      '#zvpos{flex:1;min-width:0;text-align:right;font-size:11px;color:#8b949e;',
      'overflow:hidden;text-overflow:ellipsis;white-space:nowrap;padding:0 4px;}',
      '#zvq{flex:1;min-width:0;background:#0d1117;color:#e6edf3;border:1px solid #30363d;',
      'border-radius:8px;padding:8px 10px;font-size:16px;outline:none;}',
      '#zvn{flex:none;font-size:11px;color:#8b949e;min-width:44px;text-align:center;}'
    ].join('\n');
    document.head.appendChild(css);

    // ── 自分の DOM は自分で作る (body.html は編集しない) ──
    function mkBtn(label, title, fn) {
      const b = document.createElement('button');
      b.textContent = label;
      b.title = title;
      b.onclick = fn;
      return b;
    }
    const bar = document.createElement('div');
    bar.id = 'zvbar';
    const bFind = mkBtn(T('remote.term_find', '\u{1F50D} 検索'),
      T('remote.term_find_ph', '端末内を検索…'), () => toggleFind());
    const bWrap = mkBtn(T('remote.term_wrap', '↩ 折返'),
      T('remote.term_wrap_title', '長い行を折り返す / 横スクロールに戻す'), toggleWrap);
    const bCopy = mkBtn(T('remote.term_copy', '⧉ コピー'),
      T('remote.term_copy_title', '読み込んだ端末の内容を全部コピーする'), copyAll);
    const pos = document.createElement('span');
    pos.id = 'zvpos';
    const bBottom = mkBtn(T('remote.term_bottom', '⬇ 最新'),
      T('remote.term_bottom_title', 'いちばん下へ戻って追従を再開する'), toBottom);
    [bFind, bWrap, bCopy, pos, bBottom].forEach(x => bar.appendChild(x));

    const find = document.createElement('div');
    find.id = 'zvfind';
    find.style.display = 'none';
    const q = document.createElement('input');
    q.id = 'zvq';
    q.type = 'search';
    q.autocapitalize = 'off';
    q.autocomplete = 'off';
    q.spellcheck = false;
    q.placeholder = T('remote.term_find_ph', '端末内を検索…');
    q.title = T('remote.term_find_scope', '読み込み済みの範囲だけを検索します');
    const cnt = document.createElement('span');
    cnt.id = 'zvn';
    cnt.title = T('remote.term_find_scope', '読み込み済みの範囲だけを検索します');
    const bPrev = mkBtn('▲', T('remote.term_prev', '前の一致へ'), () => gotoHit(-1));
    const bNext = mkBtn('▼', T('remote.term_next', '次の一致へ'), () => gotoHit(1));
    const bClose = mkBtn('✕', T('remote.term_find_close', '検索を閉じる'), () => toggleFind(false));
    [q, cnt, bPrev, bNext, bClose].forEach(x => find.appendChild(x));

    SEC.insertBefore(bar, SCR);
    SEC.insertBefore(find, SCR);
    if (localStorage.getItem(WRAP_KEY) === '1') { SCR.classList.add('zvwrap'); bWrap.classList.add('on'); }

    // ── 行の正規化 (受け取った span を 1 度だけ整える) ──
    function safeColor(c) { return (typeof c === 'string' && HEX.test(c)) ? c : ''; }
    // 行の指紋。**区切りを入れないと "ab"+"" と "a"+"b" が同じ**になり、
    // 変わったのに描き直さない行が出る (制御文字は必ずエスケープで書く)
    function sigOf(spans) {
      let s = '';
      for (let i = 0; i < spans.length; i++) {
        const p = spans[i];
        s += p.t + '\u0001' + p.fg + '\u0002' + p.bg + '\u0003'
          + (p.b ? 1 : 0) + (p.d ? 1 : 0) + (p.i ? 1 : 0) + (p.u ? 1 : 0) + '\u0004';
      }
      return s;
    }
    function norm(row) {
      const src = (row && row.spans) || [];
      const spans = [];
      let txt = '';
      for (let i = 0; i < src.length; i++) {
        const sp = src[i] || {};
        const t = sp.t == null ? '' : String(sp.t);
        if (!t) continue;
        spans.push({
          t: t, fg: safeColor(sp.fg), bg: safeColor(sp.bg),
          b: !!sp.bold, d: !!sp.dim, i: !!sp.italic, u: !!sp.underline
        });
        txt += t;
      }
      return { spans: spans, txt: txt, low: txt.toLowerCase(), sig: sigOf(spans) };
    }

    // ── 描画 (ANSI は解釈させない。文字は必ず textContent で入れる) ──
    function styled(text, sp, mark) {
      const s = document.createElement('span');
      if (mark) s.className = mark;
      let st = '';
      if (sp.fg) st += 'color:' + sp.fg + ';';
      if (sp.bg) st += 'background:' + sp.bg + ';';
      if (sp.b) st += 'font-weight:700;';
      if (sp.d) st += 'opacity:.62;';
      if (sp.i) st += 'font-style:italic;';
      if (sp.u) st += 'text-decoration:underline;';
      if (st) s.setAttribute('style', st);
      s.textContent = text;
      return s;
    }
    // ranges = [[開始, 終了, いま選んでいる一致か], …] (行頭からの文字位置)
    function paintRow(el, row, ranges) {
      while (el.firstChild) el.removeChild(el.firstChild);
      const rs = ranges && ranges.length ? ranges : null;
      let off = 0, ri = 0;
      for (let k = 0; k < row.spans.length; k++) {
        const sp = row.spans[k], t = sp.t;
        if (!rs) { el.appendChild(styled(t, sp, '')); off += t.length; continue; }
        let p = 0;
        while (p < t.length) {
          while (ri < rs.length && rs[ri][1] <= off + p) ri++;
          const r = rs[ri];
          if (!r || r[0] >= off + t.length) { el.appendChild(styled(t.slice(p), sp, '')); p = t.length; break; }
          const s0 = Math.max(r[0] - off, p);
          if (s0 > p) { el.appendChild(styled(t.slice(p, s0), sp, '')); p = s0; }
          const e0 = Math.min(r[1] - off, t.length);
          el.appendChild(styled(t.slice(p, e0), sp, r[2] ? 'zvhit zvcur' : 'zvhit'));
          p = e0;
        }
        off += t.length;
      }
    }
    function mkRow(row, ranges) {
      const d = document.createElement('div');
      d.className = 'zvrow';
      paintRow(d, row, ranges);
      return d;
    }
    function clearBox() {
      while (SCR.firstChild) SCR.removeChild(SCR.firstChild);
      SCR.classList.remove('empty');
      SCR.classList.remove('zvmid');
    }
    function fullRender() {
      clearBox();
      const f = document.createDocumentFragment();
      for (let i = 0; i < buf.length; i++) f.appendChild(mkRow(buf[i], rangesFor(i)));
      SCR.appendChild(f);
    }
    // 空状態。**既に出しているなら作り直さない** (1.5 秒ごとに DOM を
    // 組み替えると、アイドルのはずの画面が毎秒仕事をする)
    function showEmpty() {
      if (!buf.length && SCR.classList.contains('zvmid')) return;
      buf = []; bufFrom = 0; total = 0; plainSig = '';
      hits = []; byRow = new Map(); cur = 0; curAbs = null;
      syncCount();
      clearBox();
      SCR.classList.add('zvmid');
      const d = document.createElement('div');
      d.className = 'mid-card';
      const b = document.createElement('span');
      b.className = 'big'; b.textContent = '\u{1F916}';
      d.appendChild(b);
      const t = document.createElement('span');
      t.textContent = T('remote.no_agents_hint', 'エージェントがいません — ＋ 起動 から始められます');
      d.appendChild(t);
      SCR.appendChild(d);
      syncStatus();
    }
    function scrollBottom() { SCR.scrollTop = SCR.scrollHeight; }

    // ── 検索 (読み込み済みの範囲だけを見る) ──
    function computeHits() {
      hits = []; byRow = new Map();
      const s = findOn ? findQ.toLowerCase() : '';
      if (s) {
        for (let i = 0; i < buf.length; i++) {
          let p = buf[i].low.indexOf(s);
          if (p < 0) continue;
          const rs = [];
          while (p >= 0) { rs.push([p, p + s.length]); hits.push({ i: i, s: p }); p = buf[i].low.indexOf(s, p + s.length); }
          byRow.set(i, rs);
        }
      }
      syncCur();
    }
    // 覚えていた絶対行から「いま選んでいる 1 件」を引き直す。
    // 見つからなければ範囲内へ丸める (無くなった一致を指したままにしない)
    function syncCur() {
      if (!hits.length) { cur = 0; curAbs = null; return; }
      if (curAbs) {
        for (let k = 0; k < hits.length; k++) {
          if (bufFrom + hits[k].i === curAbs.n && hits[k].s === curAbs.s) { cur = k; return; }
        }
      }
      if (cur >= hits.length) cur = hits.length - 1;
      curAbs = { n: bufFrom + hits[cur].i, s: hits[cur].s };
    }
    function syncCount() {
      cnt.textContent = hits.length
        ? (cur + 1) + '/' + hits.length
        : ((findOn && findQ) ? '0' : '');
    }
    function rangesFor(i) {
      const rs = byRow.get(i);
      if (!rs) return null;
      const h = hits[cur];
      const same = h && h.i === i;
      const out = [];
      for (let k = 0; k < rs.length; k++) out.push([rs[k][0], rs[k][1], !!(same && h.s === rs[k][0])]);
      return out;
    }
    function repaint(i) {
      const el = SCR.children[i];
      if (el && buf[i]) paintRow(el, buf[i], rangesFor(i));
    }
    function refreshHits(prev) {
      const before = prev || byRow;
      computeHits();
      const touch = new Set();
      before.forEach((v, k) => touch.add(k));
      byRow.forEach((v, k) => touch.add(k));
      touch.forEach(repaint);
      syncCount();
    }
    // 一致 k へ移る。強調の付け替えは**関係する 2 行だけ**描き直す
    function setCur(k) {
      if (!hits.length) return;
      const old = cur;
      cur = (k % hits.length + hits.length) % hits.length;
      curAbs = { n: bufFrom + hits[cur].i, s: hits[cur].s };
      repaint(hits[old].i);
      repaint(hits[cur].i);
      syncCount();
      const el = SCR.children[hits[cur].i];
      // scrollIntoView は親ごと動かす端末があるので、自分で真ん中へ寄せる
      if (el) SCR.scrollTop = el.offsetTop - (SCR.clientHeight - el.offsetHeight) / 2;
      onScroll();
    }
    function gotoHit(d) {
      if (!hits.length) { toast(T('remote.term_no_hit', '見つかりません')); return; }
      setCur(cur + d);
    }
    function toggleFind(want) {
      findOn = (want === undefined || want === null) ? !findOn : !!want;
      find.style.display = findOn ? '' : 'none';
      bFind.classList.toggle('on', findOn);
      if (findOn) { q.focus(); } else { findQ = ''; q.value = ''; }
      curAbs = null; cur = 0;
      refreshHits();
      // 検索を開いたら、いちばん新しい一致 (下端に近いもの) から見せる
      if (findOn && hits.length) setCur(hits.length - 1);
    }
    q.addEventListener('input', () => {
      clearTimeout(findTimer);
      findTimer = setTimeout(() => {
        findQ = q.value;
        curAbs = null; cur = 0;
        refreshHits();
        if (hits.length) setCur(hits.length - 1);
      }, FIND_WAIT);
    });
    q.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); gotoHit(1); } });

    // ── 折り返し ──
    function toggleWrap() {
      const on = !SCR.classList.contains('zvwrap');
      SCR.classList.toggle('zvwrap', on);
      bWrap.classList.toggle('on', on);
      try { localStorage.setItem(WRAP_KEY, on ? '1' : '0'); } catch (e) {}
      if (follow) scrollBottom();
    }

    // ── コピー (http の LAN 経由では navigator.clipboard が無い。必ず代替を持つ) ──
    function legacyCopy(text) {
      const ta = document.createElement('textarea');
      ta.value = text;
      // 値は 'readonly' と書く。第 2 引数を空文字にすると、番人テスト
      // (remote::tests::t呼び出しに空のフォールバックが無い) が
      // 「フォールバックが空の T( 」と読む並びがページに現れてしまう
      ta.setAttribute('readonly', 'readonly');
      ta.style.cssText = 'position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;';
      document.body.appendChild(ta);
      let ok = false;
      try {
        ta.contentEditable = 'true';
        ta.readOnly = false;
        const rg = document.createRange();
        rg.selectNodeContents(ta);
        const sel = document.getSelection();
        sel.removeAllRanges();
        sel.addRange(rg);
        ta.setSelectionRange(0, text.length);
        ok = document.execCommand('copy');
      } catch (e) { ok = false; }
      document.body.removeChild(ta);
      return ok;
    }
    async function copyAll() {
      if (!buf.length) return;
      const text = buf.map(r => r.txt).join('\n');
      let ok = false;
      try {
        if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(text); ok = true; }
      } catch (e) { ok = false; }
      if (!ok) ok = legacyCopy(text);
      toast(ok
        ? T('remote.term_copied', '{n} 行をコピーしました').replace('{n}', buf.length)
        : T('remote.term_copy_failed', 'コピーできませんでした — 長押しで選択してください'));
    }

    // ── 追従 / 遡り ──
    function syncStatus() {
      bBottom.classList.toggle('pri', !follow);
      if (follow || !buf.length) { pos.textContent = ''; return; }
      const span = SCR.scrollHeight - SCR.clientHeight;
      const r = span > 0 ? Math.min(1, Math.max(0, SCR.scrollTop / span)) : 1;
      const n = bufFrom + Math.round((buf.length - 1) * r) + 1;
      pos.textContent = T('remote.term_pos', '{n}/{total} 行')
        .replace('{n}', n).replace('{total}', Math.max(total, bufFrom + buf.length));
    }
    function pause() {
      if (!follow) return;
      follow = false;
      clearTimeout(termTimer);
      syncStatus();
    }
    function resume() {
      if (follow) return;
      follow = true;
      syncStatus();
      clearTimeout(termTimer);
      pollTerm();
    }
    function toBottom() {
      scrollBottom();
      resume();
    }
    function onScroll() {
      const at = SCR.scrollTop + SCR.clientHeight >= SCR.scrollHeight - NEAR_BOTTOM;
      if (at) resume(); else pause();
      if (!at) syncStatus();
      if (!at && SCR.scrollTop <= NEAR_TOP) prependOlder();
    }
    SCR.addEventListener('scroll', onScroll, { passive: true });
    // 長押しで選び始めたら追従を止める。1.5 秒ごとに下へ引き戻されると
    // 選択がその都度消えて、そもそも選べない
    document.addEventListener('selectionchange', () => {
      if (view !== 'agent' || aview !== 'term' || !follow) return;
      const sel = document.getSelection();
      if (!sel || sel.isCollapsed || !sel.rangeCount) return;
      const n = sel.anchorNode;
      const e = n && (n.nodeType === 1 ? n : n.parentNode);
      if (e && SCR.contains(e)) pause();
    });

    // ── サーバとのやり取り ──
    function agentIdx() {
      const v = state && state.agent_active;
      return (typeof v === 'number' && v >= 0) ? v : -1;
    }
    // 応答の読み分けは 3 通り。**「無い」と「今は空」を混ぜない**:
    //   {gone:true} … この経路がサーバに無い (404/405/501 / JSON でない /
    //                 こちらの API の形ですらない) → 二度と叩かず /api/term へ落ちる
    //   {none:true} … 経路はあるが見せるものが無い (`{"ok":false,"error":…}`
    //                 = セッションが無い / 400)。**落とさない** — エージェントが
    //                 起きれば次のティックから普通に出る
    //   {net:true}  … 通信断・5xx。一時的なので次のティックで取り直す
    async function fetchSb(lines, before) {
      let url = '/api/scrollback?lines=' + lines;
      if (before !== undefined) url += '&before=' + before;
      const a = agentIdx();
      if (a >= 0) url += '&agent=' + a;
      let r;
      try { r = await fetch(url, { headers: { 'X-Token': TOK } }); } catch (e) { throw { net: true }; }
      if (r.status === 401) {
        toast(T('remote.auth_error', '認証エラー: QRコードを読み直してください'));
        throw { net: true };
      }
      if (r.status === 404 || r.status === 405 || r.status === 501) throw { gone: true };
      if (r.status === 400) throw { none: true };
      if (!r.ok) throw { net: true };
      let j;
      try { j = await r.json(); } catch (e) { throw { gone: true }; }
      if (!j || typeof j !== 'object') throw { gone: true };
      if (!Array.isArray(j.rows)) {
        // 応答の形はこちらの API — ただし今は返す行が無い (セッションが無い等)
        if (j.ok === false || typeof j.error === 'string') throw { none: true };
        throw { gone: true };
      }
      return j;
    }
    // 末尾を取り直して重ねる。**変わった行だけ描き直す** (毎回作り直すと
    // 選択も読んでいる位置も消える)
    function applyTail(from, rows) {
      const rs = rows.map(norm);
      if (!buf.length || from > bufFrom + buf.length || from < bufFrom) {
        bufFrom = from; buf = rs; noTop = false;
        fullRender();
        return true;
      }
      let changed = false;
      for (let i = 0; i < rs.length; i++) {
        const k = from + i - bufFrom;
        if (k < 0) continue;
        if (k < buf.length) {
          if (buf[k].sig === rs[i].sig) continue;
          buf[k] = rs[i]; changed = true;
          repaint(k);
        } else {
          buf.push(rs[i]); changed = true;
          SCR.appendChild(mkRow(rs[i], null));
        }
      }
      while (buf.length > MAX_ROWS) {
        buf.shift(); bufFrom++; changed = true;
        if (SCR.firstChild) SCR.removeChild(SCR.firstChild);
      }
      return changed;
    }
    async function prependOlder() {
      if (loading || sbOff || noTop || bufFrom <= 0) return;
      if (buf.length >= MAX_ROWS) {
        if (!noTop) { noTop = true; toast(T('remote.term_limit', '読み込みは {n} 行までです').replace('{n}', MAX_ROWS)); }
        return;
      }
      loading = true;
      try {
        const j = await fetchSb(CHUNK, bufFrom);
        const from = j.from | 0;
        const rows = (j.rows || []).map(norm);
        if (typeof j.total === 'number') total = j.total;
        if (!rows.length || from >= bufFrom) {
          noTop = true;
          toast(T('remote.term_top_reached', 'これより前の記録はありません'));
        } else {
          const head = rows.slice(0, Math.min(rows.length, bufFrom - from));
          // 読んでいた行が飛ばないように、増えたぶんだけ scrollTop をずらす
          const h0 = SCR.scrollHeight, t0 = SCR.scrollTop;
          const f = document.createDocumentFragment();
          for (let i = 0; i < head.length; i++) f.appendChild(mkRow(head[i], null));
          SCR.insertBefore(f, SCR.firstChild);
          buf = head.concat(buf);
          bufFrom = from;
          SCR.scrollTop = t0 + (SCR.scrollHeight - h0);
          if (bufFrom <= 0) noTop = true;
          refreshHits(new Map());
          syncStatus();
        }
      } catch (e) {
        if (e && e.gone) sbOff = true;
        else if (e && e.none) noTop = true;   // 今は返せない — 上端を叩き続けない
      }
      loading = false;
    }
    async function pollSb() {
      let j;
      try { j = await fetchSb(TAIL); } catch (e) {
        if (e && e.gone) { sbOff = true; await pollPlain(); }
        else if (e && e.none) showEmpty();
        return;
      }
      if (typeof j.total === 'number') total = j.total;
      // 行も総数も無い = 見せるものが無い (エージェントが居ない / 終わった)
      if (!j.rows.length && !total) { showEmpty(); return; }
      const prev = byRow;
      if (applyTail(j.from | 0, j.rows)) refreshHits(prev);
      if (follow) scrollBottom();
      syncStatus();
    }
    // /api/scrollback が無いサーバ向け。**いまの /api/term の挙動そのまま**
    // (見えている画面だけ) を、色なしの 1 行 1 span として描く
    async function pollPlain() {
      let r;
      try { r = await api('/api/term'); } catch (e) { return; }
      if (!r || !r.ok) { showEmpty(); return; }
      const text = r.text == null ? '' : String(r.text);
      if (text === plainSig && buf.length) { if (follow) scrollBottom(); return; }
      plainSig = text;
      const lines = text.split('\n');
      buf = lines.map(t => norm({ spans: [{ t: t }] }));
      bufFrom = 0; total = buf.length; noTop = true;
      computeHits();
      fullRender();
      if (follow) scrollBottom();
      syncStatus();
      syncCount();
    }
    // 見ているエージェントが変わったら作り直す (別の端末の行を継ぎ足さない)
    function syncAgent() {
      const a = agentIdx();
      if (a === curAgent) return;
      curAgent = a;
      buf = []; bufFrom = 0; total = 0; plainSig = '';
      noTop = false; follow = true;
      hits = []; byRow = new Map(); cur = 0; curAbs = null;
      clearBox();
      syncCount();
      syncStatus();
    }

    // ── 既存の入口を包む (70-boards.js は触らない) ──
    const origPoll = pollTerm;
    const origSetAView = setAView;
    pollTerm = function () {
      clearTimeout(termTimer);
      if (view !== 'agent') return;              // 見ていないビューのために PTY を読ませない
      if (aview !== 'term') return origPoll();   // 一覧は従来どおり /api/agents
      syncAgent();
      if (!follow) { syncStatus(); return; }     // 遡っている間は取りに行かない
      tick();
    };
    // 取得は**同時に 1 本だけ**。下端へ戻った瞬間の再開 (resume) が
    // 飛んできても、走っている取得に重ねない (重ねると同じ行を 2 回描く)
    let busy = false;
    async function tick() {
      if (busy) return;   // 走っている方が最後に次を予約する
      busy = true;
      try {
        if (sbOff) await pollPlain(); else await pollSb();
      } catch (e) {
      } finally {
        busy = false;
      }
      if (view === 'agent' && aview === 'term' && follow) {
        clearTimeout(termTimer);
        termTimer = setTimeout(pollTerm, POLL_MS);
      }
    }
    setAView = function (k) {
      origSetAView(k);
      const on = k === 'term';
      bar.style.display = on ? '' : 'none';
      find.style.display = (on && findOn) ? '' : 'none';
      // 端末へ入り直したら必ず**生きている状態**に戻す。遡ったまま離れると
      // 追従が止まっていてタイマーが 1 本も無く、戻ってきても画面が凍った
      // ままになる (「押しても切り替わらない」の正体のひとつ)。
      if (on && !follow) { follow = true; scrollBottom(); syncStatus(); }
    };
    // 下部ナビの [エージェント] も同じ扱い。10-view.js は `pollTerm()` を
    // 呼ぶだけで、遡って止まっている追従までは戻せない。
    // capture で先に受けてから、既存のハンドラに素通しする。
    const nav = document.getElementById('nav');
    if (nav) {
      nav.addEventListener('click', e => {
        const b = e.target && e.target.closest ? e.target.closest('button') : null;
        if (b && b.dataset && b.dataset.v === 'agent' && !follow) {
          follow = true; scrollBottom(); syncStatus();
        }
      }, true);
    }
    syncStatus();
  } catch (e) {
    // 端末の拡張で落ちても、この後ろの JS (キー列・音声・一覧・コマンド) は
    // 必ず動かす。ここで例外を漏らすと <script> ごと止まって画面が死ぬ
  }
})();

// ─── エージェント (キー列) ───
