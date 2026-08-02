//! オフライン・ライセンス認証 — **通信ゼロ・完全ローカル**。
//!
//! ## 何を解決するか
//!
//! 製品を有料で売れるようにするための土台。ただし本製品の売りである
//! 「ネットワークを一切叩かない」を壊してはいけないので、**認証サーバへの
//! 問い合わせを行わない**方式にする。
//!
//! ## 方式 — Ed25519 公開鍵署名
//!
//! ライセンスキーは「ペイロード (JSON) + 署名」を Base64URL で綴じた文字列で、
//! アプリには**公開鍵だけ**を埋め込む。署名の検証はローカルで完結するので、
//! 起動時も適用時もパケットは 1 バイトも出ない。
//!
//! ```text
//! ZVL1.<Base64URL(payload_json)>.<Base64URL(ed25519_signature 64B)>
//!  ^^^^ 形式バージョン。署名は "ZVL1.<payload>" の ASCII バイト列に対して行う
//!       (= 形式を跨いだペイロードの使い回しができない)
//! ```
//!
//! ## この方式の限界 — **失効 (revoke) はできない**
//!
//! オフライン検証は「発行済みの署名が数学的に正しいか」しか判定できない。
//! サーバに問い合わせないので、**一度発行したキーを後から無効化する手段が無い**
//! (返金・不正共有・鍵の流出のいずれにも対処できない)。
//!
//! 対処は 1 つだけ: **期限付きライセンス** (`exp`) を発行し、期限が来たら
//! 自動で `Expired` へ落とす。恒久ライセンス (`exp: null`) を売る場合は
//! 「失効できない」ことを承知の上で売ること。
//! この限界は UI にも明記する (アクティベーション画面の注記)。
//!
//! ## 機能を奪わない
//!
//! 未ライセンスでもアプリは**完全に動く**。ライセンスは Pro 機能を
//! 「解錠する」方向にのみ働き、既存の無料機能を 1 つもゲートしない。
//! ゲート判定は [`is_pro`] の 1 関数に集約する (判定を散らさない)。
//!
//! ## 設計の方針
//!
//! - **パスは全て `config::zaivern_dir()` 起点** (= `dirs::home_dir()` 由来)。
//!   環境・OS・ユーザー名を焼き込まない。テストは注入されたパスに対して動く。
//! - **検証は純関数** ([`verify_key`])。ファイルも時計も触らないので、
//!   全分岐をテーブルテストで固定できる。
//! - **不正入力で panic しない**。空文字・途中で切れた Base64・巨大文字列・
//!   非 ASCII・改行混じりは全て `Malformed` へ落とす。
//! - **署名を検証してからペイロードを読む**。未検証の JSON をパーサへ
//!   通さない。
//!
//! ## 秘密鍵はここに無い
//!
//! 発行側 (販売者) の秘密鍵はリポジトリにもバイナリにも存在しない。
//! 鍵の作り方とキーの発行手順は `docs/licensing.md`、発行ツールは
//! `tools/licgen/` (独立クレート。`zai` バイナリには含まれない)。

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// ライセンスキーの形式バージョン。署名対象にも含める。
const KEY_PREFIX: &str = "ZVL1";

/// 受け付けるキーの最大バイト長。これを超える入力は読まずに弾く
/// (巨大文字列を貼られても Base64 デコードを走らせないため)。
const MAX_KEY_LEN: usize = 4096;

/// Ed25519 の署名長 (バイト)。
const SIG_LEN: usize = 64;

/// Ed25519 の公開鍵長 (バイト)。
pub const PUBKEY_LEN: usize = 32;

/// 綴じるときの Base64URL (パディング無し)。
///
/// アプリ本体は**検証しかしない**ので、エンコード側は発行ツール
/// (`tools/licgen`) とテストにしか要らない。両者が同じ設定であることを
/// テストが往復で確かめる。
#[cfg(test)]
const B64_ENCODE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_encode_padding(false),
);

/// 解くときの Base64URL。パディングは**有っても無くても受ける**
/// (メールやチャットを経由するとパディングが落ちることがあるため)。
const B64_DECODE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

// ── 埋め込み公開鍵 ────────────────────────────────────────────────

/// 検証に使う Ed25519 公開鍵 (32 バイト)。
///
/// 実際の値は**ビルド時の環境変数** `ZAIVERN_LICENSE_PUBKEY` (16 進 64 桁) から
/// 埋め込む。設定されていなければ全ゼロの番兵になり、[`pubkey_configured`] が
/// `false` を返して「この配布版はライセンスを検証できない」と UI に出る。
///
/// 公開鍵は秘密ではないので値そのものをコミットしても害はないが、
/// **鍵ペアを作れるのは販売者だけ**なので、リポジトリには番兵だけを置き、
/// 公式ビルドが環境変数で本物を注入する形にしている (`docs/licensing.md`)。
///
/// 16 進が壊れていれば**コンパイルエラー**になる (const 評価中の panic)。
/// 「打ち間違えた公開鍵で出荷する」が構造的に起きない。
pub const EMBEDDED_PUBKEY: [u8; PUBKEY_LEN] = match option_env!("ZAIVERN_LICENSE_PUBKEY") {
    Some(s) => decode_hex_pubkey(s),
    None => [0u8; PUBKEY_LEN],
};

