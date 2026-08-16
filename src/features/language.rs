//! Language Pack の道具立て — **翻訳する人がアプリの中だけで完結できる**ようにする。
//!
//! 言語を選ぶ操作そのものは 🌐 メニュー / 設定 / パレット (`Cmd::SetUiLanguage`)
//! が持っている。ここに置くのは、**新しい言語を作る／直す人**が要る 3 つ:
//!
//! 1. 置き場を開く … `~/.zaivern/locales` は初回は存在しない。作って開く
//! 2. 雛形を書き出す … 同梱 `en` を土台に「訳が入っていれば入った状態」で 1 枚出す。
//!    これを `fr.json` に直して置けばフランス語版になる
//! 3. 訳漏れを書き出す … `ZAIVERN_I18N_TRACE=1` で起動しているあいだに
//!    `tr()` が引けなかった文字列を集めたもの。**画面を触った順に増える**ので、
//!    「この画面がまだ日本語のまま」を突き止める最短経路になる
//!
//! 3 は環境変数が要る。**要らない人に費用を払わせない**ためで、既定では
//! `tr()` は 1 回の `OnceLock` 読み取りしかしない。

use crate::feature::{Entry, Feature};

pub const FEATURE: Feature = Feature {
    module: "language",
    entries: &[
        Entry {
            icon: "🌐",
            label: "表示言語: ファイルの置き場を開く",
            id: "language.open_dir",
        },
        Entry {
            icon: "🌐",
            label: "表示言語: 翻訳の雛形を書き出す",
            id: "language.export_template",
        },
        Entry {
            icon: "🌐",
            label: "表示言語: 訳が無い文字列を書き出す",
            id: "language.dump_missing",
        },
    ],
    dispatch: |app, _ctx, id| match id {
        "language.open_dir" => {
            app.open_locales_dir();
            true
        }
        "language.export_template" => {
            app.export_locale_template();
            true
        }
        "language.dump_missing" => {
            app.dump_missing_translations();
            true
        }
        _ => false,
    },
    ..Feature::DEFAULT
};
