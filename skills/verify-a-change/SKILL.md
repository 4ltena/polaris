---
name: verify-a-change
description: polaris のコードを変更したあとに走らせる検証一式と、報告に何を添えるか。テスト、clippy、fmt、常時コンテキストの再測定。
---

# 変更を検証する

順に走らせる。どれか1つでも落ちたら、そこで止めて報告する。

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Linux 側の強制（landlock）に触れたなら、コンテナでも走らせる。ホストは
macOS なので、`polaris-sandbox` の Linux 経路はホストのテストでは動かない。

## 常時コンテキストを測り直す

ツールのスキーマ、ツールの本数、憲法や環境ブロックの上限を変えたら、
測り直す。上限は 990 トークンで、これは見積ではなく
`tiktoken_rs::o200k_base()` による実測で担保している。

測るテストは3本ある。`budget.rs` の
`always_on_context_stays_within_budget` が下限、`constitution.rs` の
`full_always_on_context_stays_within_budget` が実環境、同じく
`absurdly_long_cwd_cannot_push_the_assembled_system_over_budget` が
真の同時最大である。

## 報告に添えるもの

走らせたコマンドと、その実際の出力を添える。「テストは通った」だけでは
足りない。数値を書くときは、それを産んだテスト名を必ず添える。後ろ盾の
無い数字は腐る。