/// このビルドがライセンスキーを検証できるか (= 本物の公開鍵が入っているか)。
///
/// 番兵 (全ゼロ) のままのビルドは**どんなキーも解錠できない**。そこで
/// 「ライセンスキーを入力…」という**絶対に成立しない入口**を画面へ出さない
/// ための判定に使う。CLAUDE.md の「常に 0 を表示するバッジ」「中身より
/// 空状態を見せる時間が長いパネル」と同じ理由 — 押しても必ず失敗する項目は
/// 機能ではなく雑音である。
///
/// コードそのものは残す。鍵を入れて焼き直せば同じバイナリで有効になる。
pub fn signing_configured() -> bool {
    EMBEDDED_PUBKEY != [0u8; PUBKEY_LEN]
}

/// 16 進 64 桁 → 32 バイト。const 文脈でのみ使う。
const fn decode_hex_pubkey(s: &str) -> [u8; PUBKEY_LEN] {
    let b = s.as_bytes();
    assert!(
        b.len() == PUBKEY_LEN * 2,
        "ZAIVERN_LICENSE_PUBKEY は 16 進 64 桁で指定してください"
    );
    let mut out = [0u8; PUBKEY_LEN];
    let mut i = 0;
    while i < PUBKEY_LEN {
        out[i] = (hex_nibble(b[i * 2]) << 4) | hex_nibble(b[i * 2 + 1]);
        i += 1;
    }
    out
}

const fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("ZAIVERN_LICENSE_PUBKEY に 16 進以外の文字があります"),
    }
}

/// 公開鍵が実際に埋め込まれているか (全ゼロ = 未設定の番兵)。
pub fn pubkey_configured(pubkey: &[u8; PUBKEY_LEN]) -> bool {
    pubkey.iter().any(|b| *b != 0)
}

// ── ペイロードと状態 ──────────────────────────────────────────────

/// ライセンスキーに綴じ込まれる中身。
///
/// `Serialize` も derive しているのは、発行側と同じ JSON をテストで
/// 組み立てて往復検証するため (= 形式のドリフトをテストが検出する)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload {
    /// 購入者の識別子 (メールアドレスや購入 ID)。
    pub sub: String,
    /// 等級。`"pro"` など。`"free"` は Pro 扱いしない。
    pub tier: String,
    /// 発行時刻 (Unix 秒)。**検証には使わない** — 時計がずれている端末で
    /// 正規のキーを弾いてしまうため。表示用の情報として持つだけ。
    pub iat: i64,
    /// 失効時刻 (Unix 秒)。`null` は無期限。
    /// オフライン検証では失効ができないので、原則こちらを付けて発行する。
    #[serde(default)]
    pub exp: Option<i64>,
    /// 座席数 (表示用。オフラインでは強制できない)。
    #[serde(default = "one_seat")]
    pub seats: u32,
}

fn one_seat() -> u32 {
    1
}

/// ライセンスの状態。UI はこれだけを見て表示を決める。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseStatus {
    /// キーが保存されていない (= 無料利用中)。異常ではない。
    Unlicensed,
    /// 署名が正しく、期限内。
    Valid {
        tier: String,
        sub: String,
        exp: Option<i64>,
        seats: u32,
    },
    /// 署名は正しいが期限切れ。
    Expired { exp: i64 },
    /// 形式が壊れている (理由は翻訳キーとして使える固定文字列)。
    Malformed(String),
    /// 形式は正しいが署名が合わない (改竄・別の鍵で発行・写し間違い)。
    BadSignature,
}

/// Pro 機能の解錠判定 — **ゲートはこの 1 関数に集約する**。
///
/// 判定を各所へ散らすと「片方だけ直し忘れる」が必ず起きるので、
/// Pro かどうかを知りたい場所は必ずここを通すこと。
pub fn is_pro(status: &LicenseStatus) -> bool {
    match status {
        LicenseStatus::Valid { tier, .. } => !tier.trim().eq_ignore_ascii_case("free"),
        LicenseStatus::Unlicensed
        | LicenseStatus::Expired { .. }
        | LicenseStatus::Malformed(_)
        | LicenseStatus::BadSignature => false,
    }
}

// ── 検証 ──────────────────────────────────────────────────────────

