# subagent

`spawn`はsubagentを1波で実行する。各subagentは専用のシステムプロンプト、許可ツールの部分集合、サンドボックス、ターン数と壁時計の上限を持つ独立したループで動作する。型のJSON Schemaに照合済みの結果だけを親へ返し、途中で読んだファイルや失敗したツール呼び出しは親のコンテキストへ入らない。

深さの上限は2（`spawn::MAX_DEPTH`）で、subagentには、型の`allowed-tools`に`spawn`が挙がっている場合に限って`spawn`を渡す。同梱の型はどれも挙げていないので、同梱の型だけを使う限り、subagentがさらにsubagentを起動する経路はない。返り値はタスクごとに1要素のJSON配列で、順序は入力タスクに一致する。重なる`write_root`を同じ波で宣言した場合は実行せず、すべての要素を`ok: false`で返す。

```json
[
  {"type": "file-inspector", "ok": true, "result": {"path": "src/agent.rs", "responsibility": "..."}},
  {"type": "file-inspector", "ok": false, "error": "schema mismatch after retry (SchemaMismatch): ..."}
]
```

## 型を増やす前に確かめること

**型は能力を増やさない。** subagentが使えるのは、常時の6本（`read`・`write`・`edit`・`bash`・`skill`・`spawn`）のうち`allowed-tools`が挙げたものだけで、`spawn`は常に除く。型が決めるのは、どの能力を使うか、書き込めるか、どれだけの時間とターンを使うか、どの形で返すかである。新しい能力を持ち込む経路はない。

このため、**用途ごとに型を足す前に、既存の型が持つ能力の組み合わせで表せないかを確かめる。** 表せるなら型を足さない。

型を分ける理由になるのは次のいずれかである。

- 権限が違う。書き込めない作業を、書き込める型に任せない。
- 文脈が違う。親の履歴を持ち込みたくない、あるいは持ち込む必要がある。
- 返す形が違う。呼び出し側が照合するJSON Schemaが別である。
- 費用の見積もりが違う。`polaris-tier`が別になる。

理由にならないのは「その作業に名前を付けたいから」である。用途ごとに型を足し続けると、型の数だけプロンプト、許可設定、選定、引き継ぎが増え、全体の挙動を誰も説明できなくなる。人間の開発者が作業ごとに別人になるわけではないのと同じで、同じ型が読む・探す・変えるを組み合わせて働く方が、未知の作業に対応できる。

同梱する型は6つで、いずれも上の基準のどれかで区別される。

| 型 | 層（上限） | 権限 | ターン／秒 | 返す形 |
|---|---|---|---|---|
| `file-reader` | dwarf | 読取り | 10／300 | 単一のmarkdown（各ファイルの処理と接続。問いがあれば回答の節） |
| `file-inspector` | dwarf | 読取り | 12／360 | 責務と対応するテスト |
| `files-md-writer` | dwarf | 読み書き | 4／60 | `files.md`の保存結果 |
| `reviewer` | giant | 読取り＋bash | 24／900 | 根拠付きの指摘 |
| `doc-writer` | giant（下限subgiant） | 読み書き | 20／900 | 保存した文書 |
| `implementer` | supergiant | 読み書き＋bash | 40／1800 | 変更の要約（継続あり） |

### file-reader

1つ以上のファイルまたはフォルダ（絶対パス）を読み、各ファイルの「処理」と「他ファイルとの接続」だけを、単一のmarkdownで返す。単一ファイルでも複数ファイルでも、同じ形式である。返り値は`{"markdown": "..."}`の1フィールドで、最大6,000字。行の書式はスキーマの`pattern`で検査し、外れたら1回だけ再試行する。

```
# <対象名>
- `相対パス` — 処理: <1文> ／ 接続: <相手の相対パスと関数名>
## 回答        （依頼に問いがあるときだけ。`相対パス:行`の根拠、最大4件）
## 未読        （読まなかった対象があるときだけ。最大10件）
```

読むのは最大40ファイルまでで、超える分、バイナリ、巨大なファイルは「未読」に書く。索引は要約であって根拠ではない。編集するファイルは、親が自分で読む。workflowのReview段階では、ホストがルートの`read`を問い付きで`file-reader`に振り替えるため、そのときは「回答」の節が証拠になる。

## 設定

`~/.polaris/config.toml`と`<project-root>/.polaris/config.toml`を読み、後者が優先する。

