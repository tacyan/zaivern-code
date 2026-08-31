//! 短い指示を **仕様書へ書き換える** 前段 (Spec Writer)。
//!
//! ## なぜ要るか (実測)
//!
//! Team の計画は SPEC の箇条書きを機械的に割る。だから
//! 「かっこいい３DのWebページを作って」のような**一行の指示**では実装
//! タスクが 1 件にしかならず、4 体立てても**1 体しか働かない**。
//! 実機の `state.json` がまさにそうなっていた (実装 1 件 + 統合 1 件、
//! 2 体目は最後まで仕事ゼロ)。
//!
//! そこで、計画を作る**前に**「使えるエージェント」へ一度だけ渡して、
//! 指示を SPEC.md の形へ書き換えてもらう。書き換えた結果は**人に見せて
//! 確認を取ってから**計画へ進む — 勝手に膨らませた仕様で走り出すと、
//! 頼んでいない物ができる。
//!
//! ## ここに置くもの / 置かないもの
//!
//! * 置く: 依頼文の組み立て・出力の取り出し・下書きの妥当性 (**純関数**)
//! * 置かない: どのエージェントを使うかの判断 (アプリ側が持つ設定なので
//!   `app::team_glue` が決める)。ここは「実体のパスと引数」を受け取るだけ
//!
//! ## エコーを読まない
//!
//! 実行は**ヘッドレス** (`claude -p …` 等) で、読むのは stdout。
//! 端末画面ではないので、`result_parser` が踏んだ「自分の依頼文を相手の
//! 答えとして読む」経路はここには無い。それでも取り出しは**最後の塊**を
//! 採る — 依頼文を復唱してから答える CLI があっても答えのほうを採るため。

use std::path::Path;
use std::time::Duration;

use super::model::TeamRole;

/// 下書きの開始・終了マーカー。
pub const SPEC_OPEN: &str = "[ZAI-TEAM-SPEC]";
pub const SPEC_CLOSE: &str = "[/ZAI-TEAM-SPEC]";

/// 書き換えに待つ上限。
///
/// **短くしない。** 考えるエージェントは 1 分を普通に超える。ここで
/// 切ると「毎回失敗する機能」になり、誰も使わなくなる。
pub const DRAFT_TIMEOUT: Duration = Duration::from_secs(300);

/// 下書き 1 本の上限。**自前で持たない** —
/// 取り出しに使う [`super::result_parser::extract_blocks`] が既に
/// [`super::result_parser::BLOCK_MAX_BYTES`] で切っているので、ここに
/// 別の数を置くと上限が 2 つになり、どちらで落ちたのか誰にも分からなくなる。
pub const DRAFT_MAX_BYTES: usize = super::result_parser::BLOCK_MAX_BYTES;

/// 依頼文を組み立てる (純関数)。
///
/// **計画が読める形を、そのまま指示する。** `planner::parse_sections` は
/// 「`##` 見出し + 箇条書き」しか見ないので、その形を外すと書き換えても
/// タスクは分かれない (書き換えた意味が無くなる)。
pub fn build_prompt(
    goal: &str,
    brief: &str,
    agents: usize,
    roles: &[TeamRole],
    validations: &[String],
) -> String {
    let lanes: Vec<&str> = roles.iter().map(|r| r.key()).collect();
    let goal = goal.trim();
    let title = if goal.is_empty() { "(名前なし)" } else { goal };
    format!(
        "あなたは開発チームの仕様書きです。**コードは 1 行も書かないでください。**\n\
         次の短い依頼を、チームが分担して実装できる SPEC.md に書き換えてください。\n\n\
         ## 依頼\n\
         Goal 名: {title}\n\
         内容: {brief}\n\n\
         ## 編成\n\
         最大 {agents} 体が同時に動きます。用意する役割: {lanes}\n\n\
         ## 出力の形 (厳守)\n\
         下の 2 行のマーカーで挟んで、間に Markdown だけを書いてください。\n\
         マーカーの外に説明を書いてもかまいません。\n\n\
         {SPEC_OPEN}\n\
         # <表題>\n\n\
         ## タスク\n\
         - <1 人が独立して進められる単位> (files: <担当ファイル>)\n\
         - <同上>\n\n\
         ## 完了条件\n\
         - <満たされたら完成と言える条件>\n\n\
         ## 検証\n\
         - <実際に走らせるコマンド 1 行>\n\
         {SPEC_CLOSE}\n\n\
         ## 守ること\n\
         * 「## タスク」の箇条書きは **{min} 件以上 {max} 件以下**。\
           1 件だと分担にならず、多すぎると割り当てが往復するだけで進まない\n\
         * 各タスクは**互いに別のファイル**を触るように割る \
           (`(files: index.html)` の形で書く)。同じファイルを 2 人に配ると衝突する\n\
         * 依頼に書いていない機能を足さない。分からないことは決めつけず、\
           その旨をタスクの文言に残す\n\
         * 「## 検証」は**下の「使える検証コマンド」からだけ**選ぶ。\
           1 つも当てはまらなければ「## 検証」ごと省く (嘘のコマンドを書かない)\n\
         * シェルの記法 (`&&` `|` `;`) は使えない。1 行に 1 コマンド\n\
         * **パス指定は使えない** (`tools/verify.sh` や `./run.sh` は不可)。\
           PATH から解決される名前だけ (`cargo`, `npm`, `pytest` など)\n\n\
         ## 使える検証コマンド\n{validations}",
        lanes = if lanes.is_empty() {
            "implementer".to_string()
        } else {
            lanes.join(", ")
        },
        min = MIN_TASKS,
        max = agents.max(2) * 2,
        validations = validation_menu(validations),
    )
}