/// ライセンスキーを検証する。**ファイルも時計もネットワークも触らない純関数**。
///
/// `now_unix` は呼び出し側が渡す (テストが時間を固定できるようにするため)。
pub fn verify_key(key: &str, pubkey: &[u8; PUBKEY_LEN], now_unix: i64) -> LicenseStatus {
    // 長さは**最初に**見る。巨大文字列に対して走査を走らせない。
    if key.len() > MAX_KEY_LEN {
        return LicenseStatus::Malformed("ライセンスキーが長すぎます".into());
    }
    // 貼り付けで混ざる空白・改行は落とす (Base64URL に空白は現れない)。
    let compact: String = key.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return LicenseStatus::Unlicensed;
    }
    if !compact.is_ascii() {
        return LicenseStatus::Malformed("ライセンスキーに使えない文字が含まれています".into());
    }

    let parts: Vec<&str> = compact.split('.').collect();
    if parts.len() != 3 {
        return LicenseStatus::Malformed("ライセンスキーの形式が違います".into());
    }
    if !parts[0].eq_ignore_ascii_case(KEY_PREFIX) {
        return LicenseStatus::Malformed("対応していないライセンスキーの版です".into());
    }
    if parts[1].is_empty() || parts[2].is_empty() {
        return LicenseStatus::Malformed("ライセンスキーの形式が違います".into());
    }

    if !pubkey_configured(pubkey) {
        return LicenseStatus::Malformed("この配布版には検証用の公開鍵がありません".into());
    }
    let Ok(vk) = VerifyingKey::from_bytes(pubkey) else {
        return LicenseStatus::Malformed("この配布版には検証用の公開鍵がありません".into());
    };

    let Ok(sig_bytes) = B64_DECODE.decode(parts[2]) else {
        return LicenseStatus::Malformed("署名を読み取れません".into());
    };
    let Ok(sig_arr) = <[u8; SIG_LEN]>::try_from(sig_bytes.as_slice()) else {
        return LicenseStatus::Malformed("署名の長さが不正です".into());
    };
    let sig = Signature::from_bytes(&sig_arr);

    // 署名対象は "ZVL1.<payload>" の ASCII バイト列そのもの。
    // Base64 を解いた結果ではなく**綴じられた文字列**に署名することで、
    // 再エンコードの揺れ (パディングや正規化) が検証に影響しない。
    //
    // 接頭辞は利用者が打った綴りではなく**正規形の定数**を使う。こうしないと
    // 「zvl1 と打った人だけ署名が合わない」という、原因の分からない不一致に
    // なる (接頭辞の大小は上で既に許容している)。
    let signed = format!("{}.{}", KEY_PREFIX, parts[1]);
    // verify_strict は小さい位数の公開鍵や非正規な符号化を拒否する
    // (verify より厳しい方を使う)。
    if vk.verify_strict(signed.as_bytes(), &sig).is_err() {
        return LicenseStatus::BadSignature;
    }

    // ここから先は「署名検証済みのデータ」だけを扱う。
    let Ok(raw) = B64_DECODE.decode(parts[1]) else {
        return LicenseStatus::Malformed("ライセンス情報を読み取れません".into());
    };
    let Ok(text) = std::str::from_utf8(&raw) else {
        return LicenseStatus::Malformed("ライセンス情報の文字コードが不正です".into());
    };
    let Ok(payload) = serde_json::from_str::<Payload>(text) else {
        return LicenseStatus::Malformed("ライセンス情報の中身が不正です".into());
    };

    if payload.sub.trim().is_empty() || payload.tier.trim().is_empty() {
        return LicenseStatus::Malformed("ライセンス情報の中身が不正です".into());
    }
    if let Some(exp) = payload.exp {
        if now_unix >= exp {
            return LicenseStatus::Expired { exp };
        }
    }
    LicenseStatus::Valid {
        tier: payload.tier,
        sub: payload.sub,
        exp: payload.exp,
        seats: payload.seats.max(1),
    }
}

/// 現在の Unix 秒。時計が 1970 より前なら 0 に丸める (panic しない)。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

/// キーを画面に出すための伏せ字。先頭 6 文字…末尾 4 文字だけ残す。
///
/// キー全体は肩越しの覗き見やスクリーンショットで丸ごと漏れるので、
/// UI では必ずこれを通す。短すぎる文字列は全部伏せる。
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() <= 12 {
        return "•".repeat(chars.len().min(12));
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

// ── 保存場所 ──────────────────────────────────────────────────────

/// ライセンスキーの保存先 `~/.zaivern/license.key`。
///
/// ディレクトリの導出はセッション永続化と同じ [`crate::config::zaivern_dir`]
/// を**再利用する** (home の解決規則を二重に持たないため)。
pub fn license_path() -> PathBuf {
    crate::config::zaivern_dir().join("license.key")
}

/// 保存済みのキーを読む。無い・読めない・空ならすべて `None` (panic しない)。
pub fn load_key_from(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let t = raw.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// キーを保存する。ディレクトリが無ければ作る。
pub fn save_key_to(path: &Path, key: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;
    }
    let body = format!("{}\n", key.trim());
    std::fs::write(path, body).map_err(|e| format!("{} を保存できません: {e}", path.display()))?;
    harden(path);
    Ok(())
}

/// 保存済みのキーを消す。元から無い場合も成功扱い。
pub fn clear_key_at(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{} を削除できません: {e}", path.display())),
    }
}

