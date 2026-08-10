//! 🤝 **交渉の運び手** — Erlang 風メッシュ ([`crate::mesh`]) と
//! 行域の交渉 ([`crate::negotiate`]) を繋ぐ、ただ 1 つの層。
//!
//! ## なぜ別のモジュールなのか
//!
//! `mesh` と `negotiate` は**互いを 1 バイトも知らない**:
//!
//! * `mesh` は `spec` を**不透明な文字列**として運ぶ。行域の意味を持たない
//!   ので、`region` にも `negotiate` にもコンパイル時依存が無い
//! * `negotiate` は**純関数だけ**。I/O もスレッドも持たず、`Deal` を
//!   1 行の文字列へ往復させるところで止まっている
//!
//! これは並列開発の都合ではなく**設計**である。運び手を差し替えても交渉の
//! 判断は変わらないし、交渉の規則を変えても運び方は変わらない。ただし
//! **2 つの半分だけでは何も起きない**ので、繋ぐ層が 1 つ要る。それがここ。
//!
//! ## 2 つの入口 (要求側が「ずらしてよいか」を言えるかで分かれる)
//!
//! | 受け取るもの | 意味 | 扱い |
//! |---|---|---|
//! | [`Msg::Claim`] | 素の確保要求。ずらしてよいかを**言っていない** | [`Want::fixed`] — **絶対にずらさない** |
//! | [`Msg::Custom`] `kind = "negotiate"` | [`Deal`] を載せた交渉要求 | 本文のとおり (`movable` / `size_only` を尊重) |
//!
//! **言っていない要求をずらさない**のがこの層の一番大事な約束である。
//! 行域は行番号ではなく*そこにある内容*に紐づくので、勝手にずらすと
//! 「別の関数を編集しろ」と言ったことになる。既定は必ず `fixed`。
//!
//! ## 返事は必ず返す
//!
//! 読めなかった行にも返事を出す。メッシュで黙って落とすと、送り手は
//! 永遠に待つ (Erlang で送信先が死んでいても `DOWN` が来るのと同じ理由)。
//!
//! ## 一括で決める
//!
//! 受信箱に複数の要求が溜まっていたら、**1 件ずつ答えない**。
//! [`crate::negotiate::allocate`] へまとめて渡す — 片方ずつ答えると、
//! 互いに重なる 2 件の両方に「通る」と答えてしまう。
//!
//! ただし**ファイルをまたいで 1 回の `allocate` に混ぜる必要は無い**。
//! 行域はファイルをまたいで干渉しないので、[`serve_once`] は受信箱を
//! **パスの辞書順**でファイルごとに束ね、1 周で全ファイルを片付ける。
//!
//! ## 終了コード (`zai negotiate serve` / `zai negotiate ask` 共通の並び)
//!
//! | 値 | `serve` | `ask` |
//! |---|---|---|
//! | 0 | 回し終えた | 取れた |
//! | 1 | — | **断られた** |
//! | 2 | 使い方の誤り | 使い方の誤り |
//! | 3 | 既に交渉役が居る | **交渉役が居ない** |
//! | 4 | — | **上限まで待ったが返事が来ない** |
//! | 5 | メッシュが無効 | メッシュが無効 |
//!
//! 0 / 2 / 5 は両方で同じ意味にしてある。1 / 3 / 4 は
//! 「要求する側にしか無い失敗」と「答える側にしか無い失敗」なので重ならない。

use crate::features::mesh::{Mesh, Msg, Pid};
use crate::features::negotiate;
use crate::region::{self, Region};
use negotiate::{Deal, Offer, Want};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// 交渉で使う `Msg::Custom` の種別。**この文字列が両側の唯一の合図**。
pub const DEAL_KIND: &str = "negotiate";

/// [`serve_once`] が 1 回で何をしたか。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Served {
    /// 通した確保 (`spec`)。
    pub granted: Vec<String>,
    /// 断った確保 (`spec`, 持ち主, 次の一手)。
    pub denied: Vec<(String, String, String)>,
    /// ずらして通したもの (`要求`, `実際に取れた域`)。
    pub shifted: Vec<(String, String)>,
    /// 読めなかった行の数 (返事はしている)。
    pub unreadable: usize,
}

impl Served {
    /// 何も来ていなかったか。**アイドル時に 1 バイトも書かないための門**。
    pub fn is_idle(&self) -> bool {
        self.granted.is_empty()
            && self.denied.is_empty()
            && self.shifted.is_empty()
            && self.unreadable == 0
    }
}

/// いま台帳に載っている担当を `negotiate` が読める形へ。
///
/// **持ち主の表記は `Pid` の文字列**にする。`Denied` の `holder` に
/// そのまま載るので、受け取った側が `Pid::parse` で引き直せる。
fn occupied_of(mesh: &Mesh, path: &str) -> Vec<(String, Region)> {
    let mut out: Vec<(String, Region)> = Vec::new();
    for c in mesh.claims() {
        let Ok(r) = region::parse(&c.spec) else {
            continue;
        };
        // 同じファイルのぶんだけを見る (別ファイルは行域で干渉しない)。
        if r.path != path {
            continue;
        }
        out.push((c.pid.to_string(), r));
    }
    // 決定的な順に揃える (mesh の列挙順を出力へ漏らさない)。
    out.sort_by(|a, b| (&a.1.path, a.1.span, &a.0).cmp(&(&b.1.path, b.1.span, &b.0)));
    out
}

/// 受信箱を **1 回ぶん**処理して、確保要求へ答える。
///
/// `lines_of` は**パス → 行数**を解く関数。1 周で複数ファイルの要求を捌くので、
/// 行数は 1 つの値では足りない (ここを定数にしていたため、2 ファイル目以降は
/// 常に間違った上限で判断していた)。この層自身はディスクを触らない —
/// 短命なフックプロセスから呼ばれても I/O を持たずに済ませたいので、
/// **読み方は呼び出し側が決める**。既定の実装は [`lines_from_disk`]。
/// `0` を返すと「上限不明」として扱い、**ずらし先を提案しない**
/// (知らない場所を勧めるより断る方が安全)。
///
/// **1 周で全ファイルを片付ける。** 以前は「受信箱の先頭と同じファイル」だけを
/// 処理して、別ファイルの要求を次の周へ回していた。ところが次の周は
/// **新しい要求が来るまで始まらない** (`--rounds 1` の短命フックが既定の使い方)
/// ので、2 ファイル目以降の送り手だけが上限まで待たされて
/// 「返事が来ない」で落ちる。ファイルごとに [`negotiate::allocate`] を呼び、
/// **パスの辞書順**で回す (`BTreeMap` なので `Mesh::claims` の列挙順は
/// 出力へ漏れない)。
/// 既定の「パス → 行数」。**リポジトリのルートからの相対パス**として読む。
///
/// 読めない (存在しない・二値・権限が無い) なら `0` を返す —
/// [`serve_once`] はそれを「上限不明」として扱い、**ずらし先を提案しない**。
/// 「知らない場所を勧める」より「断る」ほうが安全側だからである。
///
/// 上限 [`LINES_READ_CAP`] を超えるファイルは読まない。交渉役は短命な
/// プロセスから叩かれることがあるので、1 回の判断で数百 MB を読み込む
/// 経路を残さない。
pub fn lines_from_disk(root: &Path, rel: &str) -> u32 {
    let p = root.join(rel);
    let Ok(md) = std::fs::metadata(&p) else {
        return 0;
    };
    if !md.is_file() || md.len() > LINES_READ_CAP {
        return 0;
    }
    let Ok(text) = std::fs::read_to_string(&p) else {
        return 0; // 二値・不正な UTF-8
    };
    text.lines().count().min(u32::MAX as usize) as u32
}