| キー | 既定値 | 意味 |
| --- | --- | --- |
| `[agents] paths` | `[]` | 型を探す追加ディレクトリ。`<project-root>/agents`と`~/.polaris/agents`は常に探索する。 |
| `[spawn] concurrency` | `8` | 1波で同時に実行するタスク数の上限。 |
| `[spawn] write_concurrency` | `4` | `write_root`を持つタスクの同時実行上限。 |

workflowが有効な親は、必須skill設定と段階を子へ渡す。子の段階は型のmetadataに`polaris-phase: review`などの指定があればそれを使い、なければ親の現在段階を引き継ぐ。親の承認、完了、検証の証跡は引き継がず、子の新しい作業に対する許可や完了根拠にはならない。

```toml
[agents]
paths = ["/path/to/shared/agents"]

[spawn]
concurrency = 8
write_concurrency = 4
```

## 型の定義

1つの型は`agents/<型名>/SKILL.md`で定義する。ディレクトリ名とfrontmatterの`name`は一致しなければならない。

```markdown
---
name: file-inspector
description: 単一ファイルを読み取り専用で棚卸しし、責務、入出力、対応するテストを返す。
allowed-tools: read
metadata:
  polaris-access: read
  polaris-tier: dwarf
  polaris-wall-seconds: "360"
  polaris-max-turns: "12"
  polaris-continuation: "denied"
  polaris-output: references/result.schema.json
---
```

frontmatter直後の本文が型のシステムプロンプトになる。`allowed-tools`は空白区切りで、`spawn`は、深さの上限に達した子には渡さない。`metadata`の6キーはすべて必須で、欠けた型は起動時に理由付きで読み飛ばす。任意のキーは`polaris-tier-min`（層の下限）と`polaris-phase`（子のworkflow段階）である。

| キー | 値 | 意味 |
| --- | --- | --- |
| `polaris-access` | `read` / `read-write` | `read`は読み取り専用。`read-write`は`write_root`の指定を必要とし、親の書き込み可能ルート外を拒否する。 |
| `polaris-tier` | `dwarf` / `subgiant` / `giant` / `supergiant` | 型の重さ。光度階級を借りた4段で、設定がこれをモデルと推論強度へ対応させる。4つ以外の値を書いた型は、理由付きで読み飛ばす。旧綴りの`low`・`medium`・`ultra`はここでは**受け付けない**（推論強度の値と衝突するため改名した。保存済みのrole束縛だけは`serde(alias)`で読める）。 |
| `polaris-wall-seconds` | 秒数 | 1タスクの壁時計上限。超過したタスクだけを失敗として返す。 |
| `polaris-max-turns` | ターン数 | 1タスクのターン上限。スキーマ不一致の再試行も同じ予算を使う。 |
| `polaris-continuation` | `denied` / `allowed` | 波をまたぐ継続の可否を表す。 |
| `polaris-output` | 相対パス | 型のディレクトリ内にある結果JSON Schema。絶対パスと`../`は拒否する。 |

ホスト型の`codex`providerでは、`[tier.*]`にモデルを書かなかった層は次のモデルで動く。`[tier.*]`に書いた値は常に優先する。他のproviderでは既定を置かず、書かなかった層は親のモデルのままである。

| 層 | 既定のモデル | 用途 |
|---|---|---|
| `dwarf` | `gpt-6-luna` | 機械的で自己完結した軽い作業だけ |
| `subgiant`（既定の層） | `gpt-6.1-terra` | 細かい実装、テストケース作成、文書の調整 |
| `giant` | `gpt-6.1-sol` | 主力のエージェンティックなコーディング |
| `supergiant` | `gpt-6-astra` | モデルの大きさが効く難しい作業 |

**luna（`gpt-6-luna`）は推論強度`high`未満では使わない。** 上位モデルの低effortより、下位モデルの高effortのほうが安く済むためである。セッションの`--effort`や`[tier.dwarf] effort`がこれより低いときは、`high`に引き上げて起動時に1行で知らせる（`xhigh`以上はそのまま）。他の層は、指定どおりに従う。

子は親と別のモデルで動くため、親のprompt cacheは効かない。推論強度は層ごとの既定を置かず、`[tier.*]`の指定がなければセッションの`--effort`に従う。

出力がSchemaに合わないときは検証エラーを添えて1回だけ再試行する。2回目も失敗すればそのタスクを失敗として返す。`polaris-continuation: allowed`の型（同梱では`implementer`）は、型と`write_root`をキーに、前回の会話を引き継ぐ。保持するのは最大8件で、溢れたら古い順に捨てる（捨てた会話は、次回に読み直すだけで失われない）。
