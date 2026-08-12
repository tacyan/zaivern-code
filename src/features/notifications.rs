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
//! **パレット項目は置いていない。** 機能側から `ZaivernApp` のフィールドへは
//! 触れないので、パレットから切り替えても `app.cfg` が古いままになり、
//! 設定画面のチェックが嘘を表示する。到達経路は設定画面 1 つに絞ってある
//! (「同じ操作への到達経路が 3 つあるなら 2 つ削る」)。トースト付きで
//! パレットからも切り替えたくなったら、`app.rs` 側に
//! `pub(crate) fn set_notifications_enabled(&mut self, on: bool)` を置いて
//! ここから呼ぶ — それが `set_blame_mode` と同じ作法。

use crate::feature::{Feature, Setting, SettingValue};

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

pub const FEATURE: Feature = Feature {
    module: "notifications",
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
