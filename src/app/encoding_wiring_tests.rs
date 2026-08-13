/// ピッカーは**実測で使えるものだけ**を並べる。
/// 決め打ちの表を出すと、選んだのに保存できない項目が混ざる。
#[test]
fn ピッカーは使える符号化しか並べない() {
    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn encoding_picker_ui(")
        .nth(1)
        .expect("ピッカーがある");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(
        head.contains("crate::textenc::supported_encodings()"),
        "実測の一覧を使っていない"
    );
    // 並んだ項目はすべて名前から引き直せる (= 選択がそのまま通る)
    for info in crate::textenc::supported_encodings() {
        assert_eq!(
            crate::textenc::encoding_by_name(&info.id),
            Some(info.enc),
            "{} を id から引けない",
            info.id
        );
        assert!(
            crate::textenc::is_supported(info.enc),
            "{} が使えない",
            info.id
        );
    }
}

/// 化けた開き直しは**件数付きで警告**する (黙って壊れた本文を見せない)。
#[test]
fn 化けた開き直しは件数付きで警告する() {
    // UTF-8 として不正なバイト列 (CP932 の「あ」)
    let bytes = [0x82u8, 0xA0];
    let rep = crate::textenc::reopen_with_report(&bytes, crate::textenc::Encoding::Utf8);
    assert!(rep.lossy(), "UTF-8 では読めないので化ける");
    assert!(rep.replacements > 0, "化けた箇所を数えている");

    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn reopen_with_encoding(")
        .nth(1)
        .expect("開き直しがある");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(head.contains("rep.lossy()"), "化けたかどうかを見ていない");
    assert!(head.contains("rep.replacements"), "件数を出していない");
    assert!(head.contains("箇所が化けています"), "警告の文言が無い");
}

/// 保存できない文字があったら、保存せずに**その文字へキャレットを飛ばす**。
#[test]
fn 保存失敗はキャレットを問題の文字へ飛ばす() {
    use crate::textenc::{Encoding, LineEnding};
    // ASCII しか通らない符号化が実測一覧にあるとは限らないので、
    // 「表せない文字」を確実に作れる CP932 系が使える環境でだけ本体を見る。
    if let Some(enc) = crate::textenc::encoding_by_name("cp932") {
        // 𠮟 (U+20B9F) は CP932 に無い
        let text = "abc𠮟def";
        let issue = crate::textenc::save_with(text, enc, LineEnding::Lf)
            .expect_err("表せない文字があるので断られる");
        assert_eq!(issue.char_index(), Some(3), "4 文字目 (0 起点で 3)");
        assert!(!issue.message().is_empty(), "説明文が出せる");
    } else {
        // 変換表が無い環境: Unsupported を返し、位置は無い
        let issue = crate::textenc::save_with("abc", Encoding::Ansi(932), LineEnding::Lf)
            .expect_err("この環境では使えない");
        assert_eq!(issue.char_index(), None);
    }

    let src = &crate::app::SRC.replace("\r\n", "\n");
    let body = src
        .split("fn save_with_encoding(")
        .nth(1)
        .expect("保存がある");
    let head = &body[..body.find("\n    /// ").unwrap_or(body.len())];
    assert!(
        head.contains("if let Some(ix) = issue.char_index()"),
        "問題の文字位置を使っていない"
    );
    assert!(
        head.contains("self.pending_select = Some((ix, ix + 1));"),
        "キャレットを飛ばしていない"
    );
    assert!(head.contains("issue.message()"), "理由を出していない");
}