/// 購入者情報が入ったファイルを他ユーザーから読めないようにする。
///
/// 失敗しても**続行する** — 権限が付かないことよりも、ライセンスが
/// 保存できないことの方がユーザーにとって害が大きいため。
#[cfg(unix)]
fn harden(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// Windows 側の実装。
///
/// Win32 の ACL を直接いじる API は std に無く、そのために windows-sys を
/// 足すのは割に合わない。保存先はユーザープロファイル配下 (`%USERPROFILE%\
/// .zaivern`) で、既定の ACL が既に「本人と管理者のみ」なので、ここでは
/// 何もしないのが正しい (= 何もしないことを明示した実装)。
#[cfg(windows)]
fn harden(_path: &Path) {}

/// unix / windows のいずれでもない環境向けの実装 (実質使われないが、
/// `cfg` の穴を空けたまま `harden` が消えるのを防ぐ)。
#[cfg(not(any(unix, windows)))]
fn harden(_path: &Path) {}

// ── アプリから使う入口 ────────────────────────────────────────────

/// 保存済みキーを読み、埋め込み公開鍵と現在時刻で検証する。
///
/// 戻り値の 1 つ目は**生のキー** (伏せ字表示用)。ネットワークは使わない。
pub fn current_status() -> (Option<String>, LicenseStatus) {
    match load_key_from(&license_path()) {
        Some(k) => {
            let s = verify_key(&k, &EMBEDDED_PUBKEY, now_unix());
            (Some(k), s)
        }
        None => (None, LicenseStatus::Unlicensed),
    }
}

/// キーを保存して検証し直す。保存に失敗したら `Err`。
pub fn apply_key(key: &str) -> Result<(Option<String>, LicenseStatus), String> {
    let trimmed: String = key.chars().filter(|c| !c.is_whitespace()).collect();
    if trimmed.is_empty() {
        return Err("ライセンスキーが空です".into());
    }
    save_key_to(&license_path(), &trimmed)?;
    Ok(current_status())
}

/// 保存済みキーを消して未ライセンス状態へ戻す。
pub fn remove_key() -> Result<(), String> {
    clear_key_at(&license_path())
}

/// 期限 (Unix 秒) を `YYYY-MM-DD` に直す。UTC 固定。
///
/// chrono を足さずに済ませるための最小実装。グレゴリオ暦の閏年規則だけを
/// 実装している (表示専用なので、これで十分)。
pub fn format_unix_date(ts: i64) -> String {
    if ts < 0 {
        return "-".into();
    }
    let days = ts / 86_400;
    let (mut y, mut d) = (1970i64, days);
    loop {
        let len = if is_leap(y) { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let months: [i64; 12] = [
        31,
        if is_leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while m < 12 && d >= months[m] {
        d -= months[m];
        m += 1;
    }
    format!("{y:04}-{:02}-{:02}", m + 1, d + 1)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ══════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// **テスト専用**の 32 バイト種をタグから作る。ソースに固定の秘密鍵
    /// バイト列を書かないためのヘルパで、値は**タグだけ**で決まる。
    ///
    /// # 以前の実装が壊れていた話 (同じ轍を踏まないために残す)
    ///
    /// 旧実装は添字 `i` を FNV チェーンの**末尾**に置き、`(h >> 24) as u8` で
    /// 取り出していた。FNV の乗数は `2^40 + 0x1b3` なので、最後に `i` を
    /// XOR してから 1 回だけ乗じても**動くのは下位 17 ビットだけ**で、
    /// 取り出す窓 (ビット 24..32) には届かない。結果、
    /// **32 バイトが全部同じ値**になり、種は実質 **256 通りしか存在しなかった**。
    /// 別タグ同士が 1/256 で同じ鍵ペアになり、「別の発行者の鍵は弾かれる」
    /// テストが約 0.5% の確率で「弾かれない」と言って落ちていた
    /// (実測: 200,000 回中 992 回衝突。CI の ubuntu で実際に踏んだ)。
    ///
    /// 直し方は 2 点:
    /// 1. **添字を先頭に置く。** 以降の全ての乗算で拡散する。
    /// 2. **64 ビットを畳んで 1 バイトにする。** 固定窓は上位の差を捨てる。
    ///
    /// 時刻と pid は**混ぜない**。テストは決定的であるべきで、
    /// 実行ごとに鍵が変わると、こういう確率的な失敗が再現できなくなる。
    fn test_seed(tag: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in [i as u8].into_iter().chain(tag.as_bytes().iter().copied()) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            *slot = (h ^ (h >> 32) ^ (h >> 16) ^ (h >> 8)) as u8;
        }
        out
    }

    /// テスト用の鍵導出そのものを検査する。ここが壊れると、他の
    /// ライセンステストが**何も検証していない**のに緑になる。
    #[test]
    fn テスト用の鍵導出は十分に散らばる() {
        // 1) 1 本の種の中でバイトが散らばっている
        //    (旧実装は 32 バイトが全部同じ値だった)
        let s = test_seed("entropy");
        let uniq: std::collections::HashSet<u8> = s.iter().copied().collect();
        assert!(
            uniq.len() >= 16,
            "種のバイトが散らばっていない (相異なる値 {} 個): {s:02x?}",
            uniq.len()
        );

        // 2) 違うタグは違う鍵ペアになる。1 文字違い・長さ違いを含める
        let tags = [
            "issuer-a",
            "issuer-b",
            "issuer-c",
            "a",
            "b",
            "",
            "tamper-sig",
            "unset-pubkey",
            "issuer-aa",
        ];
        let mut seen = std::collections::HashSet::new();
        for t in tags {
            let (_, pk) = test_keys(t);
            assert!(seen.insert(pk), "タグ {t:?} の鍵が他と衝突した");
        }
    }

    fn test_keys(tag: &str) -> (SigningKey, [u8; 32]) {
        let sk = SigningKey::from_bytes(&test_seed(tag));
        let pk = sk.verifying_key().to_bytes();
        (sk, pk)
    }

    /// 発行側 (tools/licgen) と同じ手順でキーを綴じる。
    fn issue_raw(sk: &SigningKey, payload_bytes: &[u8]) -> String {
        let p = B64_ENCODE.encode(payload_bytes);
        let signed = format!("{KEY_PREFIX}.{p}");
        let sig = sk.sign(signed.as_bytes());
        format!("{signed}.{}", B64_ENCODE.encode(sig.to_bytes()))
    }

    fn issue(sk: &SigningKey, p: &Payload) -> String {
        let json = serde_json::to_string(p).expect("payload serialize");
        issue_raw(sk, json.as_bytes())
    }

    fn payload(exp: Option<i64>) -> Payload {
        Payload {
            sub: "buyer@example.test".into(),
            tier: "pro".into(),
            iat: 1_700_000_000,
            exp,
            seats: 3,
        }
    }

    // ── 正常系 ────────────────────────────────────────────────────

    #[test]
    fn valid_perpetual_key() {
        let (sk, pk) = test_keys("valid-perpetual");
        let key = issue(&sk, &payload(None));
        let st = verify_key(&key, &pk, 1_800_000_000);
        assert_eq!(
            st,
            LicenseStatus::Valid {
                tier: "pro".into(),
                sub: "buyer@example.test".into(),
                exp: None,
                seats: 3,
            }
        );
        assert!(is_pro(&st));
    }

    #[test]
    fn valid_until_boundary_then_expired() {
        let (sk, pk) = test_keys("boundary");
        let exp = 1_800_000_000i64;
        let key = issue(&sk, &payload(Some(exp)));
        // 期限の 1 秒前 = 有効
        assert!(matches!(
            verify_key(&key, &pk, exp - 1),
            LicenseStatus::Valid { .. }
        ));
        // 期限ちょうど = 失効 (境界は「以上で失効」)
        assert_eq!(verify_key(&key, &pk, exp), LicenseStatus::Expired { exp });
        // 期限のずっと後 = 失効
        assert_eq!(
            verify_key(&key, &pk, exp + 86_400),
            LicenseStatus::Expired { exp }
        );
    }

    #[test]
    fn whitespace_and_newlines_are_tolerated() {
        let (sk, pk) = test_keys("whitespace");
        let key = issue(&sk, &payload(None));
        let mangled = format!("  {}\n{}  \t\n", &key[..20], &key[20..]);
        assert!(matches!(
            verify_key(&mangled, &pk, 1_800_000_000),
            LicenseStatus::Valid { .. }
        ));
    }

    #[test]
    fn prefix_is_case_insensitive() {
        let (sk, pk) = test_keys("prefix-case");
        let key = issue(&sk, &payload(None)).replacen("ZVL1", "zvl1", 1);
        assert!(matches!(
            verify_key(&key, &pk, 1_800_000_000),
            LicenseStatus::Valid { .. }
        ));
    }

    #[test]
    fn seats_default_and_floor() {
        let (sk, pk) = test_keys("seats");
        // seats を省いた JSON でも既定 1 で通る
        let key = issue_raw(
            &sk,
            br#"{"sub":"a@b.test","tier":"pro","iat":1700000000,"exp":null}"#,
        );
        match verify_key(&key, &pk, 1_800_000_000) {
            LicenseStatus::Valid { seats, .. } => assert_eq!(seats, 1),
            other => panic!("expected Valid, got {other:?}"),
        }
        // seats: 0 は 1 に丸める (0 席のライセンスは意味を成さない)
        let key0 = issue_raw(
            &sk,
            br#"{"sub":"a@b.test","tier":"pro","iat":1700000000,"exp":null,"seats":0}"#,
        );
        match verify_key(&key0, &pk, 1_800_000_000) {
            LicenseStatus::Valid { seats, .. } => assert_eq!(seats, 1),
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compat() {
        let (sk, pk) = test_keys("forward-compat");
        let key = issue_raw(
            &sk,
            br#"{"sub":"a@b.test","tier":"pro","iat":1,"exp":null,"seats":1,"future":"x"}"#,
        );
        assert!(matches!(
            verify_key(&key, &pk, 1_800_000_000),
            LicenseStatus::Valid { .. }
        ));
    }

    // ── 署名まわり ────────────────────────────────────────────────

    #[test]
    fn tampered_payload_is_bad_signature() {
        let (sk, pk) = test_keys("tamper-payload");
        let key = issue(&sk, &payload(Some(1_000)));
        // 期限を伸ばそうとして中身だけ差し替える
        let forged = issue_raw(&sk, b"unused");
        let parts: Vec<&str> = key.split('.').collect();
        let other: Vec<&str> = forged.split('.').collect();
        let mixed = format!("{}.{}.{}", parts[0], other[1], parts[2]);
        assert_eq!(verify_key(&mixed, &pk, 500), LicenseStatus::BadSignature);
    }

    #[test]
    fn tampered_signature_is_bad_signature() {
        let (sk, pk) = test_keys("tamper-sig");
        let key = issue(&sk, &payload(None));
        let parts: Vec<&str> = key.split('.').collect();
        // 署名の 1 文字を別の Base64URL 文字へ差し替える
        let mut sig: Vec<char> = parts[2].chars().collect();
        sig[0] = if sig[0] == 'A' { 'B' } else { 'A' };
        let broken: String = sig.into_iter().collect();
        let key2 = format!("{}.{}.{}", parts[0], parts[1], broken);
        assert_eq!(
            verify_key(&key2, &pk, 1_800_000_000),
            LicenseStatus::BadSignature
        );
    }

    #[test]
    fn key_signed_by_another_keypair_is_rejected() {
        let (sk_a, pk_a) = test_keys("issuer-a");
        let (_, pk_b) = test_keys("issuer-b");
        // 導出が衝突していると、この後の assert は「弾かれなかった」と
        // 言って落ちるが、原因は検証ロジックではなくヘルパにある。
        // 先にここで落として、読む人が原因を取り違えないようにする。
        assert_ne!(
            pk_a, pk_b,
            "テスト用の鍵導出が衝突した (検証の問題ではない)"
        );
        let key = issue(&sk_a, &payload(None));
        assert_eq!(
            verify_key(&key, &pk_b, 1_800_000_000),
            LicenseStatus::BadSignature
        );
    }

    #[test]
    fn unset_pubkey_reports_missing_pubkey_not_bad_signature() {
        let (sk, _) = test_keys("unset-pubkey");
        let key = issue(&sk, &payload(None));
        let st = verify_key(&key, &[0u8; PUBKEY_LEN], 1_800_000_000);
        assert_eq!(
            st,
            LicenseStatus::Malformed("この配布版には検証用の公開鍵がありません".into())
        );
        assert!(!pubkey_configured(&[0u8; PUBKEY_LEN]));
    }

    // ── 異常入力 (panic しないこと) ───────────────────────────────

    #[test]
    fn empty_and_blank_are_unlicensed() {
        let (_, pk) = test_keys("blank");
        for s in ["", "   ", "\n\n", "\t \r\n"] {
            assert_eq!(
                verify_key(s, &pk, 0),
                LicenseStatus::Unlicensed,
                "input={s:?}"
            );
        }
    }

    #[test]
    fn malformed_shapes_table() {
        let (sk, pk) = test_keys("shapes");
        let good = issue(&sk, &payload(None));
        let parts: Vec<&str> = good.split('.').collect();
        let cases: Vec<(String, &str)> = vec![
            ("ZVL1".to_string(), "セグメントが 1 つ"),
            (format!("ZVL1.{}", parts[1]), "セグメントが 2 つ"),
            (format!("{good}.extra"), "セグメントが 4 つ"),
            (format!("ZVL9.{}.{}", parts[1], parts[2]), "版が違う"),
            (format!("ZVL1..{}", parts[2]), "ペイロードが空"),
            (format!("ZVL1.{}.", parts[1]), "署名が空"),
            (format!("ZVL1.{}.@@@@", parts[1]), "署名が Base64 でない"),
            (format!("ZVL1.{}.QUJD", parts[1]), "署名が 64 バイトでない"),
            ("ライセンスキー.あいう.えお".to_string(), "非 ASCII"),
            (
                format!("ZVL1.{}.{}", parts[1], &parts[2][..10]),
                "署名が途中で切れている",
            ),
        ];
        for (input, why) in cases {
            match verify_key(&input, &pk, 1_800_000_000) {
                LicenseStatus::Malformed(_) => {}
                other => panic!("{why}: expected Malformed, got {other:?} (input={input:?})"),
            }
        }
    }

    #[test]
    fn signed_but_undecodable_payload_is_malformed() {
        let (sk, pk) = test_keys("undecodable");
        // 署名は正しいが、ペイロード部が Base64URL として解けない
        let signed = format!("{KEY_PREFIX}.@@@@");
        let sig = sk.sign(signed.as_bytes());
        let key = format!("{signed}.{}", B64_ENCODE.encode(sig.to_bytes()));
        match verify_key(&key, &pk, 0) {
            LicenseStatus::Malformed(m) => assert_eq!(m, "ライセンス情報を読み取れません"),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn signed_non_utf8_payload_is_malformed() {
        let (sk, pk) = test_keys("non-utf8");
        let key = issue_raw(&sk, &[0xff, 0xfe, 0xfd, 0xfc]);
        match verify_key(&key, &pk, 0) {
            LicenseStatus::Malformed(m) => {
                assert_eq!(m, "ライセンス情報の文字コードが不正です")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn signed_but_invalid_json_is_malformed() {
        let (sk, pk) = test_keys("bad-json");
        for body in [
            &b"not json at all"[..],
            &b"{}"[..],
            &br#"{"sub":"a","tier":"pro"}"#[..], // iat 欠落
            &br#"{"sub":"","tier":"pro","iat":1}"#[..], // sub が空
            &br#"{"sub":"a","tier":"   ","iat":1}"#[..], // tier が空白のみ
            &br#"{"sub":123,"tier":"pro","iat":1}"#[..], // 型違い
        ] {
            let key = issue_raw(&sk, body);
            match verify_key(&key, &pk, 0) {
                LicenseStatus::Malformed(_) => {}
                other => panic!("expected Malformed for {body:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn oversized_input_is_rejected_without_panic() {
        let (_, pk) = test_keys("oversized");
        let huge = "A".repeat(MAX_KEY_LEN + 1);
        match verify_key(&huge, &pk, 0) {
            LicenseStatus::Malformed(m) => assert_eq!(m, "ライセンスキーが長すぎます"),
            other => panic!("expected Malformed, got {other:?}"),
        }
        // 1 MiB の非 ASCII でも panic しない
        let huge_jp = "あ".repeat(400_000);
        assert!(matches!(
            verify_key(&huge_jp, &pk, 0),
            LicenseStatus::Malformed(_)
        ));
    }

    #[test]
    fn mutations_of_a_valid_key_never_panic() {
        let (sk, pk) = test_keys("mutations");
        let good = issue(&sk, &payload(Some(2_000_000_000)));
        let bytes: Vec<char> = good.chars().collect();
        // 1 文字ずつ落とす / 途中で切る / 1 文字ずらす — 全て panic せず判定が返る
        for i in 0..bytes.len() {
            let mut dropped = bytes.clone();
            dropped.remove(i);
            let _ = verify_key(&dropped.into_iter().collect::<String>(), &pk, 0);
            let truncated: String = bytes[..i].iter().collect();
            let _ = verify_key(&truncated, &pk, 0);
        }
        for junk in [
            "\u{0}\u{0}\u{0}",
            "....",
            "ZVL1...",
            "ZVL1.\u{202e}.\u{202e}",
            "ZVL1.A.A",
            "-----BEGIN PRIVATE KEY-----",
        ] {
            let _ = verify_key(junk, &pk, i64::MIN);
            let _ = verify_key(junk, &pk, i64::MAX);
        }
    }

    // ── is_pro / mask_key ─────────────────────────────────────────

    /// 発行側 (`tools/licgen`) は独立クレートなので Base64 の設定を重複して
    /// 持っている。アルファベット (URL_SAFE) とパディング無しがズレると
    /// 「発行したキーが検証できない」になるため、固定ベクタで綴じ方を止める。
    /// この文字列は `base64.urlsafe_b64encode(...).rstrip("=")` と一致する。
    #[test]
    fn format_is_stable_across_issuer() {
        const JSON: &str =
            r#"{"sub":"a@b.test","tier":"pro","iat":1700000000,"exp":null,"seats":1}"#;
        const B64: &str = "eyJzdWIiOiJhQGIudGVzdCIsInRpZXIiOiJwcm8iLCJpYXQiOjE3MDAwMDAwMDAsImV4cCI6bnVsbCwic2VhdHMiOjF9";
        assert_eq!(B64_ENCODE.encode(JSON.as_bytes()), B64);
        assert_eq!(B64_DECODE.decode(B64).expect("decode"), JSON.as_bytes());
        // パディングは有っても無くても解ける (メール経由の欠落・付与に耐える)。
        // 上の JSON は 4 の倍数長に収まるので、パディングが付く例は別に用意する。
        assert_eq!(B64_DECODE.decode("YWI").expect("nopad"), b"ab");
        assert_eq!(B64_DECODE.decode("YWI=").expect("padded"), b"ab");
        // 標準アルファベット (+ /) は使わない — URL_SAFE のみ
        assert!(B64_DECODE.decode("a+b/c").is_err());
    }

    #[test]
    fn is_pro_table() {
        let cases: Vec<(LicenseStatus, bool)> = vec![
            (LicenseStatus::Unlicensed, false),
            (LicenseStatus::BadSignature, false),
            (LicenseStatus::Malformed("x".into()), false),
            (LicenseStatus::Expired { exp: 1 }, false),
            (
                LicenseStatus::Valid {
                    tier: "pro".into(),
                    sub: "a".into(),
                    exp: None,
                    seats: 1,
                },
                true,
            ),
            (
                LicenseStatus::Valid {
                    tier: "PRO".into(),
                    sub: "a".into(),
                    exp: Some(9),
                    seats: 1,
                },
                true,
            ),
            (
                LicenseStatus::Valid {
                    tier: "team".into(),
                    sub: "a".into(),
                    exp: None,
                    seats: 5,
                },
                true,
            ),
            (
                LicenseStatus::Valid {
                    tier: "free".into(),
                    sub: "a".into(),
                    exp: None,
                    seats: 1,
                },
                false,
            ),
            (
                LicenseStatus::Valid {
                    tier: " Free ".into(),
                    sub: "a".into(),
                    exp: None,
                    seats: 1,
                },
                false,
            ),
        ];
        for (st, want) in cases {
            assert_eq!(is_pro(&st), want, "status={st:?}");
        }
    }

    #[test]
    fn mask_key_table() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("short"), "•••••");
        assert_eq!(mask_key("123456789012"), "••••••••••••");
        assert_eq!(mask_key("1234567890123"), "123456…0123");
        assert_eq!(mask_key("ZVL1.abcdefghij.klmnop"), "ZVL1.a…mnop");
        // 非 ASCII でも文字境界で切るので panic しない
        assert_eq!(
            mask_key("あいうえおかきくけこさしす"),
            "あいうえおか…こさしす"
        );
    }

    #[test]
    fn format_unix_date_table() {
        assert_eq!(format_unix_date(0), "1970-01-01");
        assert_eq!(format_unix_date(86_399), "1970-01-01");
        assert_eq!(format_unix_date(86_400), "1970-01-02");
        assert_eq!(format_unix_date(951_782_400), "2000-02-29"); // 閏年
        assert_eq!(format_unix_date(1_700_000_000), "2023-11-14");
        assert_eq!(format_unix_date(-1), "-");
    }

    // ── 保存場所 ──────────────────────────────────────────────────

    #[test]
    fn save_load_clear_roundtrip() {
        let dir = crate::test_util::unique_temp_dir("zaivern-license-test", "roundtrip");
        let path = dir.join("nested").join("license.key");
        assert_eq!(load_key_from(&path), None);
        save_key_to(&path, "  ZVL1.abc.def \n").expect("save");
        assert_eq!(load_key_from(&path).as_deref(), Some("ZVL1.abc.def"));
        clear_key_at(&path).expect("clear");
        assert_eq!(load_key_from(&path), None);
        // 元から無いファイルの削除も成功扱い
        clear_key_at(&path).expect("clear again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_file_is_owner_only_on_unix() {
        let dir = crate::test_util::unique_temp_dir("zaivern-license-test", "perm");
        let path = dir.join("license.key");
        save_key_to(&path, "ZVL1.a.b").expect("save");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode={mode:o}");
        }
        #[cfg(not(unix))]
        {
            // Windows は既定 ACL に任せる (harden は意図的な no-op)。
            // ファイルが読み書きできることだけ確かめる。
            assert!(load_key_from(&path).is_some());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_reads_as_none() {
        let dir = crate::test_util::unique_temp_dir("zaivern-license-test", "empty");
        let path = dir.join("license.key");
        std::fs::write(&path, "   \n\t\n").expect("write");
        assert_eq!(load_key_from(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn license_path_is_under_zaivern_dir_and_not_hardcoded() {
        let p = license_path();
        assert!(p.ends_with("license.key"), "{p:?}");
        assert_eq!(p.parent(), Some(crate::config::zaivern_dir().as_path()));
    }

    /// 発行ツール (`tools/licgen`) が実際に作ったキーを、この検証側が
    /// 受け付けるかを確かめるリリース前フック。
    ///
    /// 既定では**何もしない** (環境変数が無ければ即 return)。出荷ビルドの
    /// 検証は次のように行う:
    ///
    /// ```text
    /// KEY=$(licgen issue --secret <秘密鍵> --sub qa@example.com --days 1)
    /// ZAIVERN_LICENSE_PUBKEY=<hex64> ZAIVERN_TEST_LICENSE_KEY="$KEY" \
    ///   cargo test license::issued_key_from_licgen_verifies
    /// ```
    #[test]
    fn issued_key_from_licgen_verifies() {
        let Ok(key) = std::env::var("ZAIVERN_TEST_LICENSE_KEY") else {
            return;
        };
        assert!(
            pubkey_configured(&EMBEDDED_PUBKEY),
            "ZAIVERN_TEST_LICENSE_KEY を渡すときは ZAIVERN_LICENSE_PUBKEY も要ります"
        );
        let st = verify_key(&key, &EMBEDDED_PUBKEY, now_unix());
        assert!(matches!(st, LicenseStatus::Valid { .. }), "status={st:?}");
        assert!(is_pro(&st));
    }

    /// 埋め込み公開鍵が未設定のビルドでは、いかなるキーも Pro にならない。
    /// (= 開発ビルドが誤って Pro を解錠しない)
    #[test]
    fn dev_build_without_pubkey_never_unlocks_pro() {
        if pubkey_configured(&EMBEDDED_PUBKEY) {
            return; // 公式ビルド (環境変数で公開鍵を注入済み) では対象外
        }
        let (sk, _) = test_keys("dev-build");
        let key = issue(&sk, &payload(None));
        assert!(!is_pro(&verify_key(&key, &EMBEDDED_PUBKEY, 0)));
    }
}
