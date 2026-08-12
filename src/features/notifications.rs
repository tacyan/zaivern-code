//! 通知のオン/オフと、通知音のオン/オフ。
//!
//! **設定 2 つだけの機能。** `Setting` を宣言すると
//! [`crate::config::all_setting_defs`] が拾うので、設定画面 (⚙) に行が出て、
//! 検索にも `@modified` にも「既定へ戻す」にも自動で乗る —
//! `config.rs` の設定一覧にも `palette.rs` にも 1 行も追記しない。
//!
//! 実行時の値は [`crate::notify`] の旗が持つ。設定 → 旗の反映は
//! [`crate::config::apply_runtime_flags`] が 1 か所で行う
//! (通知モジュールは依存を持たない層なので、設定を読む経路を持ち込まない)。
//!
//! **切り替えは `app.rs` の glue 越しに行う。** 機能側から `ZaivernApp` の
//! フィールドを直接触ると `app.cfg` が古いまま残り、設定画面のチェックが
//! 嘘を表示する。そこで [`crate::app::ZaivernApp::set_notify_sound`] を
//! 呼ぶ — 設定画面が使うのと**同じ書き戻し経路** (`config.toml` への保存と
//! `apply_runtime_flags` まで) を通るので、どこから切り替えても状態が 1 つに保たれる。
//!
//! 到達経路は 2 つ: 設定画面 (⚙) の行と、ペットメニュー / パレットの
//! 「🔔 通知音」。**同じ真実源 (`KEY_SOUND`) を指しているので増やしても嘘が出ない**
//! (削るべきなのは「別々の状態を持つ重複」であって、同じ 1 つの設定への
//! 近道ではない)。

use crate::feature::{Entry, Feature, Setting, SettingValue};

/// 通知を出すかどうかの設定キー。**既定はオン** (いままでの挙動)。
///
/// 欄が無い古い `config.toml` でも [`crate::config::Config::feature_bool`] が
/// 宣言された既定へ落ちるので、そのままオンとして読める。
pub const KEY_ENABLED: &str = "notifications.enabled";

/// 通知**音**を鳴らすかどうかの設定キー。**既定はオン** (いままでの挙動)。
///
/// **3 段 (オフ/無音/オン) ではなく `enabled` と独立した真偽値にした。**
/// `Setting` が持てる型は Bool/Int/Float/Text だけなので、3 段は設定画面が
/// 数値欄か自由入力になって既存のチェックボックスより悪くなり、しかも
/// 旧 `enabled` からの移行が要る。独立させると生まれる「通知オフなのに
/// 音オン」という無意味な組は、`notify` 側がオフの時点で計画ごと捨てるので
/// 観測できない (到達経路は設定画面の 2 行だけで、パレットには出さない)。
pub const KEY_SOUND: &str = "notifications.sound";

/// 通知音を切り替える [`Entry::id`]。ペットメニューからも直に指す。
pub const ID_TOGGLE_SOUND: &str = "notifications.toggle_sound";

pub const FEATURE: Feature = Feature {
    module: "notifications",
    entries: &[Entry {
        icon: "🔔",
        label: "通知音のオン/オフ",
        id: ID_TOGGLE_SOUND,
    }],
    dispatch: |app, ctx, id| {
        if id != ID_TOGGLE_SOUND {
            return false;
        }
        // いまの値は**設定から読む** (`notify::sound()` の旗ではなく)。
        // 旗は設定から一方通行で写した派生値なので、書き戻す側がそちらを
        // 真実源に使うと向きが循環する。
        let now = app.notify_sound_enabled();
        app.set_notify_sound(!now, ctx);
        true
    },
    settings: &[
        Setting {
            key: KEY_ENABLED,
            label: "通知を出す",
            help: "オフにすると、OS の通知と webhook を一切送りません (プロセスを 1 つも起こさないので音も鳴りません)。画面のトーストと効果音は別の設定です。",
            default: SettingValue::Bool(true),
        },
        Setting {
            key: KEY_SOUND,
            label: "通知音を鳴らす",
            help: "オフにすると、通知は出しますが音は鳴らしません (macOS は sound name 句を外す / Windows はトーストを silent にする)。Linux は通知デーモンが suppress-sound ヒントを見る場合だけ効きます。「通知を出す」がオフなら、この設定は関係ありません。",
            default: SettingValue::Bool(true),
        },
    ],
    ..Feature::DEFAULT
};