/// [`lines_from_disk`] が読むファイルサイズの上限 (バイト)。
const LINES_READ_CAP: u64 = 8 * 1024 * 1024;

pub fn serve_once(mesh: &Mesh, me: &Pid, lines_of: &dyn Fn(&str) -> u32, band: u32) -> Served {
    let mut out = Served::default();
    let envs = mesh.recv_match(me, &|m| {
        matches!(m, Msg::Claim { .. }) || matches!(m, Msg::Custom { kind, .. } if kind == DEAL_KIND)
    });
    if envs.is_empty() {
        return out;
    }

    // ① 受け取った要求を Want へ。**素の Claim は必ず fixed**。
    let mut asks: Vec<(Pid, Want, bool)> = Vec::new(); // (要求者, 要求, deal 形式か)
    for e in &envs {
        match &e.msg {
            Msg::Claim { spec } => match region::parse(spec) {
                Ok(r) => asks.push((e.from.clone(), Want::fixed(&e.from.to_string(), r), false)),
                Err(why) => {
                    out.unreadable += 1;
                    let _ = mesh.send(
                        &e.from,
                        me,
                        Msg::Denied {
                            spec: spec.clone(),
                            holder: String::new(),
                            hint: why,
                        },
                    );
                }
            },
            Msg::Custom { body, .. } => match negotiate::decode(body) {
                Ok(Deal::Propose { want, .. }) => asks.push((e.from.clone(), want, true)),
                Ok(_) => { /* 返事の返事は要らない */ }
                Err(why) => {
                    out.unreadable += 1;
                    let _ = mesh.send(
                        &e.from,
                        me,
                        Msg::Custom {
                            kind: DEAL_KIND.into(),
                            body: negotiate::encode(&Deal::Reject {
                                from: me.to_string(),
                                id: String::new(),
                                reason: why,
                            }),
                        },
                    );
                }
            },
            _ => {}
        }
    }
    if asks.is_empty() {
        return out;
    }

    // ② **ファイルごとに、まとめて**決める。1 件ずつ答えると、互いに重なる
    //    2 件の両方へ「通る」と答えてしまう。逆にファイルをまたいで混ぜる
    //    必要は無い (行域はファイルをまたいで干渉しない)。
    let mut by_path: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, (_, w, _)) in asks.iter().enumerate() {
        by_path.entry(w.region.path.as_str()).or_default().push(i);
    }

    // ③ パスの辞書順に、返事を出して台帳へ載せる。
    for (path, idxs) in &by_path {
        // 台帳は**このファイルを配る直前**に読み直す。前のファイルの確保で
        // 増えた行は別ファイルなので効かないが、他プロセスが割り込んで
        // 取った担当はここで拾える (古い占有表で配ると重ねてしまう)。
        let occupied = occupied_of(mesh, path);
        let wants: Vec<Want> = idxs.iter().map(|&i| asks[i].1.clone()).collect();
        let plan = negotiate::allocate(&wants, &occupied, lines_of(path), band);

        for &i in idxs {
            let (from, want, is_deal) = &asks[i];
            let got = plan.granted.iter().find(|g| g.id == want.id);
            match got {
                Some(g) => {
                    let Some(first) = g.regions.first() else {
                        continue;
                    };
                    let spec = region::render(first);
                    if mesh.claim(&spec, from).is_err() {
                        // 台帳側で負けた = 誰かが先に取った。断りへ倒す
                        // (**取れたと言ってから取れていない**が一番危ない)。
                        out.denied.push((
                            region::render(&want.region),
                            String::new(),
                            "先に取られました。もう一度要求してください".into(),
                        ));
                        reply_denied(mesh, me, from, want, "", "再要求", *is_deal);
                        continue;
                    }
                    // 分割されたら 2 つ目以降も台帳へ載せる (載せ落とすと
                    // 「持っていない域を書いている」状態になる)。
                    for extra in g.regions.iter().skip(1) {
                        let _ = mesh.claim(&region::render(extra), from);
                    }
                    if g.regions.len() != 1 || g.regions[0] != want.region {
                        out.shifted
                            .push((region::render(&want.region), spec.clone()));
                    }
                    out.granted.push(spec.clone());
                    if *is_deal {
                        let _ = mesh.send(
                            from,
                            me,
                            Msg::Custom {
                                kind: DEAL_KIND.into(),
                                body: negotiate::encode(&Deal::Accept {
                                    from: me.to_string(),
                                    id: want.id.clone(),
                                    region: first.clone(),
                                }),
                            },
                        );
                    } else {
                        let _ = mesh.send(from, me, Msg::Granted { spec });
                    }
                }
                None => {
                    let off = negotiate::offer(want, &occupied, lines_of(path), band);
                    let (holder, hint) = describe(&off);
                    out.denied
                        .push((region::render(&want.region), holder.clone(), hint.clone()));
                    reply_denied(mesh, me, from, want, &holder, &hint, *is_deal);
                }
            }
        }
    }
    out
}

/// 断りの返事を、要求の形に合わせて出す。
fn reply_denied(
    mesh: &Mesh,
    me: &Pid,
    to: &Pid,
    want: &Want,
    holder: &str,
    hint: &str,
    is_deal: bool,
) {
    if is_deal {
        let _ = mesh.send(
            to,
            me,
            Msg::Custom {
                kind: DEAL_KIND.into(),
                body: negotiate::encode(&Deal::Reject {
                    from: me.to_string(),
                    id: want.id.clone(),
                    reason: hint.to_string(),
                }),
            },
        );
    } else {
        let _ = mesh.send(
            to,
            me,
            Msg::Denied {
                spec: region::render(&want.region),
                holder: holder.to_string(),
                hint: hint.to_string(),
            },
        );
    }
}

