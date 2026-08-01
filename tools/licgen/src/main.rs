//! Zaivern Code のライセンスキー発行ツール — **販売者だけが使う**。
//!
//! メインの `zai` バイナリとは別クレートなので、リポジトリ直下の
//! `cargo build --release` では**一切ビルドされない**。秘密鍵を扱うコードを
//! 出荷物へ近づけないための分離である。
//!
//! ```text
//! # 1) 鍵ペアを作る (秘密鍵はリポジトリ外の安全な場所へ)
//! cargo run --manifest-path tools/licgen/Cargo.toml -- keygen --out ~/zaivern-licgen.secret
//!
//! # 2) 出力された公開鍵 (16 進 64 桁) をビルド時に埋め込む
//! ZAIVERN_LICENSE_PUBKEY=<hex64> cargo build --release
//!
//! # 3) キーを発行する
//! cargo run --manifest-path tools/licgen/Cargo.toml -- issue \
//!     --secret ~/zaivern-licgen.secret \
//!     --sub buyer@example.com --tier pro --days 365 --seats 1
//! ```
//!
//! 詳しくは `docs/licensing.md`。
//!
//! ## 形式 (src/license.rs と一致させること)
//!
//! ```text
//! ZVL1.<Base64URL_nopad(payload_json)>.<Base64URL_nopad(ed25519_sig 64B)>
//! ```
//! 署名対象は `"ZVL1." + <payload セグメントの ASCII 文字列>`。
//!
//! Base64 の設定 (URL_SAFE / パディング無し) は検証側と同じものを、
//! 独立クレートゆえに**意図的に重複させて**いる。共有すると本体の
//! `crate::config` まで引き込むことになり、分離の意味が消えるため。
//! ズレると `src/license.rs` の `format_is_stable_across_issuer` テストが落ちる。

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const KEY_PREFIX: &str = "ZVL1";

const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_encode_padding(false),
);

/// 解く側は**パディングの有無を問わない**。検証側 (src/license.rs の
/// `B64_DECODE`) と同じ設定にしておかないと、パディング無しで綴じた自分の
/// 出力を自分で検証できない (実際に踏んだ)。
const B64_DECODE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

const USAGE: &str = "\
licgen — Zaivern Code ライセンスキー発行ツール (販売者用)