/// 使える検証コマンドの一覧を、依頼文へ載せる形にする。
///
/// **こちらが実際に走らせられるものだけを見せる。** 見せないと
/// エージェントは想像で書き、`tools/verify.sh --quick` のような
/// **走らせられないコマンド**が返ってくる (実測: それで計画がまるごと
/// 断られた)。候補が無いなら「無い」と正直に伝えて省かせる。
fn validation_menu(v: &[String]) -> String {
    if v.is_empty() {
        return "(このリポジトリでは自動で決められませんでした。\
                「## 検証」は省いてください)\n"
            .to_string();
    }
    v.iter().map(|c| format!("* `{c}`\n")).collect()
}

/// 下書きの「## 検証」から、**走らせられない行を落とす**。
///
/// エージェントは形式を守っていても、`tools/verify.sh --quick` のような
/// パス指定や `npm test && npm run lint` のようなシェル記法を書いてくる。
/// そのまま SPEC にすると計画が**まるごと**断られる (実測) —
/// 検証 1 行のために、書き換えた仕様書ごと捨てることになる。
///
/// **落としたものは戻り値で返す。** 黙って消すと「書いたのに走らない」に
/// なるので、呼ぶ側が人へ見せる。全部落ちたら「## 検証」ごと消えるので、
/// 計画側がリポジトリを見て自動で決め直す
/// ([`super::validation_defaults::detect`])。
pub fn strip_unrunnable_validation(draft: &str) -> (String, Vec<String>) {
    let mut out: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut in_validation = false;
    for line in draft.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            in_validation = super::planner::is_validation_heading(t.trim_start_matches('#').trim());
            out.push(line.to_string());
            continue;
        }
        if !in_validation {
            out.push(line.to_string());
            continue;
        }
        let Some(body) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .map(|b| b.trim().trim_matches('`').trim())
        else {
            out.push(line.to_string());
            continue;
        };
        if super::graph::parse_command(body).is_ok() {
            out.push(line.to_string());
        } else {
            dropped.push(body.to_string());
        }
    }
    let mut text = out.join("\n");
    // 中身が 1 行も残らなかった「## 検証」は、見出しごと落とす
    // (空の節を残すと、書いてあるのに何も走らないように見える)。
    if !dropped.is_empty() {
        text = drop_empty_validation_section(&text);
    }
    (text, dropped)
}

/// 中身の無くなった「## 検証」の節を消す。
fn drop_empty_validation_section(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        let is_head = t.starts_with('#')
            && super::planner::is_validation_heading(t.trim_start_matches('#').trim());
        if !is_head {
            out.push(lines[i]);
            i += 1;
            continue;
        }
        // 次の見出しまでに箇条書きが 1 つでもあれば残す。
        let mut j = i + 1;
        let mut has_item = false;
        while j < lines.len() && !lines[j].trim().starts_with('#') {
            let t = lines[j].trim();
            if t.starts_with("- ") || t.starts_with("* ") {
                has_item = true;
            }
            j += 1;
        }
        if has_item {
            out.extend(&lines[i..j]);
        }
        i = j;
    }
    out.join("\n")
}

/// 下書きに求める最小のタスク数。**1 件では分担にならない。**
pub const MIN_TASKS: usize = 2;

