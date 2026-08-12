/// UI で使う記号が、実際のフォント構成で描画できることを保証する。
///
/// egui 同梱の NotoEmoji はサブセットで、macOS の Apple Color Emoji は
/// カラービットマップなので egui 0.29 では使えない。そのため一部の絵文字
/// (🤖 U+1F916 / 🦀 U+1F980 など) はどのフォントにも無く、豆腐(□)として描画される。
/// 見た目だけの問題に見えて、利用者にはボタンの意味が分からなくなる。
#[test]
fn ui_symbols_have_glyphs() {
    // UI 上で意味を担っている記号だけを並べる。
    // 末尾のひとかたまりは「エージェントを追加」ピッカーが並べるカタログのアイコン。
    const UI_SYMBOLS: &str = "👑📁📂👾🔌🌿🐙⚡🛡🚀💡💾🗑📝🔔🎤⏹⟳➕✅❌⚠🖥🔒📱🐾📄📋🔄🔗✋●○◇⇄◎⇩▶→✏🛠🔤\
                                  💬📊⛔🔁·↩▸▾🔎◆📇💤🎬🎵🔆\
                                  💬📊⛔🔁·↩▸▾🔎◆📇💤🗺🔗›…\
                                  💬📊⛔🔁·↩▸▾🔎◆📇💤👤🔖\
                                  🔍📡🖱📦🍚🌀🔷🔶🕊👷🐦🅰🌊⌘➡🔩🌙🎏🎐🐉💠";
    let ctx = egui::Context::default();
    super::install_fonts(&ctx);
    let _ = ctx.run(Default::default(), |_| {});
    let fid = egui::FontId::proportional(14.0);
    let missing: String = ctx.fonts(|f| {
        UI_SYMBOLS
            .chars()
            .filter(|c| !f.has_glyphs(&fid, &c.to_string()))
            .collect()
    });
    assert!(
        missing.is_empty(),
        "フォントに無い記号が UI で使われている(豆腐になる): [{missing}]"
    );
}

/// 絵文字ではない「記号」も豆腐になる。
///
/// egui 同梱の Ubuntu-Light / NotoEmoji にも、フォールバックに積む日本語
/// フォント (Yu Gothic / ヒラギノ) にも無い字がここに集まっている:
/// 閉じるの ✕、キーヒントの ⌘ ⌥ ⌃ ⇧ ⌫、ツリーの ▸ ▾、プロンプトの ❯、
/// 罫線、点字スピナー。`install_fonts` が記号フォントを積まないと
/// 「✕ 閉じる」が「□ 閉じる」になり、押せるボタンだと分からなくなる。
///
/// ターミナルは Monospace で描くので、両方の族で見る。
#[test]
fn ui_glyph_symbols_have_glyphs() {
    // すべて src/ か assets/ に実在する記号だけを並べる (未使用の字は入れない)。
    const SYMBOLS: &str = "✕✗✓✔⌫⌥⌃⇧⌘❯▸▾▲▼◆◇◎●○★⇄⇩→➡⟳⏱⏳─│╭╮╰╯┌┐└┘├┤┬┴┼\
                               ⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏±×÷·»‹›※–—…‣\
                               ⊞";
    let ctx = egui::Context::default();
    super::install_fonts(&ctx);
    let _ = ctx.run(Default::default(), |_| {});
    for fid in [
        egui::FontId::proportional(14.0),
        egui::FontId::monospace(14.0),
    ] {
        let missing: String = ctx.fonts(|f| {
            SYMBOLS
                .chars()
                .filter(|c| !f.has_glyphs(&fid, &c.to_string()))
                .collect()
        });
        assert!(
            missing.is_empty(),
            "{:?} に無い記号が UI で使われている(豆腐になる): [{missing}]",
            fid.family
        );
    }
}

/// カタログの 29 エージェントのアイコンは「エージェントを追加」ピッカーに
/// そのまま並ぶ。1 つでも豆腐になると、その行だけ意味が読めなくなるので
/// カタログ側のアイコンも UI 記号と同じ基準で検査する。
#[test]
fn catalog_icons_have_glyphs() {
    let ctx = egui::Context::default();
    super::install_fonts(&ctx);
    let _ = ctx.run(Default::default(), |_| {});
    let fid = egui::FontId::proportional(14.0);
    let missing: Vec<String> = ctx.fonts(|f| {
        crate::agents::AGENT_CATALOG
            .iter()
            .filter(|s| !f.has_glyphs(&fid, s.icon))
            .map(|s| format!("{}={}", s.bin, s.icon))
            .collect()
    });
    assert!(
        missing.is_empty(),
        "カタログのアイコンが豆腐になる: {missing:?}"
    );
}