使い方:
  licgen keygen --out <秘密鍵ファイル>
      Ed25519 の鍵ペアを作る。秘密鍵は 16 進 64 桁で <out> に書き
      (unix では 0600)、公開鍵は標準出力に出す。
      **秘密鍵はリポジトリの外に置くこと。**

  licgen pubkey --secret <秘密鍵ファイル>
      秘密鍵から公開鍵 (16 進 64 桁) を出し直す。

  licgen issue --secret <秘密鍵ファイル> --sub <購入者> [オプション]
      ライセンスキーを 1 本発行して標準出力に出す。
        --tier  <名前>   等級 (既定: pro)。\"free\" は Pro 扱いにならない
        --days  <日数>   今日から <日数> 日で失効 (既定: 365)
        --exp   <unix秒> 失効時刻を直接指定 (--days より優先)
        --never          無期限で発行する。**失効できない**ので原則使わない
        --seats <人数>   座席数 (既定: 1。表示のみで強制はしない)

  licgen verify --pubkey <16進64桁> --key <ライセンスキー>
      発行したキーを検証側と同じ手順で確かめる (出荷前の自己点検用)。
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    let r = match sub {
        "keygen" => cmd_keygen(rest),
        "pubkey" => cmd_pubkey(rest),
        "issue" => cmd_issue(rest),
        "verify" => cmd_verify(rest),
        "-h" | "--help" | "help" | "" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(format!("知らないサブコマンド: {other}\n\n{USAGE}")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ── 引数 ──────────────────────────────────────────────────────────

/// `--name value` を拾う。値が無ければ `None`。
fn opt(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn need(args: &[String], name: &str) -> Result<String, String> {
    opt(args, name).ok_or_else(|| format!("{name} が要ります\n\n{USAGE}"))
}

// ── 16 進 ─────────────────────────────────────────────────────────

fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn from_hex32(s: &str) -> Result<[u8; 32], String> {
    let t: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if t.len() != 64 {
        return Err(format!("16 進 64 桁である必要があります (今: {} 文字)", t.len()));
    }
    let b = t.as_bytes();
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = nibble(b[i * 2])?;
        let lo = nibble(b[i * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("16 進以外の文字があります: {:?}", c as char)),
    }
}

// ── 秘密鍵ファイル ────────────────────────────────────────────────

fn write_secret(path: &Path, sk: &SigningKey) -> Result<(), String> {
    if let Some(d) = path.parent() {
        if !d.as_os_str().is_empty() {
            std::fs::create_dir_all(d).map_err(|e| format!("{} を作成できません: {e}", d.display()))?;
        }
    }
    std::fs::write(path, format!("{}\n", to_hex(sk.as_bytes())))
        .map_err(|e| format!("{} を書けません: {e}", path.display()))?;
    harden(path);
    Ok(())
}

fn read_secret(path: &Path) -> Result<SigningKey, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("{} を読めません: {e}", path.display()))?;
    let seed = from_hex32(raw.trim())?;
    Ok(SigningKey::from_bytes(&seed))
}

/// OS の CSPRNG から 32 バイトの種を取って署名鍵を作る。
///
/// `SigningKey::generate` は「RNG から 32 バイト読んで `from_bytes`」を
/// するだけなので、rand / rand_core の版差に巻き込まれないよう
/// getrandom を直に呼ぶ。乱数が取れない環境では**鍵を作らない**
/// (弱い種で発行するより失敗した方が安全)。
fn new_signing_key() -> Result<SigningKey, String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("OS から乱数を取得できません: {e}"))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// 秘密鍵ファイルを本人だけが読めるようにする (失敗しても続行)。
#[cfg(unix)]
fn harden(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// Windows 側。std に ACL API が無く、保存先はユーザーが選んだ場所なので、
/// ここでは何もせず「置き場所に気をつけること」を出力で促す。
#[cfg(windows)]
fn harden(_path: &Path) {}

#[cfg(not(any(unix, windows)))]
fn harden(_path: &Path) {}

// ── サブコマンド ──────────────────────────────────────────────────

fn cmd_keygen(args: &[String]) -> Result<(), String> {
    let out = PathBuf::from(need(args, "--out")?);
    if out.exists() {
        return Err(format!(
            "{} は既にあります。上書きすると発行済みキーが全て検証できなくなるので、\
             別のパスを指定するか、意図的なら先に手で退避してください",
            out.display()
        ));
    }
    let sk = new_signing_key()?;
    write_secret(&out, &sk)?;
    let pk = to_hex(sk.verifying_key().as_bytes());
    println!("秘密鍵: {}  ← リポジトリの外の安全な場所へ。失うと再発行できません", out.display());
    println!("公開鍵 (ZAIVERN_LICENSE_PUBKEY): {pk}");
    println!();
    println!("公式ビルドの作り方:");
    println!("  ZAIVERN_LICENSE_PUBKEY={pk} cargo build --release");
    Ok(())
}

fn cmd_pubkey(args: &[String]) -> Result<(), String> {
    let sk = read_secret(Path::new(&need(args, "--secret")?))?;
    println!("{}", to_hex(sk.verifying_key().as_bytes()));
    Ok(())
}

fn cmd_issue(args: &[String]) -> Result<(), String> {
    let sk = read_secret(Path::new(&need(args, "--secret")?))?;
    let sub = need(args, "--sub")?;
    if sub.trim().is_empty() {
        return Err("--sub が空です".into());
    }
    let tier = opt(args, "--tier").unwrap_or_else(|| "pro".into());
    let seats: u32 = opt(args, "--seats")
        .unwrap_or_else(|| "1".into())
        .parse()
        .map_err(|_| "--seats は正の整数で指定してください".to_string())?;
    if seats == 0 {
        return Err("--seats は 1 以上で指定してください".into());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "システム時計が 1970 より前です".to_string())?
        .as_secs() as i64;

    let exp: Option<i64> = if flag(args, "--never") {
        eprintln!(
            "warning: 無期限で発行します。オフライン検証では**失効できない**ので、\
             返金・共有・流出のいずれにも対処できません"
        );
        None
    } else if let Some(e) = opt(args, "--exp") {
        Some(e.parse::<i64>().map_err(|_| "--exp は Unix 秒で指定してください".to_string())?)
    } else {
        let days: i64 = opt(args, "--days")
            .unwrap_or_else(|| "365".into())
            .parse()
            .map_err(|_| "--days は整数で指定してください".to_string())?;
        Some(now + days * 86_400)
    };

    // 検証側の Payload と同じ形。キーは sub / tier / iat / exp / seats。
    let payload = serde_json::json!({
        "sub": sub,
        "tier": tier,
        "iat": now,
        "exp": exp,
        "seats": seats,
    });
    let json = serde_json::to_string(&payload).map_err(|e| format!("JSON 化に失敗: {e}"))?;
    let key = assemble(&sk, json.as_bytes());

    // 出荷前の自己点検 — 自分で検証してから出す
    verify(&sk.verifying_key(), &key)?;
    println!("{key}");
    Ok(())
}

fn cmd_verify(args: &[String]) -> Result<(), String> {
    let pk = from_hex32(&need(args, "--pubkey")?)?;
    let vk = VerifyingKey::from_bytes(&pk).map_err(|e| format!("公開鍵が不正です: {e}"))?;
    let key = need(args, "--key")?;
    verify(&vk, &key)?;
    println!("ok — 署名は正しく、この公開鍵で検証できます");
    Ok(())
}

// ── 綴じる / 確かめる ─────────────────────────────────────────────

fn assemble(sk: &SigningKey, payload: &[u8]) -> String {
    let p = B64.encode(payload);
    let signed = format!("{KEY_PREFIX}.{p}");
    let sig = sk.sign(signed.as_bytes());
    format!("{signed}.{}", B64.encode(sig.to_bytes()))
}

fn verify(vk: &VerifyingKey, key: &str) -> Result<(), String> {
    let compact: String = key.chars().filter(|c| !c.is_whitespace()).collect();
    let parts: Vec<&str> = compact.split('.').collect();
    if parts.len() != 3 || !parts[0].eq_ignore_ascii_case(KEY_PREFIX) {
        return Err("ライセンスキーの形式が違います".into());
    }
    let sig_bytes = B64_DECODE
        .decode(parts[2])
        .map_err(|e| format!("署名を読み取れません: {e}"))?;
    let sig_arr = <[u8; 64]>::try_from(sig_bytes.as_slice())
        .map_err(|_| "署名の長さが不正です".to_string())?;
    let signed = format!("{KEY_PREFIX}.{}", parts[1]);
    ed25519_dalek::VerifyingKey::verify_strict(
        vk,
        signed.as_bytes(),
        &ed25519_dalek::Signature::from_bytes(&sig_arr),
    )
    .map_err(|_| "署名が一致しません".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_key_verifies_and_survives_whitespace() {
        let sk = new_signing_key().expect("鍵生成");
        let key = assemble(&sk, br#"{"sub":"a@b.test","tier":"pro","iat":1,"exp":null,"seats":1}"#);
        let vk = sk.verifying_key();
        verify(&vk, &key).expect("そのまま検証できる");
        verify(&vk, &format!("  {key}\n")).expect("空白混じりでも検証できる");
        // 1 文字でも変えたら落ちる
        let mut bad: Vec<char> = key.chars().collect();
        let last = bad.len() - 1;
        bad[last] = if bad[last] == 'A' { 'B' } else { 'A' };
        assert!(verify(&vk, &bad.into_iter().collect::<String>()).is_err());
    }

    #[test]
    fn hex_roundtrip() {
        let sk = new_signing_key().expect("鍵生成");
        let hex = to_hex(sk.as_bytes());
        assert_eq!(from_hex32(&hex).unwrap(), *sk.as_bytes());
        assert!(from_hex32("zz").is_err());
        assert!(from_hex32(&"g".repeat(64)).is_err());
    }
}