/// エージェントの出力から下書きを取り出す (純関数)。
///
/// **依頼文のエコーを答えとして採らない。**
/// [`build_prompt`] は出力の形を見せるためにマーカーごと雛形を載せている。
/// 復唱する CLI や、何も考えずに雛形を返すエージェントがあると、
/// `# <表題>` のままの雛形が「書き換えた仕様書」として通ってしまう —
/// `result_parser` が実際に踏んだのと同じ穴なので、同じ番人
/// ([`super::result_parser::is_prompt_echo`]) を通す。
///
/// 残ったうち**最後の塊**を採る (考え直して 2 回出す CLI があるため)。
pub fn extract(stdout: &str, sent: &str) -> Option<String> {
    let text = super::result_parser::extract_blocks(stdout, SPEC_OPEN, SPEC_CLOSE)
        .into_iter()
        .rfind(|b| !super::result_parser::is_prompt_echo(b, sent, SPEC_OPEN, SPEC_CLOSE))?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // **走らせられない検証はここで落とす。** 落とさないと、検証 1 行の
    // ために書き換えた仕様書がまるごと断られる。
    let (clean, _dropped) = strip_unrunnable_validation(text);
    Some(clean)
}

/// 取り出せなかったとき、**なぜ取り出せなかったのかを言い分ける**。
///
/// 「マーカーがありません」で一括りにすると、大きすぎて捨てられた場合に
/// 直しようのない案内になる (エージェントは形式どおりに出しているのに
/// 「形式どおりに出してください」と言われる)。
pub fn why_no_draft(stdout: &str) -> String {
    if !stdout.contains(SPEC_OPEN) {
        return format!("下書きを取り出せませんでした ({SPEC_OPEN} … {SPEC_CLOSE} が出力にありません)");
    }
    if stdout.len() > DRAFT_MAX_BYTES {
        return format!(
            "下書きが大きすぎます ({} バイトまで)。SPEC を短く書き直してもらってください",
            DRAFT_MAX_BYTES
        );
    }
    format!("下書きが空でした ({SPEC_OPEN} と {SPEC_CLOSE} の間に何もありません)")
}

/// 下書きを受け取ってよいか (純関数)。
///
/// **「書き換えた」と言えるのは、計画が分かれるようになったときだけ。**
/// 1 件にしかならない下書きを通すと、確認まで出しておいて元と同じ結果に
/// なる — 一番たちの悪い「効いているように見えて効いていない」。
pub fn accept(draft: &str) -> Result<(), String> {
    if draft.trim().is_empty() {
        return Err("下書きが空でした".to_string());
    }
    if super::planner::needs_spec_rewrite(draft) {
        return Err(format!(
            "下書きのタスクが {MIN_TASKS} 件に届きませんでした \
             (このままでは 1 体しか働きません)"
        ));
    }
    Ok(())
}

/// 実体を起こして下書きを 1 本作る。
///
/// `program` は**解決済みの絶対パス**、`args` は起動引数 (依頼文は最後に
/// 足される)。判断も解決もここではしない — 呼ぶ側が済ませておく。
///
/// **ランナーは既存のものを使う** ([`super::launch::run_resolved_capped`])。
/// 時間切れ・停止・木ごとの後始末を 2 か所に持たない。
pub fn draft_with(
    program: &Path,
    args: &[String],
    cwd: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut argv: Vec<&str> = args.iter().map(String::as_str).collect();
    argv.push(prompt);
    let cancel: super::launch::CancelFlag = Default::default();
    let pid: super::launch::PidSlot = Default::default();
    let (code, why, out) = super::launch::run_resolved_capped(
        program,
        &argv,
        cwd,
        timeout,
        &cancel,
        &pid,
        // **成果物そのものなので 1 文字も捨てない。**
        usize::MAX,
    );
    use super::model::ValidationOutcome as V;
    match why {
        V::Passed => extract(&out.stdout, prompt).ok_or_else(|| why_no_draft(&out.stdout)),
        V::TimedOut => Err(format!(
            "{} 秒待っても返ってきませんでした",
            timeout.as_secs()
        )),
        V::SpawnFailed => Err("エージェントを起動できませんでした".to_string()),
        V::Cancelled => Err("中止しました".to_string()),
        _ => Err(format!(
            "エージェントが失敗しました (終了コード {code}){}",
            first_line(&out.stderr)
        )),
    }
}

