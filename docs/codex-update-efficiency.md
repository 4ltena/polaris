# Codex更新と使用量削減策の適用

2026-09-07にCodex CLI 0.153.4と公開リリース、関連PRを確認した。0.153.4の公開変更はAstraのモデル選択と非同期質問の案内修正であり、この版への更新で使用量が大幅に減るという根拠は確認できなかった。[0.153.4リリース](https://github.com/openai/codex/releases/tag/rust-v0.153.4)

## 確認した仕組み

| Codexの変更 | 分かること | Polarisでの扱い |
| --- | --- | --- |
| [自動recapの停止設定 #42101](https://github.com/openai/codex/pull/42101) | 自動要約の予約・要求・再試行を止め、手動recapは残す。 | 追加要求を選択的に止める考え方をfiles.md自動更新へ適用する。 |
| [Full Access時のGuardian省略 #42147](https://github.com/openai/codex/pull/42147)、[ユーザー承認時の背景採点停止 #42256](https://github.com/openai/codex/pull/42256) | 不要な内部レビュー要求を避ける。利用者の請求や利用枠への影響は不明。 | 対応するモデルレビュー処理がないため移植対象なし。Polarisの権限検査は維持する。 |
| [実験的context management #42385](https://github.com/openai/codex/pull/42385) | token budget、history notes、new_contextを使う。利用可能な認証・プラン・backendに制約がある。 | v0.11.0のstrict10・要約索引を別途検証する。未確認のbackend機能をコピーして有効化しない。 |
| [使用量の永続化 #41912](https://github.com/openai/codex/pull/41912) | 再開後にも使用量を復元する。直接の節約策ではない。 | physical attempt台帳と欠測・再試行の記録を検証する。通常CLIの台帳は現状メモリ内で、永続台帳APIがあることと区別する。 |
| [Fastの表示修正 #42632](https://github.com/openai/codex/pull/42632) | 案内文の変更であり、実行方式の変更ではない。 | 速度表示をトークン削減の根拠にしない。 |

## 適用する設定

スキルカタログには[独立したtoken予算 #38978](https://github.com/openai/codex/pull/38978)もある。Polarisはカタログ全体を常時送らないため、その上限値を導入しても現在の固定入力は減らない。また、Codexの[要約なしのcontext切替](https://github.com/openai/codex/blob/rust-v0.153.0/codex-rs/core/src/compact_token_budget.rs)は、原文と決定を保持するstrict10とは契約が異なる。要約要求を省くために過去の情報を失わせる変更は採用していない。

開発版では、`~/.polaris/config.toml`または`<project-root>/.polaris/config.toml`に次を指定するとfiles.mdの自動更新だけを停止できる。

```toml
[files_md]
auto_regenerate = false
```

既定はtrueで、従来の更新動作を維持する。falseではファイル変更を契機とするfiles-md-writerの追加モデル要求を起動しない。通常の応答や手動spawnは使えるが、files.mdを最新に保つには明示的な更新が必要になる。履歴の要約・保存、必須skill、サンドボックス、承認規則はこの設定の対象外である。

これは自動recap停止の設計を参考にした独自実装であり、Codexのコードを複製したものではない。追加要求が発生する作業では削減候補となるが、トークン数・費用・利用枠の削減率は未測定である。files-md-writerを導入していない環境では、この設定によるモデル要求の削減はない。

## キャッシュと利用枠を分けて評価する

Polarisには固定ツール順、安定したprompt_cache_key、ツール結果の退避、任意の要求間隔制御がある。prompt_cache_keyは経路選択のヒントであり、同一マシンへの固定やcache hitを保証しない。store:falseも、入力トークンを無料にする設定ではない。[公式Prompt Cachingガイド](https://developers.openai.com/api/docs/guides/prompt-caching)

公式ガイドではGPT-5.6以降のTTLは30分で、古いモデルのprompt_cache_retention設定とは異なる。公開Responses APIの設定がChatGPT契約のCodex endpointでも使えるとは限らないため、未確認のパラメータは追加していない。Astraのモデル自体の効率化も、すでにgpt-6-astra／mediumを使うPolarisの追加改善とは数えない。[Astraガイド](https://developers.openai.com/api/docs/guides/latest-model)

API換算費用、input・output・cacheのトークン数、ChatGPTの利用枠消費は別の指標である。CodexのFastはStandardより使用クレジットが多いと公式に説明されているが、この倍率を通常のAPI料金やすべての利用枠へ当てはめない。[公式Speedガイド](https://learn.chatgpt.com/docs/agent-configuration/speed)

採用判定では同じモデル・資料・課題・反復数で品質と範囲を確認し、キャッシュ率だけで判断しない。以前のPolaris比較でも、キャッシュ率を上げた構成が最安ではなかった。[キャッシュと費用の実測](gpt6-cache-cost-results.md)
