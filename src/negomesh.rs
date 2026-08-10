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

use crate::features::mesh::{Mesh, Msg, Pid};
use crate::features::negotiate;
use crate::region::{self, Region};
use negotiate::{Deal, Offer, Want};

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
/// `file_lines` は対象ファイルの行数。**呼び出し側が知っている値**を渡す
/// (この層はファイルを読まない — 短命なフックプロセスから呼ばれても
///  ディスクを触らずに済ませたいため)。0 を渡すと「上限不明」として扱い、
/// ずらし先を提案しない (知らない場所を勧めない)。
pub fn serve_once(mesh: &Mesh, me: &Pid, file_lines: u32, band: u32) -> Served {
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

    // ② **まとめて**決める。1 件ずつ答えると、互いに重なる 2 件の両方へ
    //    「通る」と答えてしまう。
    let path = asks[0].1.region.path.clone();
    let occupied = occupied_of(mesh, &path);
    let wants: Vec<Want> = asks
        .iter()
        .filter(|(_, w, _)| w.region.path == path)
        .map(|(_, w, _)| w.clone())
        .collect();
    let plan = negotiate::allocate(&wants, &occupied, file_lines, band);

    // ③ 返事を出し、通ったぶんはメッシュの台帳にも載せる。
    for (from, want, is_deal) in &asks {
        if want.region.path != path {
            // 別ファイルの要求は次の周で扱う (1 回に 1 ファイル)。
            continue;
        }
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
                let off = negotiate::offer(want, &occupied, file_lines, band);
                let (holder, hint) = describe(&off);
                out.denied
                    .push((region::render(&want.region), holder.clone(), hint.clone()));
                reply_denied(mesh, me, from, want, &holder, &hint, *is_deal);
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

/// 要求する側。**確保を頼んで、返事を待つ。**
///
/// `movable` が `true` なら [`Deal`] 形式で送る (= ずらしてよいと明示する)。
/// `false` なら素の [`Msg::Claim`] を送る — 相手はずらし先を提案しない。
///
/// 待ちは**上限つき**で、来なければ `None` を返す。永遠に待たないのが
/// この製品の約束 (設計原則 2: 隠れている処理は欠落ありでよいが、
/// 決してブロックさせない)。
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
    for round in 0..rounds {
        for e in mesh.recv(me) {
            match e.msg {
                Msg::Granted { spec } => {
                    return Some(region::parse(&spec).map_err(|e| e.to_string()))
                }
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
        std::thread::sleep(crate::features::mesh::backoff(round));
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════════
//  CLI — `zai negotiate serve`
// ═══════════════════════════════════════════════════════════════════════════

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
/// | 0 | 回し終えた |
/// | 2 | 使い方の誤り |
/// | 3 | 既に交渉役が居る (名前が取られている) |
/// | 4 | メッシュが無効 (`~/.zaivern/mesh/…` が無い) |
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
                None => return 2,
            },
            "--lines" => match val(&mut it) {
                Some(v) => lines = v,
                None => return 2,
            },
            "--band" => match val(&mut it) {
                Some(v) => band = v,
                None => return 2,
            },
            _ => return 2,
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let mesh = Mesh::open_for(&cwd);
    if !mesh.enabled() {
        eprintln!("メッシュが有効ではありません (先に `zai mesh join` を実行してください)");
        return 4;
    }
    let Ok(me) = mesh.spawn(crate::features::mesh::SpawnOpts {
        role: "negotiator".into(),
        label: "行域の交渉役".into(),
        trap_exit: true,
        ..Default::default()
    }) else {
        eprintln!("メッシュに参加できません");
        return 4;
    };
    if mesh.register("negotiator", &me.pid).is_err() {
        eprintln!("既に交渉役が居ます (1 リポジトリに 1 体だけ)");
        let _ = mesh.exit(&me.pid, "duplicate-negotiator");
        return 3;
    }
    let mut total = Served::default();
    for round in 0..rounds {
        mesh.beat(&me.pid);
        let s = serve_once(&mesh, &me.pid, lines, band);
        let idle = s.is_idle();
        total.granted.extend(s.granted);
        total.denied.extend(s.denied);
        total.shifted.extend(s.shifted);
        total.unreadable += s.unreadable;
        if idle && round + 1 < rounds {
            // 何も来ていない周は寝る (アイドル時のコストはゼロ)。
            std::thread::sleep(crate::features::mesh::backoff(round));
        }
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
    let _ = mesh.unregister("negotiator", &me.pid);
    let _ = mesh.exit(&me.pid, "done");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::mesh::SpawnOpts;
    use crate::test_util::unique_temp_dir;

    fn mesh_at(dir: &std::path::Path) -> Mesh {
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
        let s1 = serve_once(&m, &srv, 2000, region::SAFE_BAND);
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
        let s2 = serve_once(&m, &srv, 2000, region::SAFE_BAND);
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
        serve_once(&m, &srv, 2000, region::SAFE_BAND);

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
        let s = serve_once(&m, &srv, 2000, region::SAFE_BAND);
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
        let s = serve_once(&m, &srv, 2000, region::SAFE_BAND);
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
        serve_once(&m, &srv, 2000, region::SAFE_BAND);
        let got = request(&m, &a, &srv, &want, 3).expect("返事が来る");
        assert!(got.is_ok(), "通るはずが断られた: {got:?}");
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