/// stderr の 1 行目だけを「: …」の形で添える (空なら何も足さない)。
fn first_line(s: &str) -> String {
    match s.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(l) => format!(": {}", l.chars().take(200).collect::<String>()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::team::imp::model::TeamRole as R;

    fn prompt() -> String {
        build_prompt(
            "テスト",
            "かっこいい３DのWebページを作って",
            4,
            &[R::Architect, R::Implementer, R::Tester],
            &["cargo fmt --check".to_string(), "cargo test".to_string()],
        )
    }

    /// **依頼文には、計画が読める形がそのまま載っている。**
    /// 形を見せないと、書き換えてもタスクは分かれない。
    #[test]
    fn 依頼文は計画が読める形を指示する() {
        let p = prompt();
        for must in [SPEC_OPEN, SPEC_CLOSE, "## タスク", "## 完了条件", "files:"] {
            assert!(p.contains(must), "依頼文に {must} が無い");
        }
        // 編成の情報が伝わっている (伝えないと粒度が決まらない)。
        assert!(p.contains("architect"), "役割が伝わっていない");
        assert!(p.contains("最大 4 体"), "同時数が伝わっていない");
    }

    /// **依頼文のエコーを「書き換えた仕様書」として採らない。**
    ///
    /// `build_prompt` は出力の形を見せるためにマーカーごと雛形を載せている。
    /// 素直に取り出すと、雛形 (`# <表題>` のまま) が下書きとして通る —
    /// `result_parser` が実機で踏んだのと同じ穴。
    #[test]
    fn 依頼文の雛形を下書きとして採らない() {
        let p = prompt();
        // 雛形はマーカーに囲まれているので、素の取り出しでは 1 件見える。
        let raw = super::super::result_parser::extract_blocks(&p, SPEC_OPEN, SPEC_CLOSE);
        assert_eq!(raw.len(), 1, "雛形が依頼文に載っていること自体は前提");
        // それでも下書きとしては採らない。
        assert_eq!(extract(&p, &p), None, "雛形を下書きにしてはいけない");
    }

    /// **答えは採る。** エコー除けが全部を飲み込んだら、今度は永久に
    /// 「取り出せませんでした」になる (直したつもりで別の壊し方)。
    #[test]
    fn 復唱の後ろにある本物の答えを採る() {
        let p = prompt();
        let answer = "# 3D の Web ページ\n\n\
                      ## タスク\n\
                      - 土台の HTML を書く (files: index.html)\n\
                      - three.js の場面を作る (files: scene.js)\n\n\
                      ## 完了条件\n\
                      - ブラウザで球体が回る\n";
        // CLI が依頼文を復唱してから答える形。
        let out = format!("{p}\n\n{SPEC_OPEN}\n{answer}\n{SPEC_CLOSE}\n");
        let got = extract(&out, &p).expect("答えを取り出せる");
        assert!(got.starts_with("# 3D の Web ページ"), "{got}");
        assert!(accept(&got).is_ok(), "2 件に分かれているので受け取れる");
    }

    /// **1 件にしかならない下書きは受け取らない。**
    ///
    /// 通すと、確認まで出しておいて結果は元と同じになる — 一番たちの悪い
    /// 「効いているように見えて効いていない」。
    #[test]
    fn 分担にならない下書きは断る() {
        assert!(accept("").is_err(), "空は受け取らない");
        assert!(
            accept("# だいたい\n\n三次元のページを作ります。\n").is_err(),
            "箇条書きが無ければ 1 件にしかならない"
        );
        assert!(accept(
            "# ページ\n\n## タスク\n- 土台 (files: index.html)\n- 場面 (files: scene.js)\n"
        )
        .is_ok());
    }

    /// **大きすぎる下書きは、理由を言い分けて断る。**
    ///
    /// `extract_blocks` は上限を超えた塊を黙って捨てるので、素直に書くと
    /// 「マーカーがありません」と言ってしまう — エージェントは形式どおりに
    /// 出しているのに「形式どおりに出してください」と返る、直しようのない案内になる。
    #[test]
    fn 大きすぎる下書きは理由を言い分ける() {
        let p = prompt();
        let long = format!(
            "# x\n\n## タスク\n- a (files: a.rs)\n- b (files: b.rs)\n{}",
            "あ".repeat(DRAFT_MAX_BYTES)
        );
        let out = format!("{SPEC_OPEN}\n{long}\n{SPEC_CLOSE}");
        assert_eq!(extract(&out, &p), None, "上限を超えた塊は捨てられる");
        assert!(why_no_draft(&out).contains("大きすぎます"), "{}", why_no_draft(&out));
        // マーカーそのものが無いときは、別の案内になる。
        assert!(why_no_draft("なにも出さなかった").contains(SPEC_OPEN));
    }
}
