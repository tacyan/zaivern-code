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

/// 下書きは実行・保存側と同じ仕様書上限で扱う。
/// 短い完了報告の上限を流用すると詳細な日本語の計画が失われる。
pub const DRAFT_MAX_BYTES: usize = super::planner::SPEC_MAX_BYTES;

/// 明示選択があればその順でだけ解決する。空欄の「おまかせ」だけ全候補を使う。
/// 解決処理を受け取り、未選択の CLI を試していないこともテストできる。
pub fn select_agent<T>(
    names: &[&str],
    selected: &[String],
    mut resolve: impl FnMut(usize) -> Result<T, String>,
) -> Result<T, String> {
    let candidates: Vec<&str> = if selected.is_empty() {
        names.to_vec()
    } else {
        selected.iter().map(String::as_str).collect()
    };
    let mut errors = Vec::new();
    for name in candidates {
        let Some(index) = names.iter().position(|candidate| *candidate == name) else {
            errors.push(format!("{name}: プリセットが見つかりません"));
            continue;
        };
        match resolve(index) {
            Ok(agent) => return Ok(agent),
            Err(reason) => errors.push(format!("{name}: {reason}")),
        }
    }
    Err(format!(
        "計画を作成するエージェントを起動できません。{}",
        if errors.is_empty() {
            "エージェントを設定してください".to_string()
        } else {
            errors.join(" / ")
        }
    ))
}

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
    shape: super::composition::WorkShape,
) -> String {
    if roles == [TeamRole::Implementer] && validations.is_empty() {
        return format!("{}\nあなたは開発チームの仕様書担当です。以下の依頼から、すぐ実装できる仕様書を一度だけ作成してください。実況・相談・外部調査・ツール実行は不要です。目的、具体的な入力と出力、必要な画面や動作、指定技術、成果物のファイル名、担当タスク、未指定事項の合理的な仮定を明確にしてください。元の条件・数値・URLは保持し、参照先を未読のまま確認済みと書かないでください。短い依頼は800〜1500字を目安に、重複を避けて必要な内容をまとめてください。テスト・検証・レビュー・承認待ちの工程や担当は作らないでください。実装作業は後続の担当が行います。\n最大同時担当数: {agents}。独立したファイルだけを並列分担し、共有ファイルは一つの担当にまとめます。単一成果物を無理に分割しないでください。\n出力形式:\n{SPEC_OPEN}\n# 仕様書: {goal}\n## 目的\n## 要件\n## 成果物\n## タスク\n- implementer: 具体的な成果物の作成 (files: 相対ファイルパス)\n## 仮定\n{SPEC_CLOSE}\n依頼:\n{brief}", super::planner::IMPLEMENTATION_ONLY);
    }
    let lanes: Vec<&str> = roles.iter().map(|r| r.key()).collect();
    let goal = goal.trim();
    let title = if goal.is_empty() {
        "(名前なし)"
    } else {
        goal
    };
    format!(
        r#"あなたは開発チームの仕様書きです。今回は入力の整理と着手可能な計画だけを作ります。
外部サイト閲覧、リポジトリ探索、ライブラリ調査、ツール実行、ファイル編集はこの段階では行わないでください。参照URL・指定ライブラリは省略せず、実装担当が最初に確認する手順と確認後に調整する条件へ入れます。未知のAPIや参照サイトの内容を確認済みと書かないでください。
誤字・曖昧な表現・重複を整理し、元の意図・指定技術・数値・納期・予算は保持します。未確定事項は仮定・要確認として明記します。依頼にない性能・売上・実績を作らないでください。
詳細さは必要な入力・出力・例外・期待結果で表し、同じ要件を各節へ繰り返さないでください。短い依頼は本文1500〜2500字程度を目安とし、必要な要件や契約を削って字数を合わせないでください。最終仕様を一度だけ返し、実況や実装コードは返しません。

## 依頼
Goal 名: {title}
内容: {brief}

## 編成
暫定の推奨は {agents} 体。候補の役割: {lanes}
{guidance}
タスクは{min}〜{max}件。1枚の成果物を分けすぎない。独立した成果物は別の担当へ割り当て、依存とファイル所有者を明記します。成果物が参照するファイルも担当を決めます。依頼が文書そのものでなければ文書だけを作るタスクを置かない。
スキルは発動条件・入力・具体的手順・出力例・例外対応と要求された同梱物を実装対象に含めます。

## 出力の形
{SPEC_OPEN}
# <表題>
## 目的
<目的と対象範囲>
## 詳細要件
- REQ-01: <入力・振る舞い・出力・例外>
## 制約と品質
<指定技術、値、互換性、品質>
## 計画方針
<確認が必要な参照情報、実装順序、並列範囲>
## タスク
- <役割>: T01 <作業名> — 対応要件: REQ-01; 手順: <調査・実装・確認の順序>; 依存: <前提タスク>; 成果物: <本体>; 確認: <期待結果> (files: <担当ファイル>)
- <役割>: T02 <検証作業> — 対応要件: REQ-01; 手順: <本体を動かし正常・変更・不足入力で比較する>; 依存: T01; 成果物: <証跡>; 確認: <実物を壊した場合は同じ検査が失敗> (files: tests/**)
## 完了条件
- REQ-01: <観測方法と期待結果>
## 未確定事項とリスク
<仮定・要確認・担当と確認する時点>
## 成果物検証
```json
[ZAI-ACCEPTANCE]
{acceptance_example}
[/ZAI-ACCEPTANCE]
```
{SPEC_CLOSE}

契約例のchecksとscenariosを各REQの実物に置換してください。必須キーは省略・改名しません。requirementは一件のREQ番号、paths/artifacts/evidenceは相対パスです。checksのkindはexists(min_bytes)、contains/not_contains(text)、json_equals(pointer,value)、contrast(foreground,background,minimum)。未確定の値を固定しません。scenariosは具体的なinput・procedure・expected・reject_when・artifacts・evidenceを持ち、全REQを覆います。
本体を使った正常系と、一時コピーを壊した異常系に同じ検査を適用する手順を計画します。存在やPASSEDという文字だけで内容適合としません。未作成の証跡を合格扱いしません。ログ・入出力・追試手順はtests/**へ保存する担当を置きます。実際の実行と証跡形式の詳細は実行時の共通指示に従います。ここではログや実行例を大量に生成しません。
使える検証コマンドは以下だけです。使う場合は「## 検証コマンド」に1行1コマンドで記載。説明文は別の節へ置きます。候補がなければ節を省きます。シェル記法や実行ファイルのパス指定は使いません。
{validations}"#,
        acceptance_example = super::acceptance::EXAMPLE,
        lanes = if lanes.is_empty() {
            "implementer".to_string()
        } else {
            lanes.join(", ")
        },
        min = MIN_TASKS,
        max = agents.saturating_mul(2).clamp(2, 12),
        // 依頼形に合わせた分割指針。人数は完成した計画から再計算する。
        guidance = super::composition::spec_guidance(shape),
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
    let text = super::result_parser::extract_blocks_with_limits(
        stdout,
        SPEC_OPEN,
        SPEC_CLOSE,
        DRAFT_MAX_BYTES,
        DRAFT_MAX_BYTES.saturating_mul(2),
    )
    .into_iter()
    .rfind(|b| !super::result_parser::is_prompt_echo(b, sent, SPEC_OPEN, SPEC_CLOSE))
    .or_else(|| extract_markdown(stdout, sent))?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // **走らせられない検証はここで落とす。** 落とさないと、検証 1 行の
    // ために書き換えた仕様書がまるごと断られる。
    let (clean, _dropped) = strip_unrunnable_validation(text);
    Some(clean)
}

/// マーカーが無い最終回答も、仕様書全体として検証できる場合だけ受け取る。
/// 部分的なマーカーやエコー、説明文を無理に切り出して採用しない。
fn extract_markdown(stdout: &str, sent: &str) -> Option<String> {
    if stdout.len() > DRAFT_MAX_BYTES || stdout.contains(SPEC_OPEN) || stdout.contains(SPEC_CLOSE) {
        return None;
    }
    let mut text = stdout.trim();
    if text.starts_with("```") {
        let (fence, body) = text.split_once('\n')?;
        if !matches!(fence.trim(), "```" | "```markdown" | "```md") {
            return None;
        }
        text = body.trim().strip_suffix("```")?.trim();
    }
    if !text.starts_with("# ")
        || !text
            .lines()
            .any(|l| matches!(l.trim(), "## タスク" | "## Tasks"))
        || !text
            .lines()
            .any(|l| matches!(l.trim(), "## 完了条件" | "## Acceptance Criteria"))
        || sent.contains(text)
        || text.contains("<表題>")
        || text.contains("<役割>")
        || accept(text).is_err()
    {
        return None;
    }
    Some(text.to_string())
}

/// 取り出せなかったとき、**なぜ取り出せなかったのかを言い分ける**。
///
/// 「マーカーがありません」で一括りにすると、大きすぎて捨てられた場合に
/// 直しようのない案内になる (エージェントは形式どおりに出しているのに
/// 「形式どおりに出してください」と言われる)。
pub fn why_no_draft(stdout: &str) -> String {
    if !stdout.contains(SPEC_OPEN) {
        return format!(
            "下書きを取り出せませんでした ({SPEC_OPEN} … {SPEC_CLOSE} が出力にありません)"
        );
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
    if let Some(body) = draft.strip_prefix(super::planner::IMPLEMENTATION_ONLY) {
        return if body.trim().is_empty() {
            Err("仕様書が空です".into())
        } else {
            Ok(())
        };
    }
    super::acceptance::audit(draft)?;
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
    if prompt.starts_with(super::planner::IMPLEMENTATION_ONLY) {
        let text = draft_once(program, args, cwd, prompt, timeout)?;
        let specification = format!("{}\n{}", super::planner::IMPLEMENTATION_ONLY, text);
        accept(&specification)?;
        return Ok(specification);
    }
    audited_draft(prompt, timeout, |instruction, remaining| {
        draft_once(program, args, cwd, instruction, remaining)
    })
}

/// 生成内で見直しを行い、ローカル検査の不備がある場合だけ同じCLIに補修を依頼する。
/// 最大3回・全体timeout内で補修し、失敗は成功に偽装しない。
fn audited_draft(
    original: &str,
    timeout: Duration,
    mut run: impl FnMut(&str, Duration) -> Result<String, String>,
) -> Result<String, String> {
    if timeout.is_zero() {
        return Err("計画の品質監査の制限時間がありません".into());
    }
    let started = std::time::Instant::now();
    let instruction = format!("{original}\n\n出力前に同じ実行内で計画を見直してください。各REQについて『この検証を通る不良品』を一つ想定し、それを検出できる入力・観測・期待値・反例になっているか確認し、不備を直してから最終仕様だけを出力してください。検討の実況、重複説明、仕様全文の繰り返し出力は不要です。成果物の実装や検証コマンドの実行は後続の担当へ計画し、ここでは仕様の作成に集中してください。");
    let first = run(&instruction, timeout)?;
    if accept(&first).is_ok() {
        return Ok(first);
    }
    let mut previous = first;
    let mut defects = accept(&previous)
        .err()
        .unwrap_or_else(|| "構造検査は合格。意味上の妥当性を独立して監査してください".into());
    for round in 0..2 {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err("計画の品質監査が時間内に完了しませんでした".into());
        }
        let instruction = format!("あなたは計画と検証条件の監査担当です。前回の自己評価を信用せず、原依頼に照らして不足を監査してください。コードや成果物は作成しない。構造と意味の両方に問題がなければ [ZAI-PLAN-APPROVED] の一行だけ返し、仕様書を再出力しない。不足があれば修正した仕様書全文を返す。\n原依頼と出力契約:\n{original}\n監査対象の計画:\n{previous}\n構造診断: {defects}\n各REQで『この検証を通る不良品』を具体的に一つ考え、その不良品を落とせる観測・比較・反例へ直す。存在・見出し・テスト名だけで内容適合を判断しない。JSONやCSSの規格は実構造、色は実際の前景/背景の計算、ファイル間連携は対応値、性能は実計測で検証する。コード以外は実物の描画・入力を使った試行・原資料照合など依頼に合う方法を選ぶ。未知の事項・外部依存は明記する。原依頼の目的・出力形式・範囲を保ち、関係のない高負荷条件などを混入させない。不要なタスクや重複検証を増やさない。scenariosとchecksの全要件対応・証跡の保存先と担当・実行可能性を確認する。修正が必要な場合のみ{SPEC_OPEN}と{SPEC_CLOSE}の間へ修正済み仕様書全文を出力する。JSONの必須キーを省略・改名しない。完全な構造例（内容は依頼に合わせる）:\n{}", super::acceptance::EXAMPLE);
        let response = run(&instruction, remaining)?;
        if response.trim() == "[ZAI-PLAN-APPROVED]" {
            // 監査者の承認だけで構造不備を通さず、不備のある候補も失わない。
            match accept(&previous) {
                Ok(()) => return Ok(previous),
                Err(why) => {
                    defects = why;
                    continue;
                }
            }
        }
        previous = response;
        match accept(&previous) {
            Ok(()) => return Ok(previous),
            Err(why) => defects = why,
        }
        if round == 1 {
            break;
        }
    }
    Err(format!(
        "計画を自動修正しましたが検証条件が不足しています: {defects}"
    ))
}

fn draft_once(
    program: &Path,
    args: &[String],
    cwd: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<String, String> {
    let overrides = crate::agents::specification_only_args(program);
    let mut argv: Vec<&str> = args
        .iter()
        .chain(overrides.iter())
        .map(String::as_str)
        .collect();
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
        // 想定する仕様サイズの2倍まで保持し、無制限出力は避ける。
        DRAFT_MAX_BYTES.saturating_mul(2),
    );
    use super::model::ValidationOutcome as V;
    match why {
        V::Passed => draft_candidate(&out.stdout, prompt),
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

/// 構造不備も監査の修正材料にする。空・巨大出力・完全なエコーは採らない。
/// 最終採用は audited_draft の accept を必ず通る。
fn draft_candidate(stdout: &str, sent: &str) -> Result<String, String> {
    if let Some(spec) = extract(stdout, sent) {
        return Ok(spec);
    }
    let text = stdout.trim();
    if !text.is_empty() && text.len() <= DRAFT_MAX_BYTES && text != sent.trim() {
        return Ok(text.to_string());
    }
    Err(why_no_draft(stdout))
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

    #[test]
    fn 選択したエージェントだけを解決する() {
        let names = ["Claude Code", "Codex", "Codex (全自動)"];
        for index in [1, 2] {
            let mut called = Vec::new();
            let chosen = select_agent(&names, &[names[index].to_string()], |i| {
                called.push(i);
                Ok(names[i])
            })
            .unwrap();
            assert_eq!(chosen, names[index]);
            assert_eq!(called, vec![index]);
        }
    }

    #[test]
    fn 選択した候補が使えなくても未選択のclaudeへ切り替えない() {
        let names = ["Claude Code", "Codex"];
        let mut called = Vec::new();
        let result = select_agent(&names, &["Codex".into()], |i| {
            called.push(i);
            if i == 0 {
                Ok(i)
            } else {
                Err("ヘッドレス非対応".into())
            }
        });
        assert_eq!(called, vec![1]);
        let error = result.unwrap_err();
        assert!(error.contains("Codex"));
        assert!(error.contains("ヘッドレス非対応"));
        let missing = select_agent(
            &names,
            &["削除された設定".into()],
            |_| -> Result<(), String> { panic!("別の候補を解決してはいけない") },
        );
        assert!(missing.unwrap_err().contains("削除された設定"));
    }

    #[test]
    fn 複数選択は選択順でおまかせは設定順で解決する() {
        let names = ["Claude Code", "Codex", "Custom Agent"];
        for (selection, expected) in [
            (vec!["Custom Agent".into(), "Codex".into()], vec![2, 1]),
            (vec![], vec![0, 1]),
        ] {
            let mut called = Vec::new();
            let chosen = select_agent(&names, &selection, |i| {
                called.push(i);
                if i == 1 {
                    Ok(i)
                } else {
                    Err("起動不可".into())
                }
            })
            .unwrap();
            assert_eq!(chosen, 1);
            assert_eq!(called, expected);
        }
        assert!(select_agent::<()>(&[], &[], |_| unreachable!()).is_err());
    }

    #[test]
    fn 仕様生成は調査実行を後続へ移し指示の肥大化を抑える() {
        let p = prompt();
        assert!(p.len() < 8000, "指示が肥大化しています: {} bytes", p.len());
        assert!(p.contains("この段階では行わない"));
        assert!(p.contains("実装担当が最初に確認する"));
        assert!(p.contains("未確定事項"));
        assert!(p.contains(super::super::acceptance::EXAMPLE));
    }

    fn prompt() -> String {
        prompt_for(super::super::composition::WorkShape::SingleArtifact)
    }

    fn prompt_for(shape: super::super::composition::WorkShape) -> String {
        build_prompt(
            "テスト",
            "かっこいい３DのWebページを作って",
            4,
            &[R::Architect, R::Implementer, R::Tester],
            &["cargo fmt --check".to_string(), "cargo test".to_string()],
            shape,
        )
    }

    /// **依頼の形ごとの作法が依頼文に載る。** 編成だけ 2 体にしても、
    /// 作法が「別々のファイルに割れ」のままなら SPEC は 8 本に割れる。
    #[test]
    fn 依頼文は依頼の形に合わせた作法を載せる() {
        use super::super::composition::WorkShape as S;
        assert!(
            prompt_for(S::SingleArtifact).contains("独立した付属成果物は別タスクにする"),
            "付属成果物を独立して制作できない"
        );
        assert!(
            prompt_for(S::WideIndependent).contains("単位ごとに 1 本"),
            "独立した単位なのに単位ごとに割れと言っていない"
        );
        assert!(
            prompt_for(S::Research).contains("コードは書かない"),
            "調査なのにコードを書くなと言っていない"
        );
        // 形ごとに違う依頼文になる (同じなら形を伝えていない)。
        assert_ne!(
            prompt_for(S::SingleArtifact),
            prompt_for(S::WideIndependent)
        );
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
        assert!(p.contains("暫定の推奨は 4 体"), "同時数が伝わっていない");
        for instruction in [
            "## 目的",
            "誤字・曖昧な表現・重複",
            "数値・納期・予算は保持",
            "要確認",
            "## 詳細要件",
            "## 制約と品質",
            "## 計画方針",
            "## 未確定事項とリスク",
            "対応要件: REQ-01",
            "手順:",
            "依存:",
            "期待結果",
        ] {
            assert!(p.contains(instruction), "変換指示に {instruction} が無い");
        }
    }

    /// **役割を名乗らせる。** 名乗りが無い表題は
    /// [`super::super::plan_schema::role_of`] が全部 `implementer` に倒すので、
    /// レビューもテストも統合も実装担当が持つ計画になる (実機の Run が
    /// まさにそうで、14 本すべてが `implementer` だった)。
    ///
    /// さらに、名乗りが無いと計画側の重複検出 (`covered`) も効かず、
    /// **SPEC に既にある仕事の骨組みをもう一度積む** — 実機で
    /// `分割` / `設計` / `テスト` / `最終統合` の 4 本が二重になり、
    /// そのどれも最後まで 1 度も動かなかった。
    #[test]
    fn 依頼文はタスクに役割を名乗らせる() {
        let p = prompt();
        assert!(
            p.contains("<役割>: "),
            "タスクの雛形が役割の名乗りを求めていない"
        );
        // 名乗ってよい語は**編成で渡した役割**。ここに固定の一覧を書くと、
        // 役割を増やした日に依頼文だけが古いことに誰も気付けない。
        for r in [R::Architect, R::Implementer, R::Tester] {
            assert!(
                p.contains(r.key()),
                "使ってよい役割として {} が伝わっていない",
                r.key()
            );
        }
    }

    /// **成果物が参照するファイルにも持ち主を付けさせる。**
    ///
    /// 実機の Run で `index.html` が `assets/js/main.js` と
    /// `assets/vendor/three/three.min.js` を読み込んでいたのに、
    /// どのタスクの `files` にも無かった。持ち主が居ないファイルは
    /// 誰も作らないので、**出来上がったページは 2 本とも 404** だった。
    #[test]
    fn 依頼文は参照されるファイルにも持ち主を求める() {
        let p = prompt();
        assert!(
            p.contains("成果物が参照するファイル"),
            "参照されるファイルの持ち主について何も言っていない"
        );
    }

    /// **文書づくりの仕事に化けさせない。**
    ///
    /// 実機の Run は「かっこいい HP を作る」から 8 本の SPEC を書いたが、
    /// そのうち 5 本 (`PLAN.md` / `ARCHITECTURE.md` / `TEST.md` /
    /// `REVIEW.md` / `README.md`) は**文書**で、動くものを作るのは 3 本
    /// だけだった。さらに計画側が骨組みを 6 本積んで 14 本になり、
    /// 25 分走って完了は 0 件、出来たページは読み込みエラーのままだった。
    #[test]
    fn 依頼文は文書だけのタスクを止める() {
        let p = prompt();
        assert!(
            p.contains("文書だけを作るタスクを置かない"),
            "文書に化けるのを止めていない"
        );
        assert!(
            p.contains("分けすぎない"),
            "1 枚の成果物を割りすぎるのを止めていない"
        );
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
        let answer = with_contract(answer);
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
    fn with_contract(spec: &str) -> String {
        format!(
            "{spec}\n## 詳細要件\n- REQ-01: 起動時の画面が読める\n```json\n{}\n{}\n{}\n```",
            super::super::acceptance::OPEN,
            serde_json::json!({"checks":[{"requirement":"REQ-01","paths":["index.html"],"kind":"exists","min_bytes":1}],
            "scenarios":[{"requirement":"REQ-01","input":"画面幅320pxのブラウザで開く","procedure":"実際に描画し本文のはみ出しと操作を確認する","expected":"本文が読み切れCTAから指定画面へ遷移する","reject_when":"固定幅を大きくした一時コピーで横スクロールを検出する","artifacts":["index.html"],"evidence":"tests/browser-results.json"}]}),
            super::super::acceptance::CLOSE
        )
    }

    #[test]
    fn マーカーなしの不完全な計画も補修材料として渡す() {
        let raw = "# 計画\n## タスク\n- 実装\n- 検証\n## 完了条件\n- 完成";
        assert!(extract(raw, "原依頼").is_none());
        assert_eq!(draft_candidate(raw, "原依頼").unwrap(), raw);
        assert!(draft_candidate("", "原依頼").is_err());
        assert!(draft_candidate("原依頼", "原依頼").is_err());
        assert!(draft_candidate(&"x".repeat(DRAFT_MAX_BYTES + 1), "原依頼").is_err());
    }

    #[test]
    fn 生成用の完全な例をそのまま構造検査できる() {
        let p = prompt();
        let body = p
            .split_once(super::super::acceptance::OPEN)
            .unwrap()
            .1
            .split_once(super::super::acceptance::CLOSE)
            .unwrap()
            .0;
        let s = format!("# 計画\n## 詳細要件\n- REQ-01: 指定値を出力\n## タスク\n- 実装\n- 検証\n```json\n{}{}{}\n```",
            super::super::acceptance::OPEN, body, super::super::acceptance::CLOSE);
        assert!(accept(&s).is_ok());
    }

    #[test]
    fn 正常な計画は一回の生成で確定し再起動しない() {
        let valid = with_contract("# 画面\n## タスク\n- 実装\n- 検証");
        let mut calls = 0;
        let result = audited_draft("依頼", Duration::from_secs(2), |_, _| {
            calls += 1;
            Ok(if calls == 1 {
                valid.clone()
            } else {
                draft_candidate("[ZAI-PLAN-APPROVED]", "監査依頼").unwrap()
            })
        })
        .unwrap();
        assert_eq!(result, valid);
        assert_eq!(calls, 1);
    }

    #[test]
    fn 欠落項目を一度に修正し不完全な計画への承認は採用しない() {
        let valid = with_contract("# 画面\n## タスク\n- 実装\n- 検証");
        let mut json: serde_json::Value =
            serde_json::from_str(super::super::acceptance::EXAMPLE).unwrap();
        let case = json["scenarios"][0].as_object_mut().unwrap();
        case.remove("reject_when");
        case.remove("evidence");
        case.insert("input".into(), serde_json::Value::Null);
        let broken = format!(
            "# 計画\n## タスク\n- 実装\n- 検証\n{}{}{}",
            super::super::acceptance::OPEN,
            json,
            super::super::acceptance::CLOSE
        );
        let mut calls = 0;
        let result = audited_draft("依頼", Duration::from_secs(2), |prompt, _| {
            calls += 1;
            if calls == 1 {
                return Ok(broken.clone());
            }
            for field in [
                "scenarios[0].reject_when",
                "scenarios[0].evidence",
                "scenarios[0].input",
            ] {
                assert!(prompt.contains(field), "{field}");
            }
            assert!(prompt.contains(super::super::acceptance::EXAMPLE));
            Ok(if calls == 2 {
                "[ZAI-PLAN-APPROVED]".into()
            } else {
                valid.clone()
            })
        })
        .unwrap();
        assert_eq!(result, valid);
        assert_eq!(calls, 3);
    }

    #[test]
    fn 正常な計画も生成依頼内で反例の見直しを要求する() {
        let valid = with_contract("# 画面\n## タスク\n- 実装\n- 検証");
        let mut calls = Vec::new();
        let result = audited_draft(
            "元の依頼",
            Duration::from_secs(2),
            |prompt, remaining| {
                assert!(remaining <= Duration::from_secs(2));
                calls.push(prompt.to_string());
                Ok(valid.clone())
            },
        )
        .unwrap();
        assert_eq!(result, valid);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("この検証を通る不良品"));
        assert!(calls[0].contains("元の依頼"));
    }
    #[test]
    fn 弱い条件は同じ依頼内で修正し回数と失敗を制限する() {
        let valid = with_contract("# 画面\n## タスク\n- 実装\n- 検証");
        let mut count = 0;
        let result = audited_draft("元の依頼", Duration::from_secs(2), |prompt, _| {
            count += 1;
            if count == 3 {
                assert!(prompt.contains("成果物検証"));
                Ok(valid.clone())
            } else {
                Ok("# 検証のない計画".into())
            }
        })
        .unwrap();
        assert_eq!(result, valid);
        assert_eq!(count, 3);
        let mut calls = 0;
        assert!(audited_draft("依頼", Duration::from_secs(2), |_, _| {
            calls += 1;
            Ok("不十分".into())
        })
        .is_err());
        assert_eq!(calls, 3);
        let mut calls = 0;
        assert!(audited_draft("依頼", Duration::from_secs(2), |_, _| {
            calls += 1;
            Err("接続失敗".into())
        })
        .is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn 生成計画は実物の検証条件を省略できない() {
        assert!(accept("# 計画\n## タスク\n- a\n- b\n")
            .unwrap_err()
            .contains("成果物検証"));
    }

    #[test]
    fn 分担にならない下書きは断る() {
        assert!(accept("").is_err(), "空は受け取らない");
        assert!(
            accept("# だいたい\n\n三次元のページを作ります。\n").is_err(),
            "箇条書きが無ければ 1 件にしかならない"
        );
        assert!(accept(&with_contract(
            "# ページ\n\n## タスク\n- 土台 (files: index.html)\n- 場面 (files: scene.js)\n"
        ))
        .is_ok());
    }

    #[test]
    fn マーカーなしでも完成した非html仕様書を取り出せる() {
        let p = prompt();
        for file in ["SKILL.md", "src/main.rs", "main.py"] {
            let answer = format!(
                "# 成果物\n\n## タスク\n- implementer: 作成 (files: {file})\n\
                 - tester: 利用方法で検証\n\n## 完了条件\n- 依頼どおりに利用できる"
            );
            let answer = with_contract(&answer);
            for out in [answer.clone(), format!("```markdown\n{answer}\n```")] {
                assert_eq!(extract(&out, &p).as_deref(), Some(answer.as_str()));
            }
            assert_eq!(extract(&answer, &answer), None);
            assert_eq!(extract(&format!("{SPEC_OPEN}\n{answer}"), &p), None);
        }
        for out in [
            "認証が必要です",
            "SPEC.md に書きました",
            "# 未完成\n## タスク\n- 作成",
        ] {
            assert_eq!(extract(out, &p), None);
        }
    }

    /// **大きすぎる下書きは、理由を言い分けて断る。**
    ///
    /// `extract_blocks` は上限を超えた塊を黙って捨てるので、素直に書くと
    /// 「マーカーがありません」と言ってしまう — エージェントは形式どおりに
    /// 出しているのに「形式どおりに出してください」と返る、直しようのない案内になる。
    #[test]
    fn 詳細な日本語計画は報告の上限を超えても監査まで全文を保つ() {
        let spec = with_contract(&format!(
            "# 画面\n## タスク\n- 実装\n- 検証\n## 完了条件\n- 指定の画面が動く\n## 補足\n{}",
            "日本語の詳細要件。".repeat(2000)
        ));
        assert!(spec.len() > super::super::result_parser::BLOCK_MAX_BYTES);
        for output in [spec.clone(), format!("{SPEC_OPEN}\n{spec}\n{SPEC_CLOSE}")] {
            assert_eq!(draft_candidate(&output, "元の依頼").unwrap(), spec);
            let mut calls = 0;
            let audited =
                audited_draft("元の依頼", Duration::from_secs(2), |instruction, _| {
                    calls += 1;
                    if calls == 1 {
                        draft_candidate(&output, instruction)
                    } else {
                        assert!(instruction.contains(&spec));
                        Ok("[ZAI-PLAN-APPROVED]".into())
                    }
                })
                .unwrap();
            assert_eq!(audited, spec);
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn 仕様の境界まで取り出せて通常報告の上限は広がらない() {
        let body = "a".repeat(DRAFT_MAX_BYTES);
        let output = format!("{SPEC_OPEN}{body}{SPEC_CLOSE}");
        assert_eq!(extract(&output, "元の依頼"), Some(body));
        assert!(
            super::super::result_parser::extract_blocks(&output, SPEC_OPEN, SPEC_CLOSE).is_empty()
        );
        let output = format!("{SPEC_OPEN}{}{SPEC_CLOSE}", "a".repeat(DRAFT_MAX_BYTES + 1));
        assert!(extract(&output, "元の依頼").is_none());
    }

    #[test]
    fn 大きすぎる下書きは理由を言い分ける() {
        let p = prompt();
        let long = format!(
            "# x\n\n## タスク\n- a (files: a.rs)\n- b (files: b.rs)\n{}",
            "あ".repeat(DRAFT_MAX_BYTES)
        );
        let out = format!("{SPEC_OPEN}\n{long}\n{SPEC_CLOSE}");
        assert_eq!(extract(&out, &p), None, "上限を超えた塊は捨てられる");
        assert!(
            why_no_draft(&out).contains("大きすぎます"),
            "{}",
            why_no_draft(&out)
        );
        // マーカーそのものが無いときは、別の案内になる。
        assert!(why_no_draft("なにも出さなかった").contains(SPEC_OPEN));
    }
}
