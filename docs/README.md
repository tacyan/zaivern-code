# docs/ — 何がどこに書いてあるか

**1 文書 = 1 つの保証。**「この主張の根拠はどれか」を引くための索引である。
製品としての紹介は [../README.md](../README.md)、開発規約は
[../CLAUDE.md](../CLAUDE.md) にある。

## 競合ゼロ (行域オーナーシップ)

| 文書 | 何を保証するか |
|---|---|
| [conflict-zero.md](conflict-zero.md) | **主張の線引きと全実測。**「競合ゼロ」が何を指し何を指さないか。§5 = 残っている穴、§6 = 反証された主張の履歴 |
| [czero-repo-shapes.md](czero-repo-shapes.md) | リポジトリの形 13 通りごとに、何が保証されて何が保証されないか |
| [anyrepo-proof.md](anyrepo-proof.md) | **利用者自身のリポジトリ**で再現する手順と、「証明できた」と名乗る条件 |
| [guard-edges.md](guard-edges.md) | 書き込みの関所が取りこぼす形と、その塞ぎ方 |
| [conflict-bench.md](conflict-bench.md) | ファイル単位のベンチ (`tools/conflict-bench.sh`) の数字と、その読み方 |
| [region-cost.md](region-cost.md) | 行域判定**そのもの**の費用。ハーネスの費用を分けて差し引く |

## 計測の作法

| 文書 | 何を保証するか |
|---|---|
| [bench-honesty.md](bench-honesty.md) | ベンチが「静かな嘘」をつかないための決まり (古いバイナリ・探索順・`$?`) |
| [xplat-bench.md](xplat-bench.md) | macOS と Linux を並べた結果。落とす指標が一致するか |
| [idle-cost.md](idle-cost.md) | アイドル時の CPU をどう測るか。**窓の長さを書かない数字は嘘になる** |

## 実装のしくみ

| 文書 | 何を保証するか |
|---|---|
| [workspace-key.md](workspace-key.md) | `~/.zaivern` の置き場を決める 16 桁の導出規則と、旧キーの引き取り |
| [licensing.md](licensing.md) | 完全オフラインのライセンス認証。**失効できない**ことを含む |
| [plugins.md](plugins.md) | プラグイン開発ガイド (利用者向け) |
| [PLUGIN_SPEC.md](PLUGIN_SPEC.md) | プラグイン基盤の実装仕様 (内部向け) |
| [UX_LOOP.md](UX_LOOP.md) | 操作性ループの進捗台帳。反復ごとの調査・統合ログ |

## 読むときの約束

* **数字には必ず条件が付いている。** 版・OS・体数・窓の長さ・種を見ずに
  引用しない。条件の書いていない数字を見つけたら、それは直すべき欠陥である。
* **反証された主張は消さずに履歴節へ置いてある** (例:
  [conflict-zero.md](conflict-zero.md) §6、[idle-cost.md](idle-cost.md)
  「覆った主張」)。**履歴節の数字を現在の事実として引かないこと。**
* **「要再測定」と書いてある項目は、まだ裏が取れていない。**
  現在わかっているものは [conflict-zero.md](conflict-zero.md) §5-1
  (0.15.0 の `--shift` 修正後の Linux 64 体) と
  [region-cost.md](region-cost.md) §8 (`is_disjoint` 早期脱出後の最悪ケース)。
* **`src/*.rs:行番号` は書かない。** 複数のエージェントが同時に編集するので
  行番号は必ず腐る。**ファイル名と記号名**で指すこと。
- [i18n.md](i18n.md) — 多言語化の仕組み
- [translating.md](translating.md) — 翻訳の手引き
