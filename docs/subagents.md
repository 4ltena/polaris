# subagent

`spawn`はsubagentを1波で実行する。各subagentは専用のシステムプロンプト、許可ツールの部分集合、サンドボックス、ターン数と壁時計の上限を持つ独立したループで動作する。型のJSON Schemaに照合済みの結果だけを親へ返し、途中で読んだファイルや失敗したツール呼び出しは親のコンテキストへ入らない。

深さは1に固定する。subagentへ渡すツール一覧から`spawn`を除くため、subagentがさらにsubagentを起動する経路はない。返り値はタスクごとに1要素のJSON配列で、順序は入力タスクに一致する。重なる`write_root`を同じ波で宣言した場合は実行せず、すべての要素を`ok: false`で返す。

```json
[
  {"type": "file-inspector", "ok": true, "result": {"path": "src/agent.rs", "responsibility": "..."}},
  {"type": "file-inspector", "ok": false, "error": "schema mismatch after retry (SchemaMismatch): ..."}
]
```

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
  polaris-tier: low
  polaris-wall-seconds: "360"
  polaris-max-turns: "12"
  polaris-continuation: "denied"
  polaris-output: references/result.schema.json
---
```

frontmatter直後の本文が型のシステムプロンプトになる。`allowed-tools`は空白区切りで、`spawn`は指定しても渡さない。`metadata`の6キーはすべて必須で、欠けた型は起動時に理由付きで読み飛ばす。

| キー | 値 | 意味 |
| --- | --- | --- |
| `polaris-access` | `read` / `read-write` | `read`は読み取り専用。`read-write`は`write_root`の指定を必要とし、親の書き込み可能ルート外を拒否する。 |
| `polaris-tier` | 任意の文字列 | 型の重さの分類。現時点では実行挙動を変えない。 |
| `polaris-wall-seconds` | 秒数 | 1タスクの壁時計上限。超過したタスクだけを失敗として返す。 |
| `polaris-max-turns` | ターン数 | 1タスクのターン上限。スキーマ不一致の再試行も同じ予算を使う。 |
| `polaris-continuation` | `denied` / `allowed` | 波をまたぐ継続の可否を表す。 |
| `polaris-output` | 相対パス | 型のディレクトリ内にある結果JSON Schema。絶対パスと`../`は拒否する。 |

出力がSchemaに合わないときは検証エラーを添えて1回だけ再試行する。2回目も失敗すればそのタスクを失敗として返す。`polaris-continuation: allowed`は読み込み・保持するが、波をまたぐ継続経路自体が未実装のため、現時点の実行は`denied`と同じである。