/// [`Offer`] を「持ち主」と「次の一手」の 2 つの文字列へ。
///
/// 画面にも `Msg::Denied` にも同じ文言が載るので、**ここが唯一の真実源**。
fn describe(off: &Offer) -> (String, String) {
    match off {
        Offer::Grant => (String::new(), "通ります".into()),
        Offer::Shift { to, .. } => (
            String::new(),
            format!("ずらす — {} なら誰とも重なりません", region::render(to)),
        ),
        Offer::Split { parts } => (
            String::new(),
            format!(
                "分ける — {}",
                parts
                    .iter()
                    .map(region::render)
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
        ),
        Offer::Wait { holder, .. } => (holder.clone(), "待つ — 持ち主が手放すまで".into()),
        Offer::Impossible { reason } => (String::new(), reason.clone()),
    }
}

/// 要求側の待ち間隔。**[`crate::features::mesh::backoff`] とは別物**。
///
/// mesh の backoff は「何も起きていない見張り」用で **2 秒から**始まる。
/// 要求側が待っているのは*すぐ来るはずの返事*なので、そこから始めると
/// 交渉役が同じ機械で回っていても**即答が 2 秒に化ける**
/// (実測: 同一マシン・同一ディレクトリの往復は 1〜3ms。それを 2000ms 待つ)。
/// 25ms から倍々にして 2 秒で頭打ちにする。
///
/// | `rounds` | 待ちの上限 |
/// |---|---|
/// | 4 | 0.375 秒 |
/// | 8 | 5.175 秒 (既定) |
/// | 12 | 13.175 秒 |
///
/// 既定を 8 (≒5 秒) にしたのは Erlang の `gen_server:call` の既定と同じ理由で、
/// 「人が『固まった』と感じる前に諦める」線がそこにあるため。
fn ask_backoff(round: u32) -> Duration {
    // 25ms << round。`round` が大きいと桁溢れするので上限側で先に潰す。
    let ms = 25u64.saturating_mul(1u64 << round.min(20));
    Duration::from_millis(ms.min(2_000))
}

/// `rounds` 回まで待つときの、待ち時間の合計 (表示用)。
///
/// 「返事が来ませんでした」とだけ言われても**どれだけ待ったのか**が
/// 分からないと、増やせばよいのか交渉役が死んでいるのかを判断できない。
fn ask_budget(rounds: u32) -> Duration {
    (0..rounds).map(ask_backoff).sum()
}

/// 要求する側。**確保を頼んで、返事を待つ。**
///
/// `movable` が `true` なら [`Deal`] 形式で送る (= ずらしてよいと明示する)。
/// `false` なら素の [`Msg::Claim`] を送る — 相手はずらし先を提案しない。
/// **言っていない要求を勝手にずらさない**のがこの層の約束なので、
/// ここで形を変えるのが「ずらしてよい」の唯一の表明手段である。
///
/// 待ちは**上限つき** ([`ask_budget`]) で、来なければ `None` を返す。
/// 永遠に待たないのがこの製品の約束 (設計原則 2: 隠れている処理は
/// 欠落ありでよいが、決してブロックさせない)。返り値の 3 状態
/// (`Some(Ok)` / `Some(Err)` / `None`) は**呼び出し側で必ず区別する** —
/// 「断られた」と「返事が来ない」は打つ手がまったく違う。
pub fn request(
    mesh: &Mesh,
    me: &Pid,
    to: &Pid,
    want: &Want,
    rounds: u32,
) -> Option<Result<Region, String>> {
    let sent = if want.movable {
        mesh.send(
            to,
            me,
            Msg::Custom {
                kind: DEAL_KIND.into(),
                body: negotiate::encode(&Deal::Propose {
                    from: me.to_string(),
                    want: want.clone(),
                }),
            },
        )
    } else {
        mesh.send(
            to,
            me,
            Msg::Claim {
                spec: region::render(&want.region),
            },
        )
    };
    sent.ok()?;
    // **選択受信**。`recv` は受信箱を丸ごと空にするので、`ZAIVERN_MESH_PID` の
    // ような**長生きするプロセスの Pid で要求したとき、そのプロセス宛の
    // `Announce` / `Down` / `Sync` まで食い潰す**。返事に当たる 3 種だけを
    // 取り出し、残りは受信箱に置いていく (Erlang の `receive` と同じ)。
    let is_reply = |m: &Msg| {
        matches!(m, Msg::Granted { .. } | Msg::Denied { .. })
            || matches!(m, Msg::Custom { kind, .. } if kind == DEAL_KIND)
    };
    for round in 0..rounds {
        for e in mesh.recv_match(me, &is_reply) {
            match e.msg {
                Msg::Granted { spec } => return Some(region::parse(&spec)),
                Msg::Denied { holder, hint, .. } => {
                    return Some(Err(if holder.is_empty() {
                        hint
                    } else {
                        format!("{holder} が持っています。{hint}")
                    }))
                }
                Msg::Custom { ref kind, ref body } if kind == DEAL_KIND => {
                    match negotiate::decode(body) {
                        Ok(Deal::Accept { region, .. }) => return Some(Ok(region)),
                        Ok(Deal::Reject { reason, .. }) => return Some(Err(reason)),
                        Ok(Deal::Counter { offer, .. }) => return Some(Err(describe(&offer).1)),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(ask_backoff(round));
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════════
//  CLI — `zai negotiate serve` / `zai negotiate ask`
// ═══════════════════════════════════════════════════════════════════════════

/// 取れた / 回し終えた。
pub const EXIT_OK: i32 = 0;
/// **断られた** (`ask` のみ)。どうやっても通らない。
pub const EXIT_DENIED: i32 = 1;
/// 使い方の誤り。
pub const EXIT_USAGE: i32 = 2;
/// 交渉役が居ない (`ask`) / 既に交渉役が居る (`serve`)。
/// **どちらも「交渉役はちょうど 1 体」の破れ**なので同じ番号にしてある。
pub const EXIT_NEGOTIATOR: i32 = 3;
/// **上限まで待ったが返事が来なかった** (`ask` のみ)。
/// 断られたのとは**別物**: 交渉役が回っていない可能性が高い。
pub const EXIT_NO_REPLY: i32 = 4;
/// メッシュが無効 (`~/.zaivern/mesh/…` が無い = 誰もメッシュに載っていない)。
///
/// **`join` というサブコマンドは無い。** メッシュは `zai mesh spawn` が
/// `procs/` を作った時点で有効になる (`Mesh::enabled` は `stat` 1 回)。
/// 案内文を間違えると、ユーザーは存在しないコマンドを叩いて詰まる
/// (実際に `zai mesh join` と案内していて `知らないサブコマンド join` が出た)。
pub const EXIT_NO_MESH: i32 = 5;

/// メッシュ上の登録名。**1 リポジトリに 1 体だけ**が名乗れる。
pub const NEGOTIATOR: &str = "negotiator";

/// `ask` の既定の待ち周回数 (≒5.2 秒。[`ask_backoff`] の表を参照)。
const ASK_ROUNDS: u32 = 8;

/// 交渉プロセスを立てて、受信箱を回す。
///
/// ## なぜ「名前」で 1 体に絞るのか
///
/// 交渉役が 2 体居ると、**互いに重なる 2 件の両方へ「通る」と答えられる**
/// (それぞれが相手の判断を知らないため)。Erlang の `register/2` と同じく、
/// `"negotiator"` という名前は 1 つの [`Pid`] にしか付かず、競合したら
/// **fail-closed で 2 体目が降りる** — これがこの層の唯一の正しさの根拠。
///
/// ## 終了コード
///
/// | 値 | 意味 |
/// |---|---|
/// | [`EXIT_OK`] | 回し終えた |
/// | [`EXIT_USAGE`] | 使い方の誤り |
/// | [`EXIT_NEGOTIATOR`] | 既に交渉役が居る (名前が取られている) |
/// | [`EXIT_NO_MESH`] | メッシュが無効 (`~/.zaivern/mesh/…` が無い) |
pub fn serve_cli(argv: &[String]) -> i32 {
    let mut rounds: u32 = 1;
    let mut lines: u32 = 0;
    let mut band: u32 = crate::region::SAFE_BAND;
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        let val = |it: &mut dyn Iterator<Item = &String>| it.next().and_then(|v| v.parse().ok());
        match a.as_str() {
            "--rounds" => match val(&mut it) {
                Some(v) => rounds = v,
                None => return EXIT_USAGE,
            },
            "--lines" => match val(&mut it) {
                Some(v) => lines = v,
                None => return EXIT_USAGE,
            },
            "--band" => match val(&mut it) {
                Some(v) => band = v,
                None => return EXIT_USAGE,
            },
            _ => return EXIT_USAGE,
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    // 行数は**ファイルごと**に解く。`--lines` は「全ファイルこの行数として
    // 扱う」上書きで、指定が無ければ実ファイルから読む (既定)。
    // 定数 1 つで全ファイルを判断していた頃は、2 ファイル目以降が常に
    // 間違った上限で判定されていた。
    let root = cwd.clone();
    let resolver = move |rel: &str| -> u32 {
        if lines > 0 {
            lines
        } else {
            lines_from_disk(&root, rel)
        }
    };
    let mesh = Mesh::open_for(&cwd);
    if !mesh.enabled() {
        eprintln!("メッシュが有効ではありません (先に `zai mesh spawn` を実行してください)");
        return EXIT_NO_MESH;
    }
    let Ok(me) = mesh.spawn(crate::features::mesh::SpawnOpts {
        role: NEGOTIATOR.into(),
        label: "行域の交渉役".into(),
        trap_exit: true,
        ..Default::default()
    }) else {
        eprintln!("メッシュに参加できません");
        return EXIT_NO_MESH;
    };
    if mesh.register(NEGOTIATOR, &me.pid).is_err() {
        eprintln!("既に交渉役が居ます (1 リポジトリに 1 体だけ)");
        let _ = mesh.exit(&me.pid, "duplicate-negotiator");
        return EXIT_NEGOTIATOR;
    }
    let mut total = Served::default();
    // **連続して空振りした回数**。周回の通し番号ではない。
    // `mesh::backoff` の引数は「何も起きなかった周が続いた数」なので、
    // 通し番号を渡すと**仕事をした直後でも 30 秒寝る**ようになる
    // (round 4 以降は上限に張り付く)。要求側の待ちは既定 5.2 秒なので、
    // そのままだと**忙しいリポジトリほど「返事が来ない」で落ちる**。
    let mut idle_streak: u32 = 0;
    for round in 0..rounds {
        mesh.beat(&me.pid);
        let s = serve_once(&mesh, &me.pid, &resolver, band);
        let idle = s.is_idle();
        total.granted.extend(s.granted);
        total.denied.extend(s.denied);
        total.shifted.extend(s.shifted);
        total.unreadable += s.unreadable;
        if !idle {
            // 仕事があった = 次も来る見込み。間隔を最短へ戻す。
            idle_streak = 0;
            continue;
        }
        if round + 1 < rounds {
            // 何も来ていない周は寝る (アイドル時のコストはゼロ)。
            std::thread::sleep(crate::features::mesh::backoff(idle_streak));
        }
        idle_streak = idle_streak.saturating_add(1);
    }
    println!(
        "通した {} / 断った {} / ずらした {} / 読めなかった {}",
        total.granted.len(),
        total.denied.len(),
        total.shifted.len(),
        total.unreadable
    );
    for (want, got) in &total.shifted {
        println!("  ずらした: {want} → {got}");
    }
    for (spec, holder, hint) in &total.denied {
        let who = if holder.is_empty() {
            String::new()
        } else {
            format!(" ({holder})")
        };
        println!("  断った: {spec}{who} — {hint}");
    }
    let _ = mesh.unregister(NEGOTIATOR, &me.pid);
    let _ = mesh.exit(&me.pid, "done");
    EXIT_OK
}

/// 要求する側の入口 — `zai negotiate ask --spec 'src/a.rs#L10-40' […]`。
///
/// ## なぜ「自分が交渉役になる」を選択肢に入れないのか
///
/// 交渉役が居ないときに勝手に名乗ると、**同時に走った 2 つの `ask` が
/// 2 体の交渉役になる**。それぞれ相手の判断を知らないので、互いに重なる
/// 2 件の**両方へ「通る」と答えてしまう** — 衝突 0 の根拠がその瞬間に消える。
/// 居なければ [`EXIT_NEGOTIATOR`] で正直に降りて、人 (か supervisor) に
/// `zai negotiate serve` を立てさせる。
///
/// ## 誰の担当として台帳に載るのか
///
/// 交渉役は**要求を送ってきた [`Pid`]** の担当として台帳へ載せる。
/// `--as` を付けなければこの CLI が使い捨ての Pid を起こすので、
/// **プロセスが終われば次の `zai mesh reap` で担当も解放される**
/// (それが正しい: 死んだ要求者の担当を握り続けるほうが事故)。
/// 長く持ちたいなら、既にメッシュに居る呼び出し元の Pid を
/// `--as "$ZAIVERN_MESH_PID"` で渡す — `zai mesh spawn -- <cmd>` が
/// その環境変数を立てるので、エージェント本体の担当として載る。
///
/// ## 引数
///
/// | 引数 | 意味 |
/// |---|---|
/// | `--spec <s>` | 欲しい行域 (`src/a.rs#L10-40`)。**必須** |
/// | `--movable` | 近くの空きへ**ずらしてよい**。付けなければ絶対にずらさない |
/// | `--size-only` | 行数さえ合えばよい (分割も許す)。`--movable` を含む |
/// | `--to <pid>` | 交渉役を名前で探さずに直接指す |
/// | `--as <pid>` | 要求者の Pid (既にメッシュに居るもの)。担当がこの Pid に載る |
/// | `--rounds N` | 返事を待つ周回数 (既定 [`ASK_ROUNDS`]) |
/// | `--max-shift N` | ずらしてよい幅の上限 (行)。`--movable` があるときだけ効く |
///
/// **`--movable` が効くのは、交渉役が `serve --lines N` で
/// ファイルの行数を知っているときだけ**。行数が分からない交渉役は
/// 「存在しない行へ振り替える」より断るほうを選ぶ (`offer` の
/// `file_lines == 0` の枝)。
///
/// ## 終了コード
///
/// | 値 | 意味 |
/// |---|---|
/// | [`EXIT_OK`] | 取れた (ずらして取れた場合も 0。**要求と実際の域を両方出す**) |
/// | [`EXIT_DENIED`] | 断られた |
/// | [`EXIT_USAGE`] | 使い方の誤り |
/// | [`EXIT_NEGOTIATOR`] | 交渉役が居ない |
/// | [`EXIT_NO_REPLY`] | 上限まで待ったが返事が来ない (**断られたのとは別物**) |
/// | [`EXIT_NO_MESH`] | メッシュが無効 |
pub fn ask_cli(argv: &[String]) -> i32 {
    let mut spec: Option<String> = None;
    let mut to: Option<String> = None;
    let mut as_pid: Option<String> = None;
    let mut movable = false;
    let mut size_only = false;
    let mut rounds: u32 = ASK_ROUNDS;
    let mut max_shift: Option<u32> = None;
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--spec" => match it.next() {
                Some(v) => spec = Some(v.clone()),
                None => return usage_err("--spec に値がありません"),
            },
            "--to" => match it.next() {
                Some(v) => to = Some(v.clone()),
                None => return usage_err("--to に値がありません"),
            },
            "--as" => match it.next() {
                Some(v) => as_pid = Some(v.clone()),
                None => return usage_err("--as に値がありません"),
            },
            "--rounds" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => rounds = v,
                None => return usage_err("--rounds は 0 以上の整数です"),
            },
            "--max-shift" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => max_shift = Some(v),
                None => return usage_err("--max-shift は 0 以上の整数です"),
            },
            "--movable" => movable = true,
            "--size-only" => size_only = true,
            other => return usage_err(&format!("知らない引数: {other}")),
        }
    }
    let Some(spec) = spec else {
        return usage_err("--spec が要ります (例: --spec 'src/a.rs#L10-40')");
    };
    let region = match region::parse(&spec) {
        Ok(r) => r,
        Err(why) => return usage_err(&format!("域が読めません: {why}")),
    };

    let cwd = std::env::current_dir().unwrap_or_default();
    let mesh = Mesh::open_for(&cwd);
    if !mesh.enabled() {
        eprintln!("メッシュが有効ではありません (先に `zai mesh spawn` を実行してください)");
        return EXIT_NO_MESH;
    }

    // ① 交渉役を先に探す。**居なければ何も起こさずに降りる**
    //    (自分の Pid を作ってから降りると、要らない登録レコードが残る)。
    let server = match &to {
        Some(s) => match Pid::parse(s) {
            Some(p) => p,
            None => return usage_err(&format!("--to の Pid が読めません: {s}")),
        },
        None => match mesh.whereis(NEGOTIATOR) {
            Some(p) => p,
            None => {
                eprintln!("交渉役が居ません (`zai negotiate serve` を 1 体だけ立ててください)");
                return EXIT_NEGOTIATOR;
            }
        },
    };

    // ② 要求者の Pid。`--as` で呼び出し元の Pid を指せば、担当は
    //    **その長生きするプロセス**のものとして載る (この CLI が終わっても
    //    残る)。指さなければ使い捨ての Pid を起こす。
    let (me, spawned) = match &as_pid {
        Some(s) => match Pid::parse(s) {
            Some(p) if mesh.lookup(&p).is_some() => (p, false),
            Some(p) => return usage_err(&format!("--as の Pid はメッシュに居ません: {p}")),
            None => return usage_err(&format!("--as の Pid が読めません: {s}")),
        },
        None => match mesh.spawn(crate::features::mesh::SpawnOpts {
            role: "asker".into(),
            label: "行域の要求".into(),
            ..Default::default()
        }) {
            Ok(p) => (p.pid, true),
            Err(e) => {
                eprintln!("メッシュに参加できません: {e}");
                return EXIT_NO_MESH;
            }
        },
    };

    // ③ 要求を組み立てる。**`--movable` が無ければ `fixed`** —
    //    行域は行番号ではなく*そこにある内容*に紐づくので、言っていない
    //    要求をずらすと「別の関数を編集しろ」と言ったことになる。
    let mut want = if movable || size_only {
        Want::movable(&me.to_string(), region)
    } else {
        Want::fixed(&me.to_string(), region)
    };
    if size_only {
        want = want.size_only();
    }
    if let Some(n) = max_shift {
        want = want.max_shift(n);
    }

    let asked = region::render(&want.region);
    let verdict = request(&mesh, &me, &server, &want, rounds);
    let code = match &verdict {
        Some(Ok(got)) => {
            let got_spec = region::render(got);
            if got_spec == asked {
                println!("取れました: {got_spec}");
            } else {
                // **黙って別の場所を渡さない。** 要求と実際を必ず並べる。
                println!("ずらして取れました: {asked} → {got_spec}");
            }
            println!("  担当: {me}");
            EXIT_OK
        }
        Some(Err(why)) => {
            println!("断られました: {asked} — {why}");
            if !movable && !size_only {
                println!("  `--movable` を付けると、近くの空き域へずらせるか交渉します");
            }
            EXIT_DENIED
        }
        None => {
            // **断られたのとは別物**。交渉役が回っていない可能性が高い。
            println!(
                "返事がありません: {asked} ({:.1} 秒待ちました)",
                ask_budget(rounds).as_secs_f32()
            );
            println!("  交渉役 {server} が回っているか (`zai mesh list`) を確かめてください");
            EXIT_NO_REPLY
        }
    };

    // ④ 取れていない自前の Pid は片付ける。**取れたときは残す** —
    //    ここで exit すると、いま載せたばかりの担当を自分で解放してしまう。
    if spawned && code != EXIT_OK {
        let _ = mesh.exit(&me, "done");
    }
    code
}

/// 使い方の誤りを 1 行で出して [`EXIT_USAGE`] を返す。
fn usage_err(why: &str) -> i32 {
    eprintln!("zai negotiate ask: {why}");
    EXIT_USAGE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::mesh::SpawnOpts;
    use crate::test_util::unique_temp_dir;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    /// どの OS でも「絶対に生きていない」OS PID。
    ///
    /// `u32::MAX - 1` を使ってはいけない理由は `mesh::tests::DEAD_PID` の
    /// 説明のとおり (unix の `pid_t` は `i32` なので負のプロセスグループ
    /// 問い合わせに化け、**macOS で緑・Linux で赤**になる)。同じ値を使う。
    const DEAD_PID: u32 = 0x7FFF_FFFE;

    fn mesh_at(dir: &Path) -> Mesh {
        Mesh::open_at(dir.to_path_buf(), "test-node")
    }

    fn spawn(m: &Mesh, role: &str) -> Pid {
        m.spawn(SpawnOpts {
            role: role.into(),
            ..Default::default()
        })
        .expect("参加")
        .pid
    }

    /// 「OS からは既に消えている」プロセスを台帳へ登録する。
    /// **本物の子プロセスを起こさない** — CI の Linux ランナーは
    /// プロセスツリーが溜まると死ぬので、PTY もコマンド起動も使わない。
    fn spawn_dead(m: &Mesh, role: &str) -> Pid {
        m.spawn(SpawnOpts {
            role: role.into(),
            os_pid: DEAD_PID,
            ..Default::default()
        })
        .expect("参加")
        .pid
    }

    fn spec(s: &str) -> Region {
        region::parse(s).expect("域が読める")
    }

    /// 交渉役を**別スレッド**で回す。プロセスも PTY も起こさない。
    ///
    /// `Drop` で必ず止めて join する — テストが落ちてもスレッドが残らない。
    struct Serving {
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Serving {
        fn start(dir: &Path, srv: Pid, lines: u32) -> Serving {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let root = dir.to_path_buf();
            let handle = std::thread::spawn(move || {
                let m = Mesh::open_at(root, "test-node");
                while !flag.load(Ordering::Relaxed) {
                    serve_once(&m, &srv, &|_| lines, region::SAFE_BAND);
                    // 交渉役の周回。要求側の `ask_backoff` (25ms〜) より
                    // 細かくしておかないと、待ちが常に 1 段ぶん伸びる。
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            Serving {
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for Serving {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn 素の確保要求はずらさずに答える() {
        let dir = unique_temp_dir("zaivern", "negomesh-plain");
        let m = mesh_at(&dir);
        let srv = spawn(&m, "negotiator");
        let a = spawn(&m, "agent");
        let b = spawn(&m, "agent");

        // A が先に取る
        m.send(
            &srv,
            &a,
            Msg::Claim {
                spec: "src/x.rs#L10-40".into(),
            },
        )
        .expect("送れる");
        let s1 = serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND);
        assert_eq!(s1.granted, vec!["src/x.rs#L10-40".to_string()]);
        assert!(s1.shifted.is_empty());

        // B が重なる域を素の Claim で要求 → **ずらさずに断る**
        m.send(
            &srv,
            &b,
            Msg::Claim {
                spec: "src/x.rs#L20-50".into(),
            },
        )
        .expect("送れる");
        let s2 = serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND);
        assert!(s2.granted.is_empty(), "重なったのに通した");
        assert!(
            s2.shifted.is_empty(),
            "movable と言っていない要求をずらした"
        );
        assert_eq!(s2.denied.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ずらしてよいと言えば近くの空きへ振り替わる() {
        let dir = unique_temp_dir("zaivern", "negomesh-shift");
        let m = mesh_at(&dir);
        let srv = spawn(&m, "negotiator");
        let a = spawn(&m, "agent");
        let b = spawn(&m, "agent");

        m.send(
            &srv,
            &a,
            Msg::Claim {
                spec: "src/x.rs#L10-40".into(),
            },
        )
        .expect("送れる");
        serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND);

        let want = Want::movable("b1", region::parse("src/x.rs#L20-50").expect("解釈"));
        m.send(
            &srv,
            &b,
            Msg::Custom {
                kind: DEAL_KIND.into(),
                body: negotiate::encode(&Deal::Propose {
                    from: b.to_string(),
                    want: want.clone(),
                }),
            },
        )
        .expect("送れる");
        let s = serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND);
        assert_eq!(s.granted.len(), 1, "ずらせば通るはずが通っていない");
        assert_eq!(s.shifted.len(), 1, "ずらした記録が無い");

        // **不変条件**: 台帳に載った担当は互いに素
        let held: Vec<Region> = m
            .claims()
            .iter()
            .filter_map(|c| region::parse(&c.spec).ok())
            .collect();
        assert!(
            region::is_disjoint(&held, region::SAFE_BAND),
            "重なった担当が載った: {:?}",
            region::conflicting_pairs(&held, region::SAFE_BAND)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 読めない要求にも必ず返事を返す() {
        let dir = unique_temp_dir("zaivern", "negomesh-bad");
        let m = mesh_at(&dir);
        let srv = spawn(&m, "negotiator");
        let a = spawn(&m, "agent");
        m.send(
            &srv,
            &a,
            Msg::Claim {
                spec: "src/x.rs#L9-2".into(), // 空の域
            },
        )
        .expect("送れる");
        let s = serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND);
        assert_eq!(s.unreadable, 1);
        let back = m.recv(&a);
        assert!(
            back.iter().any(|e| matches!(&e.msg, Msg::Denied { .. })),
            "黙って落とすと送り手が永遠に待つ"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 要求側は返事を待って結果を受け取る() {
        let dir = unique_temp_dir("zaivern", "negomesh-req");
        let m = mesh_at(&dir);
        let srv = spawn(&m, "negotiator");
        let a = spawn(&m, "agent");

        let want = Want::fixed("a1", region::parse("src/x.rs#L10-40").expect("解釈"));
        // 先に要求を積んでおき、交渉役が 1 回回してから受け取る
        // (request は上限つきで待つので、答えが先に居ても取りこぼさない)。
        m.send(
            &srv,
            &a,
            Msg::Claim {
                spec: region::render(&want.region),
            },
        )
        .expect("送れる");
        serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND);
        let got = request(&m, &a, &srv, &want, 3).expect("返事が来る");
        assert!(got.is_ok(), "通るはずが断られた: {got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 別ファイルの要求も同じ周で片付ける() {
        let dir = unique_temp_dir("zaivern", "negomesh-multifile");
        let m = mesh_at(&dir);
        let srv = spawn(&m, "negotiator");
        let a = spawn(&m, "agent");
        let b = spawn(&m, "agent");

        // わざと辞書順の後ろから積む。「先頭の要求と同じファイルだけ」を
        // 処理する実装だと、`src/a.rs` が受信箱に残って b だけが待たされる。
        m.send(
            &srv,
            &a,
            Msg::Claim {
                spec: "src/z.rs#L10-40".into(),
            },
        )
        .expect("送れる");
        m.send(
            &srv,
            &b,
            Msg::Claim {
                spec: "src/a.rs#L10-40".into(),
            },
        )
        .expect("送れる");

        let s = serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND);
        assert_eq!(
            s.granted,
            vec!["src/a.rs#L10-40".to_string(), "src/z.rs#L10-40".to_string()],
            "1 周で全ファイルを処理していない (並びはパスの辞書順)"
        );
        // **返事も両方へ届いている。** 台帳だけ更新して黙っていると、
        // 送り手は上限まで待ってから「返事が来ない」で落ちる。
        assert!(
            m.recv(&a)
                .iter()
                .any(|e| matches!(&e.msg, Msg::Granted { .. })),
            "src/z.rs の要求者へ返事が来ていない"
        );
        assert!(
            m.recv(&b)
                .iter()
                .any(|e| matches!(&e.msg, Msg::Granted { .. })),
            "src/a.rs の要求者へ返事が来ていない"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn メッシュ越しに要求して返事を受け取る往復() {
        let dir = unique_temp_dir("zaivern", "negomesh-roundtrip");
        let m = mesh_at(&dir);
        let srv = spawn(&m, "negotiator");
        m.register(NEGOTIATOR, &srv).expect("名乗れる");
        assert_eq!(m.whereis(NEGOTIATOR), Some(srv.clone()), "名前で引ける");
        let a = spawn(&m, "agent");
        let b = spawn(&m, "agent");
        let c = spawn(&m, "agent");
        // 交渉役を裏で回す (ここから先は本物の往復)。
        let _serving = Serving::start(&dir, srv.clone(), 2000);

        // ① A は不動で #L10-40 → そのまま取れる
        let wa = Want::fixed(&a.to_string(), spec("src/a.rs#L10-40"));
        let ra = request(&m, &a, &srv, &wa, ASK_ROUNDS)
            .expect("返事が来る")
            .expect("誰とも重なっていないのに断られた");
        assert_eq!(region::render(&ra), "src/a.rs#L10-40");

        // ② B は重なる #L20-50 を**不動**で → 断られ、**ずらし先を勧められない**
        let wb = Want::fixed(&b.to_string(), spec("src/a.rs#L20-50"));
        let why = request(&m, &b, &srv, &wb, ASK_ROUNDS)
            .expect("返事が来る")
            .expect_err("重なっているのに通した");
        assert!(
            !why.contains("ずらす"),
            "movable と言っていない要求へずらし先を勧めた: {why}"
        );
        assert!(
            why.contains(&a.to_string()),
            "持ち主が誰かを言っていない: {why}"
        );

        // ③ C は重なる域を **--movable** で → 近くの空きへずらして取れる
        let wc = Want::movable(&c.to_string(), spec("src/a.rs#L20-50"));
        let rc = request(&m, &c, &srv, &wc, ASK_ROUNDS)
            .expect("返事が来る")
            .expect("ずらせば通るはずが断られた");
        assert_ne!(
            region::render(&rc),
            "src/a.rs#L20-50",
            "ずらしていない (重なったまま通した)"
        );
        assert_eq!(
            rc.span.expect("行域がある").len(),
            31,
            "行数が変わった (要求は 31 行)"
        );

        // ④ **不変条件**: 台帳に載った担当は互いに素
        let held: Vec<Region> = m
            .claims()
            .iter()
            .filter_map(|c| region::parse(&c.spec).ok())
            .collect();
        assert_eq!(held.len(), 2, "通った 2 件だけが載っている: {held:?}");
        assert!(
            region::is_disjoint(&held, region::SAFE_BAND),
            "重なった担当が載った: {:?}",
            region::conflicting_pairs(&held, region::SAFE_BAND)
        );
        drop(_serving);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 返事が来ないのと断られたのは区別できる() {
        let dir = unique_temp_dir("zaivern", "negomesh-noreply");
        let m = mesh_at(&dir);
        // 交渉役は登録するが**一度も回さない**。
        let srv = spawn(&m, "negotiator");
        let a = spawn(&m, "agent");
        let want = Want::fixed(&a.to_string(), spec("src/a.rs#L10-40"));

        let t0 = Instant::now();
        let got = request(&m, &a, &srv, &want, 3);
        let waited = t0.elapsed();
        assert!(got.is_none(), "誰も答えていないのに返事が出た: {got:?}");
        // 上限つき: 3 周ぶん (25+50+100ms) 待って諦める。**永遠に待たない**。
        assert!(
            waited >= ask_budget(3) && waited < ask_budget(3) + Duration::from_secs(5),
            "待ち時間が予算どおりでない: {waited:?} (予算 {:?})",
            ask_budget(3)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 要求側の待ちは二十五ミリ秒から始まり二秒で頭打ちになる() {
        // mesh::backoff は 2 秒からなので、そのまま使うと即答が 2 秒に化ける。
        assert_eq!(ask_backoff(0), Duration::from_millis(25));
        assert_eq!(ask_backoff(1), Duration::from_millis(50));
        assert_eq!(ask_backoff(6), Duration::from_millis(1600));
        assert_eq!(ask_backoff(7), Duration::from_millis(2000), "頭打ち");
        assert_eq!(ask_backoff(99), Duration::from_millis(2000), "桁溢れしない");
        assert_eq!(ask_budget(ASK_ROUNDS), Duration::from_millis(5175));
    }

    #[test]
    fn 落ちた要求者の担当は_reap_が自動で解放する() {
        let dir = unique_temp_dir("zaivern", "negomesh-reap");
        let m = mesh_at(&dir);
        let srv = spawn(&m, "negotiator");
        // **応答も残さずに死ぬ**エージェント (OS からは既に消えている)。
        let ghost = spawn_dead(&m, "agent");
        m.send(
            &srv,
            &ghost,
            Msg::Claim {
                spec: "src/x.rs#L10-40".into(),
            },
        )
        .expect("送れる");
        assert_eq!(
            serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND).granted,
            vec!["src/x.rs#L10-40".to_string()]
        );

        // ① reap が担当を自動で解放する (人は 1 バイトも掃除しない)。
        let t0 = Instant::now();
        let rep = m.reap();
        let reap_took = t0.elapsed();
        assert_eq!(rep.dead, vec![ghost.to_string()]);
        assert_eq!(rep.released, vec!["src/x.rs#L10-40".to_string()]);
        assert!(m.claims().is_empty(), "担当が残った");
        // **待ち時間はゼロ**: 生死の一次情報は OS の生存確認なので、
        // 心拍のタイムアウト (STALE 60 秒 / HARD_STALE 30 分) を待たない。
        assert!(
            reap_took < Duration::from_secs(1),
            "reap が 1 秒以上かかった: {reap_took:?}"
        );

        // ② 解放されたので、他の要求者が**同じ域を**取れる。
        let b = spawn(&m, "agent");
        m.send(
            &srv,
            &b,
            Msg::Claim {
                spec: "src/x.rs#L10-40".into(),
            },
        )
        .expect("送れる");
        let s = serve_once(&m, &srv, &|_| 2000, region::SAFE_BAND);
        assert_eq!(
            s.granted,
            vec!["src/x.rs#L10-40".to_string()],
            "解放後なのに取れない (死人の担当を握ったまま)"
        );

        // ③ **冪等**: もう一度刈っても何も起きない (生きている b は残る)。
        let again = m.reap();
        assert!(again.released.is_empty(), "生きている担当まで解放した");
        assert_eq!(m.claims().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 交渉役が落ちたら名前が空いて次の一体が名乗れる() {
        let dir = unique_temp_dir("zaivern", "negomesh-failover");
        let m = mesh_at(&dir);
        let dead_srv = spawn_dead(&m, "negotiator");
        m.register(NEGOTIATOR, &dead_srv).expect("名乗れる");
        let next = spawn(&m, "negotiator");
        assert!(
            m.register(NEGOTIATOR, &next).is_err(),
            "刈る前に 2 体目が名乗れると、重なる 2 件の両方へ通ると答えられる"
        );

        let t0 = Instant::now();
        let rep = m.reap();
        let took = t0.elapsed();
        assert_eq!(
            rep.unnamed,
            vec![NEGOTIATOR.to_string()],
            "名前が外れていない"
        );
        assert_eq!(m.whereis(NEGOTIATOR), None);
        assert!(took < Duration::from_secs(1), "刈るのに 1 秒以上: {took:?}");

        // 空いたので次の 1 体が名乗れる = 交渉が止まらない。
        m.register(NEGOTIATOR, &next).expect("名乗れる");
        assert_eq!(m.whereis(NEGOTIATOR), Some(next));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn askの引数検査はメッシュに触る前に落ちる() {
        // ここで検査しているのは**メッシュを開く前の枝だけ**。
        // 実 `~/.zaivern` を触らないため、それ以降は通らない引数を渡す。
        let a = |args: &[&str]| {
            let mut v = vec!["ask".to_string()];
            v.extend(args.iter().map(|s| s.to_string()));
            ask_cli(&v)
        };
        assert_eq!(a(&[]), EXIT_USAGE, "--spec が無い");
        assert_eq!(a(&["--spec"]), EXIT_USAGE, "--spec に値が無い");
        assert_eq!(a(&["--spec", "src/a.rs#L9-2"]), EXIT_USAGE, "空の域");
        assert_eq!(a(&["--spec", "src/a.rs#L1-9", "--rounds"]), EXIT_USAGE);
        assert_eq!(
            a(&["--spec", "src/a.rs#L1-9", "--rounds", "たくさん"]),
            EXIT_USAGE
        );
        assert_eq!(a(&["--spec", "src/a.rs#L1-9", "--to"]), EXIT_USAGE);
        assert_eq!(a(&["--spec", "src/a.rs#L1-9", "--as"]), EXIT_USAGE);
        assert_eq!(a(&["--しらない"]), EXIT_USAGE);
    }

    #[test]
    fn 終了コードは全部違う値を持つ() {
        // 同じ番号を 2 つの意味に使うと、呼び出し側が
        // 「断られた」と「返事が来ない」を取り違える。
        let all = [
            EXIT_OK,
            EXIT_DENIED,
            EXIT_USAGE,
            EXIT_NEGOTIATOR,
            EXIT_NO_REPLY,
            EXIT_NO_MESH,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "終了コードが重複している: {all:?}");
        assert_eq!(sorted, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn 行数はファイルごとに解く() {
        let dir = unique_temp_dir("zaivern", "negomesh-lines");
        std::fs::create_dir_all(dir.join("src")).expect("作れる");
        let short = (1..=30).map(|i| format!("l{i}\n")).collect::<String>();
        let long = (1..=900).map(|i| format!("l{i}\n")).collect::<String>();
        std::fs::write(dir.join("src/short.rs"), &short).expect("書ける");
        std::fs::write(dir.join("src/long.rs"), &long).expect("書ける");

        assert_eq!(lines_from_disk(&dir, "src/short.rs"), 30);
        assert_eq!(lines_from_disk(&dir, "src/long.rs"), 900);
        // 読めないものは 0 = 「上限不明」。ずらし先を勧めない側へ倒す。
        assert_eq!(lines_from_disk(&dir, "src/nope.rs"), 0);
        assert_eq!(lines_from_disk(&dir, "src"), 0, "ディレクトリは 0");

        // **1 周で 2 ファイルを捌いても、それぞれの行数で判断する。**
        // 定数 1 つで判断していた頃は、2 ファイル目が常に間違った上限だった。
        let m = mesh_at(&dir.join("mesh"));
        let srv = spawn(&m, "negotiator");
        let a = spawn(&m, "agent");
        for spec in ["src/short.rs#L1-10", "src/long.rs#L800-820"] {
            m.send(&srv, &a, Msg::Claim { spec: spec.into() })
                .expect("送れる");
        }
        let root = dir.clone();
        let s = serve_once(
            &m,
            &srv,
            &move |rel: &str| lines_from_disk(&root, rel),
            region::SAFE_BAND,
        );
        assert_eq!(s.granted.len(), 2, "2 ファイルとも 1 周で捌けていない");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 交渉役は一体しか名乗れない() {
        let dir = unique_temp_dir("zaivern", "negomesh-one");
        let m = mesh_at(&dir);
        let p1 = spawn(&m, "negotiator");
        let p2 = spawn(&m, "negotiator");
        assert!(m.register("negotiator", &p1).is_ok());
        assert!(
            m.register("negotiator", &p2).is_err(),
            "2 体目が名乗れてしまうと、重なる 2 件の両方へ通ると答えられる"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
