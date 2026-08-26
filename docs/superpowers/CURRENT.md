# polaris 現況

最終更新 2026-08-26

## この文書の役割

セッションをまたいで残す唯一の現況記録である。新しい会話は、リポジトリを調べ直す前にここを読んで再開する。

`.superpowers/sdd/<計画名>/progress.md` とは役割が異なる。あちらは実行中の計画に閉じた台帳で、タスクごとの委譲、レビュー、裁定を時系列で記録する。Git 管理外の作業領域であり、計画が終われば消える。こちらは計画をまたいで残り、コミットする。同じ内容を両方に書かない。

## 現在地

| | |
| --- | --- |
| ブランチ | `main`（HEAD `4a43c48`）。`origin/main`は`fca96c4`(v0.6.0 CHANGELOG、47コミット分)まで同期・push済み。`v0.1.0`〜`v0.6.0`タグもorigin反映済み。それ以降のローカル2コミット(`235ec5d` プロンプトキャッシュ改善、`4a43c48` v0.7.0 CHANGELOG追加)と`v0.7.0`タグ(ローカルのみ)は**利用者の指示で意図的に未push** |
| 進行中の計画 | プロンプトキャッシュ利用率の改善。コード・CHANGELOG・`v0.7.0`タグまで完了（詳細は専用節）。A/B実測と、`Folder::take_item`が`reasoning` itemを捨てている件の扱いが未了。計画書は起こしていない。`worktree-feat-tui-live-progress`のTUIフルスクリーン化計画(9タスク)は完了・最終レビュークリア・main統合済み。worktreeをロックしていた別セッションのpidは確認できなくなった（`lsof`でcwd該当なし、2026-08-26時点）——次回`git worktree remove`を試して片付けてよい |
| 直近で終えた計画（main に統合済み・タグ済み・push済み） | `docs/superpowers/plans/2026-08-20-polaris-m4-core.md`（M4 core、全12タスク）、`docs/superpowers/plans/2026-08-24-polaris-tui-live-tool-progress.md`（ライブ表示改良）、per-directory `files.md` 自動生成計画、TUI `Viewport::Inline` 切替——以上すべて`v0.5.0`「`Regulus`」としてタグ・CHANGELOG・push済み。`docs/superpowers/plans/2026-08-21-polaris-tui.md`（v0.3.0「`Castor`」）、TUI v2・オンボーディング・codex互換サブコマンド等（v0.4.0「`Acubens`」）も同様にタグ・push済み |
| 直近で終えた計画（main に統合済み・タグ済み・push未実施） | `docs/superpowers/plans/2026-08-25-polaris-tui-fullscreen-scroll.md`（全9タスク、`v0.6.0`「`Spica`」。`Viewport::Fullscreen`への移行、履歴の自前スクロール管理、フッター固定、`with_fullscreen_picker`廃止、最終レビューで見つかった4件のImportant指摘も1回の修正waveで解消・再レビュー済み。左右カーソル移動・Ctrl+P/N入力履歴・IME preedit位置修正も同梱）。SDD台帳は完了に伴い削除済み。CHANGELOG追記・タグ付けは完了、pushのみ利用者の指示待ち |
| 仕様 | `docs/superpowers/specs/2026-08-16-polaris-harness-design.md`、`docs/superpowers/specs/2026-08-18-polaris-codex-provider-design.md`、`docs/superpowers/specs/2026-08-20-polaris-skill-bm25-router-design.md`、`docs/superpowers/specs/2026-08-21-polaris-tui-design.md`、`docs/superpowers/specs/2026-08-21-polaris-tui-v2-design.md`、`docs/superpowers/specs/2026-08-21-polaris-tui-onboarding-design.md`、`docs/superpowers/plans/2026-08-24-polaris-tui-live-tool-progress-design.md`、`docs/superpowers/specs/2026-08-25-polaris-tui-fullscreen-scroll-design.md` |
| 版の方針 | 単一の `vX.Y.Z` を単調に進める方式。2026-08-25、コードネームを「黄道十二星座を明るさ順ではなく神話的な繋がり(各星座の伝統的な星名)で単調に辿る」方式へ確定。確定した対応表: `v0.1.0`=`hamal`(おひつじ座、tag・push済み)、`v0.2.0`=`Aldebaran`(おうし座、tag・push済み)、`v0.3.0`=`Castor`(ふたご座、tag・push済み)、`v0.4.0`=`Acubens`(かに座、tag・push済み)、`v0.5.0`=`Regulus`(しし座、tag・push済み——M4 core・files.md自動生成・ライブ表示改良・Viewport::Inline化)、`v0.6.0`=`Spica`(おとめ座、tag・push済み——TUIフルスクリーン化・入力欄編集機能拡張・マウスドラッグ選択)、`v0.7.0`=`Zubenelgenubi`(てんびん座、tag済み・**push未実施**——プロンプトキャッシュ利用率改善)、`v0.8.0`=`Antares`(さそり座)、`v0.9.0`=`Rukbat`(いて座)、`v0.10.0`=`Algedi`(やぎ座)、`v0.11.0`=`Sadalmelik`(みずがめ座)、`v0.12.0`=`Alrescha`(うお座)。**`v0.7.0`分の push(main分・タグ分とも)は利用者の明示的な承認が要る、標準ルール通り** |

## プロンプトキャッシュの利用率

`235ec5d`でコミット済み(`v0.7.0`「`Zubenelgenubi`」、CHANGELOGは`4a43c48`)。`prompt_cache_key` の生成（`crates/polaris-provider/src/lib.rs`）、codex・openai 両プロバイダからの送信、`run_loop` が `cached_tokens` を合算していなかった欠落の修正（`crates/polaris-core/src/agent.rs`）、一発実行での usage 表示（`crates/polaris-cli/src/main.rs`）、および対応するテストである。

2026-08-26、この差分へ2点を加えた。

第一に、`crates/polaris-provider/src/codex.rs` の `include` を、`POLARIS_INCLUDE_REASONING` による opt-in から常時送信へ変更した。同じ差分が持ち込んだテスト `the_body_asks_for_encrypted_reasoning_so_the_turn_is_cacheable` は、環境変数を設定しない素の環境で `left: Array []` として落ちていた。コード自身のコメントが「これが無いとバックエンドは推論モデルのプロンプトキャッシュに何も書かない」と実測付きで述べている以上、既定を off に倒すと変更の目的そのものが既定経路で効かない。テストを実装に合わせるのではなく、実装をテストに合わせた。

第二に、同ファイルのテスト内にあった `needless_borrows_for_generic_args`（`Message::user(&format!(...))`）を解消し、rustfmt を通した。`cargo clippy --workspace --all-targets -- -D warnings` がこの1件で落ちていたためである。

### 判断の根拠にした実測（2026-08-26）

同一の指示「複数コードの変更をした。現状の変更点をまとめる」を polaris と codex にそれぞれ与え、消費量を突き合わせた。codex 側の数値は `~/.codex/sessions/2026/08/26/rollout-2026-08-26T12-20-44-01a03c15-9aea-7700-abcd-2c094f98e8a7.jsonl` の `total_token_usage` から取っている。

| | polaris | codex |
| --- | --- | --- |
| input（累計） | 49,446 | 274,800 |
| うち cached | 15,360（31.1%） | 214,784（78.2%） |
| output | 1,705 | 2,210 |
| プロバイダ往復 | 9回以上 | 5回 |

polaris 側は TUI の累積カウンタで、同一セッションの前ターンを含む2ターン分である。codex 側は実ユーザー発話1件のセッション全量にあたる。尺が揃っていない分は polaris に不利な方向へ効いている。

codex のリクエスト単位の内訳は、初回だけ冷えて以降が9割台で安定する形になっていた。1回目 12.6%、2回目 97.6%、3回目 77.9%、4回目 98.0%、5回目 93.6% である。polaris の 31.1% はこの形になっていない。なお codex は初回リクエストだけで input 46,566 トークンを送っており、これがツールを1つも呼ぶ前の常時コンテキストの床にあたる。polaris の同じ床は547である。

### 併せて判明したこと

`Folder::take_item` は `reasoning` item を `_ => {}` で捨てており、`Message` 型にも保持する場所が無い。一方 codex は rollout に `reasoning` item を `encrypted_content` ごと5件残していた。polaris は `include` で要求しながら受け取ったものを回収していない。

ただしキャッシュ率の主因ではない。`include` を on にするだけで2リクエスト目に 8,014 中 7,680 という実測がコード内コメントに残っている。ツール呼び出しを跨いだ推論の連続性の問題として、別に扱う。

### 検証

`cargo test --workspace` は全緑、`cargo clippy --workspace --all-targets -- -D warnings` は clean、`cargo fmt --all -- --check` も clean。いずれも 2026-08-26 に実測した。

### 未実施

修正前後の A/B 実測は行っていない。2026-08-26、コミット自体は利用者の指示(このコミットを`v0.7.0`とする)で先に進めたが、A/B実測は別枠として残っている。手順は決めてある。`235ec5d`の前(`fca96c4`)と後(`235ec5d`以降)それぞれで`POLARIS_DUMP_USAGE=1`を与えた一発実行を1回ずつ流し、リクエスト単位の `cached_tokens` と `input_tokens` を上の codex の表と同じ形に並べる。作業ツリーがコミット済みになったため、`git worktree add`等で2つのビルドを共存させるか、`git stash`せず`git checkout fca96c4 -- <該当crate>`で一時的に戻して測る必要がある。累計値だけでは応答の非決定性で揺れるため、初回が冷えて2回目以降が9割台に乗る形になるかどうかで判定する。

`Folder::take_item`が`reasoning` itemを`_ => {}`で捨てている件（下の「併せて判明したこと」）は、A/B実測とは別に、設計判断を要する変更として扱う。`Message`型に保持場所が無いため、型を広げるかどうかから決める必要があり、`spec-first-development`の対象になりうる規模。

## マイルストーン

| | 内容 | 状態 |
| --- | --- | --- |
| M1 | read ツール、1 プロバイダ、予算テスト、監査ログ、停止条件 | 完了 |
| M2 | write / edit / bash とサンドボックス | 完了 |
| M2.5 | Codex プロバイダ（ChatGPT サブスク OAuth） | 完了 |
| M3a | skills ローダと skill ツール | 完了 |
| M3b | BM25 ルータと評価コーパス | 完了 |
| M4（`v0.5.0`「`Regulus`」の一部） | subagent、`spawn`、単一波オーケストレーション（継続波は明示的にスコープ外） | 完了。tag・push済み |
| files.md自動生成・ツール呼び出しライブ表示改良（`v0.5.0`「`Regulus`」の一部） | ディレクトリ別`files.md`の決定的自動生成、`AgentEvent` 通知機構、ツール呼び出し・spawn の逐次表示、write/edit の実差分表示 | 完了。tag・push済み |
| TUIフルスクリーン化・入力欄編集拡張(`v0.6.0`「`Spica`」) | `Viewport::Fullscreen`への移行、履歴の自前スクロール管理、フッター固定表示、`with_fullscreen_picker`廃止、左右カーソル移動、Ctrl+P/N入力履歴 | 完了。tag・CHANGELOG済み、**push未実施**(利用者の指示待ち) |
| M5 | TUI、圧縮、マルチプロバイダ、セッション永続化 | TUI 部分は完了（v0.1.0〜v0.6.0すべてtag済み、v0.5.0まではpush済み）。圧縮・マルチプロバイダは未着手 |

M1・M2・M2.5 が揃い、v1.0.0 (`hamal`) の水準に達した。API キーを持たない利用者でも `polaris login` から ChatGPT のサブスクリプション認証だけで実モデルへ繋げる経路ができたことで、M1 の受け入れ基準のうち唯一無人では確認できなかった「実キーでの一発実行」を、キー無しで満たせるようになった。タグ付け自体は利用者の承認を待つ準備段階のまま。M3 以降は 1.x として積む。M3a を M2 より先に進めたのも利用者の指示による。

## M1 の進捗

全 11 タスク完了。各タスクは実装、レビュー、必要に応じた修正と再レビューを経ている。

全体レビューを最上位モデルで実施し、Critical 0、Important 8、Minor 17。判定は「修正付きでマージ可」。Important 8 件を 7 コミットで修正し、範囲を絞った再レビューで全件 ADDRESSED を確認した。そのうえで、レビュアが「出荷すべきでない」と判定した総リクエストタイムアウトと、仕様の約束をコードが守っていなかった環境ブロックの未 cap を、締めの 2 コミットで直した。

## M3a の進捗

全 5 タスク完了。

ブランチ全体の最終レビューで、ブロッキング 4 件と Minor 15 件。ブロッキングは 4 件とも同じ欠陥クラスであり、壊れたコードに対しても通ってしまうテストであった。仕様の中心的主張を裏付けるテストの不在、`discover` 本体の無検査、ツールが宣言する引数名と読み手を結ぶ経路の断絶、skipped 原因表示の空洞化である。いずれもタスク単位のレビューを 5 回すべてすり抜けていた。継ぎ目と主張は、継ぎ目としてまとめて見ないと見えない。

修正 1 回と再レビューを経たのち、受け入れ基準の中心にあたる 1 件が部分的にしか閉じていないと判定されたため、追加で閉じた。常時コンテキストの組み立てを `polaris-core` の単一関数 `assemble_always_on(constitution, environment, &[Skill])` へ集約し、その戻り値である `AlwaysOn` を私有フィールド、変更子なし、公開コンストラクタなしとした。`polaris-core` の外では、常時コンテキストへ skill 内容を混ぜる操作が検出されるのではなく、型として表現できない。`agent::run` は `&AlwaysOn` を受け取り、自身では `all_specs()` を呼ばないため、測る対象と送る対象が同一になった。

## M3b の進捗

全 4 タスク完了。仕様は `docs/superpowers/specs/2026-08-20-polaris-skill-bm25-router-design.md`。

`polaris-tools::skill::lookup` の検索経路を、クエリ全体が name か description の連続部分文字列として現れることを要求する旧実装から、BM25 ランキング（k1=1.5、b=0.75、name フィールド 3 倍重み、軽量な語幹化、9 組の一般的なソフトウェア開発用語の同義語表）へ置き換えた。あわせて、trigger 形式の description（`Use when/for/before` で始まる）かつ量化語（any/every/all）を含み固有の製品・技術名を持たない skill を、クエリの一致に関係なく検索結果へ常時追加する仕組み（`near_universal`、上限 20 件、測定コーパス 831 件中 4 件が該当）を足した。

セッション内の事前検討（本番コードには含めない、`/private/tmp/.../scratchpad/` 上の Python プロトタイプ）で、831 件の skill/plugin コーパスと 22 プロジェクトから生成した 132 件の自然文クエリに対して測定した。現行の部分文字列一致は全クエリで 0 件ヒットだったのに対し、BM25 単体で recall 74%、`near_universal` を足すと 98% まで伸びた。トークンコストの検証を先行させたところ、素朴な組み合わせ（フェーズ分類器を無制限に足す案）は平均 3,400 トークン/検索まで膨らむことが分かり不採用とした。最終的に採った「常時関連候補は少数に機械的に絞り、`lookup` の戻り値へ常時含める」方式は、セッション開始時の常時オンコンテキストには一切触れない設計とした——`docs/superpowers/CURRENT.md`（当時）の「常時コンテキストは skill 数から独立させる」という既存の決定と正面から矛盾する案（常時オンのシステムプロンプトへ焼き込む案）を検討段階で退けたためである。

Task 1（`bm25.rs`）と Task 2（`near_universal.rs`）は、まだ `lookup` へ配線されていない自己完結モジュールとして独立に実装・レビューした。両タスクとも、`cargo clippy --all-targets -- -D warnings` が「テストからの呼び出しは使用済みと数えられる」という計画側の誤った前提に反して dead_code を検出することが判明した（`--all-targets` はテストなしの素の `lib` ターゲットも並行してコンパイルするため）。`mod` 宣言 1 行への一時的な `#[allow(dead_code)]` で解決し、Task 3 の配線完了時に両方とも除去した。この知見は `~/.claude/skills/cargo-clippy-all-targets-dead-code/` へ独立に記録した。Task 2 は 1 回の修正ラウンド（上限超過テストのフィクスチャが昇順で最初から並んでいたため truncate-before-sort の欠陥を検出できなかった）を経て完了。

### 最終レビューが見つけたこと

ブランチ全体の最終レビュー（最上位モデル）は、タスク単位のレビューでは見えない統合レベルの欠陥を 4 件（Important）発見した。

- BM25 のトークナイザが ASCII のみ（`[a-z0-9]+`）のため、日本語クエリは常にヒット 0 件になっていた。仕様の非目標が明記していた「悪化させない」という約束に反する退行だった
- `near_universal` が非空なら「一致なし」の見出しが常に消え、検索が本当に何も見つけなかった場合と区別がつかなくなっていた
- `near_universal` を検索結果の末尾へ追加していたため、`list_candidates` のバイト上限（末尾から切り詰める）に、大きい description を持つコーパスで真っ先に削られてしまう——「常時含める」という保証が最も必要な場面でまさに破られる形だった
- 新しい検索経路の上限・同点順位・重複排除に対するテストが 1 本も無く、`list_candidates` の件数上限を元の `MAX_RESULTS` へ戻す変異を検出できない状態だった

1 回の修正波（9 件、上記 4 件の Important と、費用対効果の良い Minor 5 件）で全件に対応し、範囲を絞った再レビュー（最上位モデル）で全件 ADDRESSED を独立の再トレースで確認した。修正はクエリの部分文字列フォールバック（BM25 が 0 件のとき旧実装へ戻す）、3 状態の見出し分岐、`near_universal` を結合リストの先頭へ置く並び替え、上限境界を検証する新規テスト 6 本、`lookup` の公開 rustdoc の刷新から成る。再レビューが新たに見つけた 4 件の Minor（コメントの記述漏れ 2 件、`near_universal` の並び順自体を検証するテストの不在、部分文字列フォールバックが打ち切り通知を出さない——ただし既存の BM25 経路も同じ挙動）はいずれも非 load-bearing と判定し、2 回目の修正波は行わず裁定して繰り越した。裁定の全文は台帳（`.superpowers/sdd/2026-08-20-polaris-m3b-bm25-skill-router/progress.md`、消去済み。全文は本文書のこのセクションと git 履歴に残る）にある。

### 検証

HEAD `c82e910` で `cargo test --workspace` 386 件全緑（`polaris-tools` 109 件、他クレート計 277 件。この文書を書く際に自分で再実行して確認した値）、`cargo clippy --workspace --all-targets -- -D warnings` clean、`cargo fmt --all -- --check` clean、`git status --short` 空。常時コンテキストへの影響はゼロトークン——`near_universal` は `lookup` の戻り値（ツール結果、メッセージ末尾）のみに載り、`assemble_always_on` には一切触れない。この不変条件を `budget.rs` の新規テストで固定した（近傍候補 0 件・4 件・上限 20 件相当のいずれでも `AlwaysOn` のトークン数が変わらないことを確認）。

M3b の受け入れ基準にあった recall 98% という数値そのものは、Rust 側では検証していない——831 件規模のコーパス再現は仕様の「保証しない範囲」に明記した非目標であり、Rust 側の統合テストは個別ケースの回帰防止に留める。

### 大規模 skill/plugin コーパスでの codex・pi との比較、および候補プレビュー圧縮

M3b 完了後、評価に使った実コーパス（831 件、うち macOS の大文字小文字非依存ファイルシステムで 1 件衝突し実質 830 件）を codex・pi（`--model openai-codex/gpt-5.6-sol`）・polaris それぞれのネイティブな skill 探索先へ実ファイルとして設置し、同一タスクを 1 回ずつ実行して実測した。結果は `README.md`「skill/plugin を大量に含めた場合の比較」に記録した。codex は skill 数が多いとき description 抜きの圧縮カタログへ自動的に切り替わる（約 16.5 トークン/skill）が、pi にはこの種の圧縮が無く線形に増え続ける（約 131.9 トークン/skill、830 件時点で初回ターンだけで 110,692 トークン）。polaris の常時コンテキストは 830 件設置後も不変（486〜496 トークン）で、`lookup` を実際に呼ばせた場合のみ、その呼び出し 1 回につきトークンが加わる。

この計測で `lookup` 1 回あたりのコストが常時コンテキストよりずっと大きい実際のレバーであると分かったため、`list_candidates`（`crates/polaris-tools/src/skill.rs`）が候補ごとに description を全文表示していたのを、先頭文のみ（`description_preview`、200 バイト上限）へ変更した。安全性は M3b の評価用コーパスで検証した——先頭文だけに削っても、trigger 形式の近傍候補が量化語入りの判定節を保つ割合は 84 件中 83 件（98.8%）、BM25 が一致させた語がプレビューに残る割合は評価用クエリの該当ペア 41 件中 40 件（97.6%）で、削った分だけ再検索が増えるリスクは小さいと判断した。実コーパス 831 件での候補リスト全体のレンダリングコストは 43,777→20,389 トークン（約 53% 減）、実際の `lookup` 呼び出し 1 回目の増分は同じ 830 件コーパス・同じタスクでの実測で 1,234→624 トークン（約 49% 減）になった。

既存のバイト上限テスト 2 本（`search_results_are_capped_by_bytes_not_only_by_count`、`a_single_candidate_over_the_byte_cap_is_still_returned`）は、description の肥大でバイト上限を試していたが、プレビュー化で 1 件あたりの description の寄与が上限されたため、その経路では上限に到達できなくなった。name の肥大で同じ性質を試す形に書き換えた。新規テスト 5 本（`description_preview` の単体テスト 4 本、検索結果がプレビューだけを含むことを確認する統合テスト 1 本）を追加し、HEAD `1353ee5` で `cargo test --workspace` 391 件全緑を確認した。

### read の出力上限、および候補プレビューの階層化

利用者から「速度は多少犠牲にしてよいのでトークン数を限界まで削る」方針を受け、2 点を追加した。

`read`（`crates/polaris-tools/src/read.rs`）は `MAX_READ_BYTES`（5 MiB）でファイルサイズしか縛っておらず、モデルが巨大な `limit` を指定した場合の出力側は無制限だった——`CURRENT.md` の未着手項目としてすでに記録されていた穴である。`bash` の `MAX_OUTPUT_BYTES` と同じ形で `MAX_READ_OUTPUT_BYTES`（1 MiB、打ち切り通知つき）を追加した。切り詰め通知は実際に書き込んだ行数から導出するよう変更し、`limit` に由来する上限とバイト上限のどちらで止まった場合でも正しい継続位置を報告する。

利用者から「`MAX_RESULTS` と `read` の `DEFAULT_LIMIT` は維持しつつ、他の削減余地とアルゴリズム自体を再考したい」との指示を受け、`list_candidates` の候補リストを階層化した。`MAX_RESULTS`（20）自体は変えず、候補の並び順のうち先頭 `MAX_PREVIEWED_RESULTS`（8）件だけ description プレビューを残し、以降は名前のみを表示する。全件の名前は変わらず見えるため、候補が隠れるわけではない。安全性は M3b の評価用コーパスで検証した——recall した 50 件中 31 件は上位 8 件以内、5 件は 8 件目より後でも名前自体にクエリと一致する語が残っており、名前だけで一致しないのは 1 件（`description_preview` 導入時に既に許容していた `agent-browser`/「screenshot comparison page」の例と同一）だけだった。上限を 10 に広げても、この 1 件は救えないことを確認済み。実コーパス 831 件からランダムに抽出した 20 件の窓 30 個の平均で、候補リストのレンダリングコストは約 474→256 トークンへ下がる。同じ 830 件コーパス・同じタスクでの `lookup` 呼び出し 1 回目の増分は、実測で 624〜660→344 トークンまで下がった（`description_preview` 導入前の当初値 1,234 トークンから累計で約 72% 減）。

新規テスト 2 本（プレビュー上限を超えた候補が名前のみになることを確認する統合テスト、near_universal がプレビュー上限を超えて押し出されても名前が残ることを確認する統合テスト）と `read` 側の新規テスト 2 本（巨大な `limit` が出力バイト上限で打ち切られること、1 行だけでバイト上限を超える場合でも最低 1 行は返ること）を追加した。HEAD `b9db4e1` で `cargo test --workspace` 395 件全緑、`cargo clippy --workspace --all-targets -- -D warnings` clean、`cargo fmt --all -- --check` clean、`git status --short` 空を確認した。計測に使った一時的なデバッグ出力（実 API 応答の `usage` を stderr へ出す 1 行）は毎回ビルド後に元へ戻し、`cargo build --release -p polaris-cli` で計装なしのバイナリへ戻したことも確認済み。

利用者からは、常時コンテキスト本体（システムプロンプト・ツールスキーマ・`ENVIRONMENT_LIMIT`）についても削減余地を洗い出すよう指示があったが、調査の結果これ以上の削減余地は薄いと判断した。`ENVIRONMENT_LIMIT`（200）は実測 24 トークンに対する意図的な 8 倍の安全マージン（`constitution.rs` 自身のコメントに明記）であり、削るのは削減ではなく安全性との取引になる。`bash` の出力上限、`write`/`edit` の戻り値（ファイル内容ではなく短い確認メッセージのみ）はすでに安全な形になっている。

### pi と codex の比

既存の実測値（0・5・830 skill 時点のトークン数）から pi/codex の比を追加で出した。少数の skill では pi が codex の 3.9〜5.8% と大幅に軽いが、830 件では逆転して pi が codex の 249.9%（2.5 倍）になる。5〜830 件の区間（実測点の間隔が広く、0〜5 件の区間よりサンプルが安定している）で見た 1 skill あたりの増分は codex 約 15.55 トークン、pi 約 131.96 トークンで、pi の方が約 8.5 倍重い。この 2 区間の傾きを外挿すると、およそ 260 skill 付近で総量が逆転する計算になるが、5 件と 830 件の間に実測点が無く、codex の圧縮形式への切り替わりタイミングも未確認のため、これは見積もりであり実測ではない。詳細は `README.md`「pi と codex の比」に記録した。

## v0.4.0「Regulus」の進捗(TUI v2 + TUI onboarding + codex 互換サブコマンド + TUI スラッシュコマンド)

M3b 完了後、対話 TUI（新規クレート `polaris-tui`、`ratatui` + `crossterm`）を実装し `v0.3.0`(通称 `Castor`)として git tag 済み。セッション永続化(`~/.polaris/state/<project-id>/tui-session.jsonl`)、承認モーダル、端末エスケープシーケンス注入対策を含む。詳細は `CHANGELOG.md` の `[0.3.0] "Castor"` に記録済み。`Castor` は当初 M4(subagent・波・継続波)向けに予定していたコードネームだったが、対話 TUI が先に `v0.3.0` を占めたため、M4 のコードネームは `Spica` へ変更した(`docs/superpowers/specs/2026-08-16-polaris-harness-design.md` のリリース方針節を参照)。

続けて3件を実装した。

- **TUI v2**(ステータスバー、ツール呼び出しのインライン可視化、太字/コードの簡易 Markdown 整形)。`worktree-feat-tui-v2` で実装し `865e5e3` で main へマージ、最終レビュー指摘への対応も `f76fc33` で完了
- **TUI onboarding**(資格情報が一切無い状態で対話 TUI を起動すると `POLARIS_API_KEY is not set` で即終了する代わりに、ChatGPT サインイン/API キー入力の選択画面を出す)。`worktree-feat-tui-onboarding` で実装し `c24c1f7` で main へマージ。マージ直前に画面の文言・レイアウトをコードネーム未定のまま独自の英語コピー(codex の文言を複製せず、選択肢2つ・番号+上下キー選択+サブタイトル行という構造のみ踏襲)へ書き換えた
- **codex 互換サブコマンド**(`exec`・`sandbox`・`doctor`・`completion`)。利用者指示「regulusとしての実装スコープで、codexのコマンドを使えるようにする」を受け、実際の `codex` CLI(0.148.0、24 サブコマンド)を `codex <sub> --help` で調べたうえで、polaris の既存アーキテクチャ(単一の one-shot 実行経路、`polaris-sandbox` クレート)に素直に載る4件だけを移植した。`mcp`/`app`/`cloud`/`resume` 系など残り約20件は、polaris に対応する基盤が無い(MCP クライアント不在、GUI/クラウド不在)か、別途設計を要する(複数セッション管理、レビュー用プロンプト)ことを理由に対象外とし、README「サブコマンド」節に対象外の一覧と理由を明記した。新規テスト21件(ユニット3件+CLI統合18件)
- **TUI スラッシュコマンド**(`/help`・`/status`・`/clear`・`/quit`)。利用者から「入力欄で `/` を打っても予測変換が出ず、`/` 付きの入力がそのままモデルへの指示として送られてしまう」という報告を受け、実際に `codex` の対話 TUI を tmux 経由で起動して `/` の挙動を実地で観察した(バイナリ文字列の推測ではなく、実際に認証してチャット入力へ到達し、`/` を打って出るポップアップと各プレフィックスの補完結果を1つずつ記録)。codex 側は約27個のスラッシュコマンドを持つが、polaris の既存アーキテクチャで完結する4個だけを実装した——`/model`・`/mcp`・`/resume`・`/review` 等は、対話的なモデル選択 UI・MCP クライアント・複数セッション管理・レビュー用プロンプトという、polaris にまだ無い基盤を要するため見送った。新規モジュール `crates/polaris-tui/src/slash.rs`(コマンド定義・前方一致検索・パース、ユニットテスト10本)。入力欄が `/` で始まる間、`render_chat` が候補ポップアップ(コマンド名+説明、1文字ごとに絞り込み)を入力欄の直上に描画する。スラッシュコマンドは `session.messages` にもモデルへの送信にも永続化ファイルにも一切触れない——`polaris-tui::persist::clear_session`(新設、`/clear` 用)を除き既存コードへの変更は `render_chat`/`Status` への `Notice`/`suggestions` 追加のみ。実バイナリを tmux で起動し、`/` の入力→絞り込み→`/status`(ローカルで notice 表示、モデルに送らないことを「0 messages」の表示で確認)→`/quit` まで人間と同じ操作で動作確認した

続けて利用者から「slash直後ではなく、入力に応じてリアルの codex のようにサジェストを出していきたい」との指示を受け、`codex` の実際のピッカーを tmux でさらに詳しく観察したところ、`↑`/`↓` でハイライト行(太字シアン+説明が通常輝度、他の行は説明が dim)を移動でき、Enter は入力文字列そのものではなく**ハイライトされている行を直接実行する**(`/fast` をハイライトしたまま Enter を押すと "Service tier set to priority" が実行された)ことを確認した。これに合わせて `selected_suggestion: usize` を `run()` のループ状態に追加し、ポップアップが出ている間は `↑`/`↓`/Enter を `apply_key` より前に横取りして処理するようにした(型名 `slash::action_for(name)` を新設——`COMMANDS` 由来の既知の名前を直接 `Action` へ変換する)。`render_chat` は `selected_suggestion: usize` を追加引数に取り、ハイライト行を codex と同じ配色で描画する。フルネームを最後まで打ってから Enter する従来の操作も、その時点でポップアップの候補が1件(それ自身)に絞られてハイライトされているため、そのまま動く。実バイナリを tmux で再確認し、`/` → Down(ハイライトが `/help` から `/status` へ移動)→ Enter(入力欄に1文字も追加せず `/status` が実行され「0 messages」の notice が出る)まで確認した。テスト2本を追加(`action_for` が `COMMANDS` の全名前を解決できること、`parse` と整合すること)

さらに利用者から「フロントエンド TUI 部分だけ、codex をそのままにできないか」との指示を受け、`AskUserQuestion` で範囲を確認した(「orchestia」は前の会話の `codex-orchestia` との混同で、TUI の話としては無関係と判明。「そのまま」はコード移植ではなく、polaris 独自実装のまま見た目・挙動を codex へ近づける、の意)。`codex` の実際のチャット画面を tmux で観察し、枠付きヘッダーボックス(`>_ OpenAI Codex (v0.148.0)` / `model: ... /model to change` / `directory: ...`)、枠の無い `›` プロンプト+入力欄下のフッター行(`{model} default · {cwd}`)という構造を確認した。polaris 側は文言を独自にしたまま同じ構造だけを取り入れた——`HeaderInfo` に `cwd: &str` を追加(`RunArgs` にも `cwd: PathBuf` を追加し `main.rs` から配線)、ヘッダーを単一行から「polaris」タイトル付きの枠付きボックス(モデル名・プロバイダー名・累積トークンを1行にまとめた内容)へ変更、入力欄の枠を廃して `› ` プロンプト+空欄時のグレー表示プレースホルダー("Ask polaris to do anything")へ変更、入力欄の下に `{model} · {cwd}` のフッター行を追加した。既存テスト15箇所の `HeaderInfo` リテラルに `cwd` フィールドを追加。レイアウトの固定行数が 5→8 行に増えたことで揺れた既存テスト1本(`the_header_shows_provider_model_and_usage`、60列では新ヘッダー行がトークン数の手前で切れていた)は表示幅を80列へ広げて対処。実バイナリを tmux で再起動し、ヘッダーボックス・プレースホルダー・フッター・スラッシュ候補ポップアップが新レイアウトの下でも揃って正しく表示されることを確認した。codex 側で観察した「経過秒数付き "Working" 表示 + Esc での途中中断」は、polaris の現在のイベントループがブロッキング `read()` に依存しており途中終了を割り込ませる仕組みが無いため、この回では対象外とし別途の課題として繰り越した

続けて利用者から「コマンドを再現しきれていない。また `/` を打った段階ではサジェストが出るが、そこから先数文字入力した際にはサジェストが出ない」との報告を受けた。`tmux-verify-reference-cli-behavior`(このセッション中に distil したスキル)の手順で1文字ずつ送って再現を試みたところ、ライブ絞り込み自体は`/s`→`/st`→`/sta`のどの段階でも正しく動作しており、コードにバグは無かった。原因は別のところにあった——polaris の4コマンド(`help`・`status`・`clear`・`quit`)は codex の約27個に対して語彙が極端に狭く、`/model`・`/resume`・`/mcp` など polaris に無いコマンド名を打ち始めるとすぐ候補がゼロになり、「ポップアップが消えた」ように見える。「サジェストが出ない」という報告は「絞り込みが壊れている」のではなく「コマンドが足りない」ことの症状だった、という理解に至った。

これを受けてコマンドを4→8個へ拡張した。追加は `/skills`(このプロジェクトで発見された skill 一覧。`args.skills` として既に main.rs から渡っている `polaris_skills::Skill` を再利用するだけで実装できた)、`/new`(`/clear` のエイリアス——polaris は1プロジェクト1セッションの設計であり「新しいチャットを始める」と「会話を消去する」を区別する状態を持たないため)、`/init`(カレントディレクトリに `AGENTS.md` の雛形を作成。存在する場合は上書きしない)、`/logout`(`polaris_auth::logout` を呼び保存済み資格情報を削除。main.rs の `Command::Logout` サブコマンドと同じ実装を再利用)。`apply_slash_action` に `skills: &[Skill]` と `cwd: &Path` の2引数を追加(呼び出し2箇所を機械的に更新)。`/diff`・`/model`・`/mcp`・`/resume`・`/review`・`/compact` 等の残り約19個は前回同様、対応する基盤(複数行ローカル出力表示、対話的モデル選択、MCPクライアント、複数セッション管理、レビュー用プロンプト、会話圧縮)が無いため見送ったまま——README「スラッシュコマンド」節に理由を記載した。

`apply_slash_action` は今回追加した4コマンドについて初めて直接のユニットテストを持った(`/help`・`/status`・`/clear`・`/quit` は元々 tmux での手動確認のみで、`apply_slash_action` 自体の自動テストが無かった、という既存の隙間だったと判明——今回まとめて埋めた)。`/logout` だけは `polaris_auth::store::default_path()` を関数内部で直接解決しており(`main.rs` の `Command::Login`/`Command::Logout` と同じ流儀)、`HOME` 環境変数へ依存するためユニットテストからは検証しづらく、実バイナリでの tmux 確認のみに留めた。新規テスト6本(`skills` の空/非空、`init` の作成/非上書き、`clear`/`new` が session とファイルの両方を空にすること、`Unknown` が名前を含むこと)。`cargo test --workspace` は475→482件全緑(polaris-tui: 56→63)、clippy clean、fmt はこのセッションで触った範囲は全て clean(既存の無関係3件のみ残存)。実バイナリを tmux で再起動し、8個のコマンドが `/` で全件表示されること、`/new` を1文字ずつ打って `/n`→`/ne`→`/new` の各段階でライブ絞り込みが効くこと、`/init` が実際にプロジェクト直下へ `AGENTS.md` を作成することを確認した。

利用者からさらに「実装されていないコマンド(`/model`・`/usage` など)を試していた」ことが判明し(前回の「絞り込みが壊れて見える」報告の実体はこれだった)、「実装されていないコマンドについて追加したい」との指示を受けた。これを受け `/model`・`/diff`・`/review` の3件を追加し、コマンド数は8→11個になった。

- **`/model`** — 現在のモデル・プロバイダと変更方法(`POLARIS_MODEL` + 再起動)を表示するだけの読み取り専用コマンド。codex の `/model` は対話的な選択式ピッカーだが、polaris はモデルをセッション途中で切り替える経路自体を持たないため、「本来の機能」を偽って実装するより「変更方法を教える」に留めた
- **`/diff`** — `git diff` の出力を表示する。これまで local な出力の表示先が「1行の `Status::Notice`」しか無く、複数行を要する `/diff` は前回まで見送っていた。今回、`render_chat` に `local_lines: &[Line<'static>]` を新規追加し、既存の `history_lines(session)` の末尾へ結合してから既存のスクロール・オフセット計算に載せる形にした——新しいレイアウト領域は増やさず、`session.messages` にも一切触れない(モデルへは送られない、`/clear`・`/new` で一緒に消える)。出力は200行で打ち切り、その旨を明示する行を足す(既存の「切り詰めたら打ち切ったと本文で述べる」という決めごとを踏襲)。`sanitize`(制御文字の無害化)を `render.rs` から `pub(crate)` に上げて再利用した——外部コマンドの出力を経由する初めてのローカル表示なので、エスケープシーケンス注入対策を初めから外さない
- **`/review`** — 他の10個と違い「ローカルで完結する」コマンドではない。codex の `/review` は "review my current changes and find issues" に相当する固定プロンプトを実際にモデルへ送る機能であり、polaris 側もこれを再現するには session/エージェントループへ実際に投げる必要がある。`slash::Action::Review(String)`(空文字列可の追加指示)を新設し、`run()` 側で「ハイライトされた候補を Enter で確定」「フルネームを打って Enter で確定」の両経路とも、`apply_slash_action` を呼ぶ前に横取りして `review_prompt(extra)` へ展開し、通常の `session.push_user(&text)` 以降の流れ(モデルへの送信・永続化・agent::run())へそのまま合流させる形にした。これに伴い、`Action::Review` を含む一部コマンドが今後「実引数」を持つ可能性を見込み、`parse()` を「先頭の空白区切り1語だけをコマンド名として照合し、残りは引数」という形へ変更した(`/status now` が `Unknown` にならず `Status` として通るようになる副次効果も生んだ)

新規テスト9本(`/model` の表示内容、`/diff` のGitリポジトリ外/クリーンなリポジトリ/実際の変更ありの3パターン、`/clear` が `local_lines` も一緒に空にすること、`review_prompt` の素の指示文/追加指示付きの2パターン、`parse` が `/review <追加指示>` を正しく分離すること、他コマンドが引数付きでも `Unknown` にならないこと)。`cargo test --workspace` は482→491件全緑(polaris-tui: 63→72)、clippy は `render_chat` の引数が8個になり `too_many_arguments` に新たに抵触したため `#[allow(clippy::too_many_arguments)]` を追加(`apply_slash_action` と同じ扱い)、fmt はこのセッションで触った範囲は全て clean(既存の無関係3件のみ残存)。実バイナリを tmux で再起動し、`/` で11個全件が表示されること、`/diff` が実際のpolarisリポジトリの実差分(2120行、200行に打ち切り)を履歴領域に表示すること、`/model` が表示されること、`/review` が実際にモデルへの送信(`thinking...`表示)まで到達することを確認した。

続けて利用者から「codex、orchestia の TUI をより深く調査し、UI をなるべく反映させられるようにする」との指示を受けた。まず `orchestia` コマンドを調査したところ `/Users/kn/.local/bin/orchestia` は実体として `codex` バイナリそのもの(`codex --help` と出力が同一)であり、独自の TUI を持たないことを確認した——先の会話で確定していた「orchestia は TUI の話としては無関係」という理解を裏付けた。改めて `codex` の実チャット画面を tmux で fake API key を使い最後まで(サインイン→ディレクトリ信頼確認→メイン画面)遷移させ、`-e` 付きで ANSI 情報ごと観察した。前回把握していた「枠付きヘッダーボックス+枠無し入力欄+フッター行」という構造に加え、今回さらに次を確認した——(1) ヘッダーボックスは太字シアンの `>_` アイコン+太字タイトル(`OpenAI Codex (v0.148.0)`、バージョンは dim)を1行目に置き、枠線自体が dim スタイルで描画されている、(2) `model:`/`directory:` の各ラベルは同じ文字幅にパディングされ値が縦に揃っている、(3) フッターは一様なグレーではなく、モデル名が暖色(`rgb(246,226,183)`)・ディレクトリが淡い緑(`rgb(171,223,167)`)の2色構成で、中黒(`·`)は dim、(4) 入力欄のプロンプト `›` は太字+dim の同時適用、(5) ターン送信後は「thinking」の代わりに文字ごとに輝度が変化するグラデーション付き `Working` 表示+`(経過秒 · esc to interrupt)` が出る——これは前回同様、polaris の現在のブロッキング `read()` イベントループでは実現できないため今回も対象外のまま繰り越した。

(1)〜(4)を反映し、`render.rs` のヘッダー/フッターを再設計した。ヘッダーボックスの内容を単一行から codex と同型の5行構成(`›_ polaris` 太字シアン見出し行、空行、`model:` 行、`directory:` 行、`tokens:` 行——ラベル部分をすべて11文字幅パディングして縦に揃え、枠線に `Modifier::DIM` を付与)へ変更し、レイアウトの `header_area` を `Constraint::Length(3)` から `Length(7)` へ拡張した。フッターは `Line`+複数 `Span` 構成へ変更し、モデル名を `Color::Rgb(246,226,183)`、ディレクトリを `Color::Rgb(171,223,167)`、中黒区切りを dim スタイルにした。入力欄のプロンプト `› ` に `Modifier::DIM` を追加(既存の `BOLD` と併用)。ヘッダーが3→7行に伸びたことで表示幅の狭い既存テストの多く(`TestBackend::new(60, 10)` 等)で履歴領域が0行になり失敗したため、影響を受けたテストのバックエンド高さを機械的に引き上げて対処した(新規テストは追加せず、既存71件がそのまま緑のまま通る形に収めた)。`cargo test --workspace` 491件全緑、clippy clean、fmt はこのセッションで触った範囲は全て clean(既存の無関係3件のみ残存)。実バイナリを tmux で再起動し、ヘッダーボックス(dim枠+太字見出し+ラベル揃え)・フッターの2色配色・`/` ポップアップが新レイアウトの下でも正しく表示されることを確認した。

続けて利用者から「現在、自動で一つの対話が開いている。そうではなく、どこのディレクトリにいても無で起動し、`/resume`で対話を読み取れるようにする。対話一覧は対象ディレクトリごとにジャンル分けする」との指示を受けた。指示文の「`~/.codex`の中に会話は全て保存し」という一文が本物の `codex` の実ディレクトリを指すのか polaris 独自のディレクトリを指すのか曖昧だったため、`AskUserQuestion` で確認し「`~/.polaris`(推奨)」を選んでもらった——本物の `codex` のデータには一切触れない設計に確定した。

これは「1プロジェクト1セッション、常に同じファイルへ追記・`/clear`で上書き」という既存の永続化モデル自体を変える必要のある変更だった。新しい設計は次の通り。

- **保存先をプロジェクトごとのハッシュ化ディレクトリから、全プロジェクト共有の1箇所へ移した**——`polaris_core::project::sessions_dir()`(新設)が `~/.polaris/sessions/` を返す。既存の `state_dir()`(監査ログ・サンドボックス用ヘルパーのステージング領域)は変更なしでプロジェクトごとのまま残した——チャット会話の保存先だけを分離した
- **1会話=1ファイルペアへ変更**——`<会話id>.jsonl`(メッセージ本体、既存の `persist::load_session`/`append_message` をそのまま再利用)+ `<会話id>.meta.json`(起動元ディレクトリと開始時刻。`persist::SessionMeta`・`write_meta_if_absent`・`read_meta` を新設。`serde_json::Value` ではなく型付き構造体にするため `serde`(derive機能込み、ワークスペース既存の依存)を `polaris-tui` の直接依存に追加)。メタ書き込みは最初のメッセージを実際に送るまで遅延される(`write_meta_if_absent` は2回目以降呼んでも無視される単なる存在チェック)——起動しただけで何も送らずに終了した空セッションが `/resume` の一覧を汚さないようにするため
- **起動時は常に空の `Session::default()`**——旧来の「起動時に `state_dir` の1ファイルを自動読込」を削除した。会話idは `{13桁ゼロ埋めミリ秒}-{プロセスID}-{カウンタ}`(`new_session_id()`、ファイル名としてソート可能)で、起動のたびに新規発行される
- **`/resume`(新規スラッシュコマンド)**——フルスクリーンのピッカーを開く。`sessions::list_sessions`(新規モジュール)が `~/.polaris/sessions/` を走査し、`.meta.json` はあるが対応する `.jsonl` が空(=送信せず終了したセッション)のものは除外する。`sessions::grouped` が起動元ディレクトリでグループ化し、今いるディレクトリのグループを常に先頭へ、それ以外は「そのディレクトリで最後に会話した日時」の新しい順に並べる(各グループ内は新しい会話が先頭)。日時表示用に依存追加無しの UTC タイムスタンプ整形(`time.rs` 新設、Howard Hinnant の `civil_from_days` を移植)を書いた。ピッカー自体(`render::render_resume_picker`)は `/` ポップアップと同じ操作感(`↑`/`↓`で移動、Enterで確定、Escでキャンセル)。既存の `approver.rs` が承認モーダル用に持っていた `KeyReader` トレイト抽象化(実端末無しでテストできるようにする仕組み)を再利用し、`run_resume_picker`/`handle_resume` を `Backend`/`KeyReader` についてジェネリックにしたことで、`ScriptedReader`(キー入力を事前に仕込んだテスト用リーダー)を使ったユニットテストで実際にピッカーを駆動する挙動まで検証できた
- **`/new` の意味を変更**——旧来は `/clear` の別名(同じファイルをその場で空にする、元に戻せない)だったが、複数会話を持てるようになったことで「消す」と「新しく始める」を区別できるようになったため、`/new` は今の会話をファイルごと残したまま新しい会話id・ファイルへ切り替える動作にした(`handle_new_session`、新設)。`/clear` は変更なし(その場で空にする、`apply_slash_action` に残ったまま)。`New`/`Resume` はどちらも `run()` 側でディスパッチ前に横取りされる(`Review` と同じ扱い)ため、`apply_slash_action` 内の対応する match アームは「本来ここには来ないはずの防御的フォールバック」になった(元々 `Review` 用にあった同種のアームへ統合)

`main.rs` に `sessions_dir` を新規解決して `RunArgs` へ配線した。新規テスト23本(`project::sessions_dir` の1本、`persist` のメタ読み書き4本、`sessions::list_sessions`/`grouped` の8本、`render::render_resume_picker` の2本、`time::format_unix_millis` の3本、`lib.rs` の `handle_resume`/`handle_new_session` まわり5本)。`cargo test --workspace` 513件全緑(polaris-tui: 72→93、polaris-core: 89→90)、clippy clean(`sessions.rs` の `sort_by` を `sort_by_key` へ直す1件のみ指摘があり対応)、fmt はこのセッションで触った範囲は全て clean(既存の無関係2件のみ残存)、`docs/filemap.md` を新規ファイル(`sessions.rs`・`time.rs`)ぶん再生成した。実バイナリを tmux で2つの偽プロジェクトディレクトリ(`proj-a`・`proj-b`)を使って検証した——`proj-a` で起動すると常に空の状態から始まること、メッセージ送信で `~/.polaris/sessions/` にファイルペアが実際に作られること、`proj-b` で別の会話を送った後 `proj-a` へ戻って `/resume` を開くと `proj-a` のグループが先頭(自身の会話がハイライト済み)、`proj-b` のグループがその下という指示通りの並びで表示されること、Enter で選ぶと実際に過去の会話が読み込まれ履歴領域に表示されることを確認した。
- 前回のプロバイダ解決バグ修正(`default_provider_name`)も同じ作業ツリーにまとめて乗っている

**main へはまだマージしていない——変更一式が作業ツリーに未コミットのまま。**

この4件をまとめて `v0.4.0`「`Regulus`」として出荷する方針にした(利用者指示、本節執筆時点)。onboarding のプランは元々「バージョンアップの対象ではない」と明記していたが、この方針転換でその前提を上書きしている。**`Regulus` の範囲(この4件で締めるか、さらに何か積むか)がまだ確定していないため、タグ付け・`CHANGELOG.md` への `[0.4.0]` エントリ追加・`git push` のいずれもまだ行っていない。**

続けて利用者から「codexの実コードをgithubから読み込み、tui部分を完全に揃えたい」との指示を受けた。「orchestia実装時にはできていた」という言及があり、実際に `gh repo view openai/codex` で確認のうえ `git clone --depth 1 https://github.com/openai/codex.git`(スクラッチパッド配下、読み取り専用)して `codex-rs/tui` を実地で調査した。結果、`codex-rs/tui` は492ファイル・約27万行(`bottom_pane` だけで5.8万行、`chatwidget` だけで5.7万行)——MCP・複数エージェントピッカー(`agent_picker.rs`・`agents_overview.rs`)・クラウドタスク・プラグイン・IDE連携・画像表示・ペット・キーマップエディタまで含む、フル機能のマルチエージェントIDE級TUIだと判明した。文字通りの「完全一致」はpolaris自体の設計方針(最小コンテキストの単一エージェント)と矛盾しかつ非現実的と判断し、`AskUserQuestion` で範囲を確認した——「対象を絞って本物のソースで裏取り」(polarisが既に持つ画面要素に限定し、tmuxでの推測ではなく実ソースで検証する)を選んでもらい、「バージョンは次へ進める」は「進めない方針にする」(手動入力)という回答を得た。あわせて `status_indicator_widget.rs` を読んだところ、codexのspinner/経過秒/esc中断は `FrameRequester` による非同期(tokio)前提の再描画に依存しており、polaris-tuiの現状のブロッキング同期ループでは実現不可能——「見た目を似せる」以前にイベントループ自体の作り直しが要る、という根本的な前提を確認した。3段階の計画(Phase 0: 非同期イベントループ化 / Phase 1: shimmer・経過秒・esc中断 / Phase 2: `/resume` の表記調整)を提示し、承認を得て実装した。

- **Phase 0(非同期イベントループ化)**: `crossterm` を(`ratatui` 経由の再エクスポートだけでなく)`event-stream` フィーチャー付きで `polaris-tui` の直接依存に追加(ワークスペースの `Cargo.toml` にも新規追加)。`terminal: ratatui::DefaultTerminal` を `RefCell<ratatui::DefaultTerminal>` へ変更——ターン実行中に `agent::run` と再描画を並行させる必要があり、かつ承認モーダル(`TuiApprover`)も同じターミナルへ同期的に描画するため、単純な `&mut` の排他所有では両立できない(`approver.rs` の `TuiApprover::terminal` フィールドも `&RefCell<Terminal<B>>` へ変更。単一タスクの協調的実行なので実行時のエイリアシングは発生しないことをコメントで明記)。ターン本体は `tokio::pin!(agent_future)` した `agent::run(...)` を `tokio::select!` で 100ms tick(`tokio::time::interval`)・`crossterm::event::EventStream`(Escキー検知用)と競わせる形に書き換えた。`agent_future` は `&mut session` を握ったままなので、tick時の再描画は生の `session` を読めない(借用が衝突する)——ターン開始直前に `session.clone()` した `render_snapshot` を再描画専用に使う設計にした(`Session` に `Clone` を新規導出。副作用として、ターン中のツール呼び出しは今回もライブ表示されず、ターン完了後にまとめて表示されるまま——このスコープではステータス行のアニメーションだけを対象にしたため)。Escを押すと `agent_future` を(pin先の変数ごと)スコープを抜けてドロップすることで実際にキャンセルされる(Rustの非同期は「誰もpollしなくなった瞬間に進行が止まる」ため、ドロップ=キャンセルとして機能する。ただしツールが起動済みのOSサブプロセスまでは殺さない、「待つのをやめる」だけの中断であることをコメントで明記)。中断後は失敗時と同じロールバック(`session.messages.truncate(checkpoint)`)を行い `Status::Notice("interrupted")` を表示する
- **Phase 1(shimmer・経過秒・esc中断の表示)**: `render::Status::Thinking` を単位無しから `Thinking { elapsed: Duration }` へ変更。`shimmer.rs` を実際に読み、そのアルゴリズム(プロセス開始起点ではなくターン開始からの経過秒による2秒周期スイープ、半値幅5文字のコサイン型グラデーション)を `render.rs` に移植した(`render.rs` は元々「時計を読まない、呼び出し元から時間をもらう」という既存方針(`shimmer_spans` は `Instant` ではなく `elapsed: Duration` を引数に取る)を踏襲。色はcodexの実端末色検出(`terminal_palette`)までは移植せず、固定のグレー→白のRGBブレンドで近似した——「対象を絞る」の範囲内と判断)。ステータス行は `"Working"`(shimmer)+`" (Ns · esc to interrupt)"`(dim)という構成にした
- **Phase 2(`/resume` の表記調整)**: `time.rs` に `format_relative(then_millis, now_millis) -> String` を追加(`"42s ago"`/`"35m ago"`/`"2h ago"`/`"3d ago"`、30日超は既存の絶対表記へfallback)——codexの実物 `/resume` ピッカー(`resume_picker.rs` とそのテストスナップショット)を読み、相対時刻表記であることを確認したうえで移植した。ピッカーの選択マーカーを `›`(U+203A)から `❯`(U+276F、codexの実際のグリフと同一)へ変更し、画面下部にヒントバー `"enter to resume · esc to cancel"` を追加した(codexは `"enter to resume · esc to start new · ctrl + c to quit · tab to toggle sort"` だが、polarisには対応する機能が無い部分は削って2項目のみに絞った)。`render_resume_picker` は `now_millis: u128` を呼び出し元(`lib.rs`)から受け取る形に変更(`render.rs` は時計を読まない既存方針を踏襲)

新規テスト12本(`time::format_relative` 4本、`render::shimmer_spans` 2本、`Status::Thinking` の表示内容1本、`/resume` ピッカーの新表記(相対時刻・ヒントバー)1本、他は既存テストの更新分)。`cargo test --workspace` 515→524件全緑(polaris-tui: 93→98)、clippy clean、fmtはこのセッションで触った範囲は全てclean(既存の無関係1ファイル・2箇所のみ残存)。実バイナリをtmuxで検証した——(1) 偽の到達不能ホスト(`POLARIS_BASE_URL=http://10.255.255.1:1`)へ向けてターン送信し、shimmerの輝度グラデーションが文字ごとに実際に動いていること(`Working` の各文字が異なるRGB値で描画される)、経過秒表示が `0s`→`1s` と実際にカウントアップすること、Escキーで即座に `"interrupted"` へ遷移しその後も `/status` 等が正常に動作する(イベントループがハングしない)ことを確認、(2) `~/.polaris/sessions/` に手でセッションを1件仕込んだ状態で `/resume` を開き、`43s ago`(相対時刻)・`❯` マーカー・`enter to resume · esc to cancel`(ヒントバー)が実際に表示されEnterで正しく再開できることを確認した。

**「バージョンは進めない方針にする」との利用者指示により、この作業を含めてもタグ付け・`CHANGELOG.md` への `[0.4.0]` エントリ追加・`git push` のいずれも行っていない。**

続けて利用者から「ui部分はかなり模倣されている。ただ、/コマンドが実装しきれていない。デスクトップの画像も参照しつつ、tui部分のコードを読み込み、これらの模倣についてさらに検討する」との指示を受けた。デスクトップ上の4枚のスクリーンショット(`model選択.png`・`skill1.png`・`skill2.png`・`通常時.png`)を確認したところ、次が判明した——(1) `/model`は実際には対話的ピッカー(番号付き選択肢+太字シアンハイライト+`(current)`表示+`↑`/`↓`+Enter確定/Esc戻る)であり、polarisの現状(読み取り専用の1行表示)とは踏み込み方が違う、(2) フッターは`gpt-5.6-sol high · ~/File/projects/...`——モデル名の後ろにreasoning effortが入り、パスは`~`で短縮表示されている(polarisは絶対パスのまま)、(3) `/skills`も対話的ピッカー、`@`メンションという`/`コマンドとは別の入力トリガーが存在する。あわせて `codex-rs/tui/src/slash_command.rs` を実際に読み、canonicalなコマンド定義(`SlashCommand` enum)を確認したところ、codexは実際には約65個のスラッシュコマンドを持っていた(前回tmuxで見えていたのは氷山の一角)。全てを実装対象から除外理由つきで仕分けたうえで、`AskUserQuestion` で2点確認した——低・中コストな追加候補(`/pwd`・`exit`エイリアス・`/export`・`/permissions`ピッカー・`/fork`、およびフッターの`~`短縮)は「全て実装する」、`/model`の真の対話切替(プロバイダをセッション中に作り直すアーキテクチャ変更が要る規模)は「今回は見送り」を選んでもらった。

- **`/pwd`**: 現在の作業ディレクトリを絶対パスで表示するだけの読み取り専用コマンド
- **`exit`エイリアス**: `/quit`と同じ`Action::Quit`へ解決されるよう`parse()`に追加(`/q`と同様、`COMMANDS`一覧には載せない裏コマンド)
- **`/export`**: 会話をmarkdownとしてファイルへ書き出す。`export_markdown(session)`(新設)が`## You`/`## polaris`/`## tool result`の見出し+本文というシンプルな構成で組み立てる。宛先は`/export notes.md`のように指定でき、省略時は`polaris-export-<ミリ秒タイムスタンプ>.md`をカレントディレクトリに書く
- **`/permissions`**: `ApprovalPolicy`(Never/OnRequest/Always)を対話的に選び直すピッカー。デスクトップの`model選択.png`で確認した「番号+太字シアンハイライト+`(current)`表示」というレイアウトをそのまま`render_permissions_picker`(新設)に適用した——`/resume`の`❯`マーカー・ヒントバー規約とも揃えてある。`New`/`Resume`/`Fork`と同じ理由で`run()`側の2箇所で横取りされる(`apply_slash_action`はここでも防御的フォールバックのみ)。選んだポリシーは`args.approval_policy`を直接ではなく、`run()`内のローカル`let mut approval_policy`(新設)に反映される——`args`自体を可変にせずに済むようにするため
- **`/fork`**: 今の会話を新しいid/ファイルへ複製する。`/new`と実装の骨格は同じだが、空で始まる`/new`と違い、複製元の全メッセージをその場で(遅延書き込みではなく即座に)新ファイルへ書く——複製直後に何も送らないまま終了しても`/resume`の一覧から消えないようにするため
- **フッターの`~`短縮**: `abbreviate_home(&args.cwd)`(新設、`$HOME`読み取りのため`lib.rs`側に置き、`render.rs`の「時計もHOME環境変数も読まない」という既存の純粋性方針は保った)を`HeaderInfo`の新フィールド`cwd_short`として渡し、フッターだけに使う。ヘッダーボックスの`directory:`行は絶対パスのまま据え置いた(codexが header 側も短縮しているという根拠は無かったため)

新規テスト16本(`slash.rs`側の`/export`・`/permissions`・`/fork`・`/pwd`・`exit`パース6本、`render.rs`側の`render_permissions_picker`表示2本、`lib.rs`側の`/pwd`・`/export`(2パターン)・`handle_fork`・`handle_permissions`(2パターン)・`abbreviate_home`(2パターン)8本)。`cargo test --workspace` 524→535件全緑(polaris-tui: 98→109)、clippy は`abbreviate_home`内の`redundant_guards`指摘1件のみ(`Some(rest) if rest.is_empty()`を`Some("")`へ修正して対応)、fmtはこのセッションで触った範囲は全てclean(既存の無関係1ファイル・2箇所のみ残存)。実バイナリをtmuxで検証した——`$HOME`配下のディレクトリで起動しフッターが`~/projects/demo`と短縮表示されること、`/permissions`でDown→Enterにより実際に`Always`へ切り替わり通知が出ること、`/pwd`が絶対パスを表示すること、`/export`が実際にmarkdownファイルを書き出し中身が`## You`/本文を含むこと、`/fork`が新しい会話ファイルを作り元のファイルはそのまま残ることを確認した。

**「バージョンは進めない方針にする」という前回からの指示は継続しており、この作業を含めてもタグ付け・`CHANGELOG.md` への `[0.4.0]` エントリ追加・`git push` のいずれも行っていない。**

続けて利用者から「modelやskillsのコマンドについても実装したい。加えて、一番先頭にある>_のロゴについて変更あんを考えたい」との指示を受けた。前回「`/model`の真の対話切替は別途アーキテクチャ検討が要る規模」として見送っていたが、明示的な再指示のため実装した。

- **アーキテクチャ変更**: `polaris_provider::Provider` トレイトに `fn set_model(&self, model: &str)`(デフォルト実装は no-op)を追加。`&mut self` ではなく `&self` にしたのが要——これにより `RunArgs.provider: &'a dyn Provider` という既存の「共有参照」の形のまま、内部可変性(`OpenAiProvider`/`CodexProvider` の `model` フィールドをそれぞれ `String` から `std::sync::RwLock<String>` へ変更)でモデル切り替えを実現した。`RunArgs` がプロバイダを所有し直す(`Box<dyn Provider>` 化する)という前回懸念していた規模の変更は不要だった——テスト用の `Canned`/`Scripted` プロバイダはデフォルト実装のままで無改修
- **`/model`**: `render::MODEL_CATALOG`(新設、固定6件——デスクトップの `model選択.png` に写っていたモデル名からreasoning effortの次元とニッチな `gpt-daybreak-blue-latest` を除いたもの)を対象にした対話的ピッカー。`New`/`Resume`/`Permissions`/`Fork` と同じ理由で `run()` 側の2箇所で横取りされる。選択すると `provider.set_model(picked)` で実際の送信先を切り替え、`run()` 内のローカル `let mut model_name`(新設、ヘッダー/フッター/通知の表示用)も同時に更新する
- **`/skills`**: 一覧を「6件+"+N more"に切り詰めた1行 `Status::Notice`」から、発見された全skillを表示するフルスクリーンピッカーへ変更(`render_skills_picker`、新設)。codexの`/skills`が持つ「有効/無効切り替え」機能はpolarisに対応する状態が無いため、Enter/Esc双方が単に閉じるだけの閲覧専用にした
- **`>_`ロゴの変更**: ヘッダー1行目のアイコンを、codexの実際の`>_`(端末プロンプト)をそのまま流用していた`›_`から、`polaris`(北極星)という名前にちなんだ金色の`✦`へ変更した。`AskUserQuestion`で4案(現状維持/星単体/星+プロンプトの複合/アイコン無し)をASCIIモックアップ付きで提示し、「北極星モチーフ(✦)」を選んでもらった。色は既存のシアン(対話可能なヒント・ピッカーのハイライトに使う意味を持つ色)と衝突しないよう、固定の識別マークとして暖色の金(`Rgb(250, 204, 21)`)にした

新規テスト9本(`polaris-provider`側の`set_model`実HTTPリクエスト検証1本、`polaris-tui`側の`render_model_picker`/`render_skills_picker`表示4本、`handle_model`(切替/キャンセル)2本、`run_skills_picker`(空/一覧+クローズ)2本)。`cargo test --workspace` 535→540件全緑(polaris-provider: 57→58、polaris-tui: 109→113)、clippy clean、fmtはこのセッションで触った範囲は全てclean(既存の無関係1ファイル・2箇所のみ残存)。実バイナリをtmuxで検証した——モック用HTTPサーバー(Python)を127.0.0.1:8991に立て`POLARIS_BASE_URL`で向け、`/model`で`gpt-5.6-sol`を選択後に実際にメッセージを送信し、サーバー側が受け取ったリクエストbodyの`model`フィールドが`gpt-5.6-sol`に切り替わっていることを確認、`/skills`のフルスクリーン表示とEnterでの正常なクローズを確認、新ロゴ`✦ polaris`(金色)がヘッダーに正しく描画されることを確認した。

**「バージョンは進めない方針にする」は継続しており、この作業を含めてもタグ付け・`CHANGELOG.md` への `[0.4.0]` エントリ追加・`git push` のいずれも行っていない。**

続けて利用者から「モデル選択について。modelを選んだ後(enterした後)、次でeffortを表示する段階を用意。desktopに配置している2枚の画像を参照。なお、max,ultraだけ別ページに分けることはせずまとめて表示する」との指示を受けた。デスクトップに新たに置かれた2枚のスクリーンショット(`Screenshot 2026-08-24 at 9.34.48.png`・`9.35.00.png`)を確認したところ、codexの実際の`/model`は2段階ウィザードだった——1画面目でモデルを選ぶと、2画面目「Select Reasoning Level for {model}」でreasoning effortを選ばせる。codex側はこの2画面目をさらに「Low/Medium/High/Extra high + More reasoning...」の主画面と、そこから開く「Advanced Reasoning」副画面(Max/Ultra、"⚠ Consumes usage limits faster"の警告付き)の2つに分けているが、今回の指示は明示的に「まとめて1つに表示する」だった。

- **`polaris_provider::Provider`にeffort対応を追加**: `fn set_effort(&self, effort: Option<&str>)`(デフォルトno-op)を新設。`OpenAiProvider`は`effort: RwLock<Option<String>>`を追加し、`Some`のとき`reasoning_effort`フィールドをbodyに足す(`None`で外す)。`CodexProvider`は元々`Token::effort`(ChatGPTのプラン種別から自動決定される値)を`reasoning.effort`として送っていたが、新設した`effort_override: RwLock<Option<String>>`が設定されていればそちらを優先する(`token.effort`にフォールバック)よう`attempt()`を変更した
- **`render::EFFORT_CATALOG`**(新設、`(名前, 説明)`のタプル6件固定)を`low`→`ultra`まで1つのフラットな配列として定義し、`render_effort_picker`で番号付きピッカーとして描画。`DEFAULT_EFFORT`(=`"low"`)は`EFFORT_CATALOG[0].0`から導出、`(default)`ラベルは常にlow行に付き、現在選択中のeffortには別途`(current)`が付く(同じ行なら`(default, current)`とまとめる——2枚目のスクリーンショットで"Low (default)"と"High (current)"が別々のラベルだったことを踏まえた実装)
- **`handle_model`を2段階ウィザードへ再設計**: `run_model_picker`(既存)→`run_effort_picker`(新設)の順に呼び、両方confirmされて初めて`provider.set_model`/`set_effort`と表示用の`model_name`/`effort_name`(いずれも`run()`のローカル状態、新設)を更新する。Escの意味を画面ごとに変えた——2画面目でのEscは1画面目(モデル一覧)へ戻るだけ(ウィザード全体はキャンセルしない)、1画面目でのEscはウィザード全体をキャンセルする。これはピッカー自身が表示する"esc to go back"というヒント文言と整合させた設計
- **フッター表示の拡張**: `render::HeaderInfo`に`effort_name`フィールドを追加し、フッターを`{model} {effort} · {cwd}`(スクリーンショットで確認した実際の表記`gpt-5.6-sol high · ~/File/projects/...`と同型)に変更した

新規テスト9本(`polaris-provider`側の`set_effort`実HTTPリクエスト検証2本(`OpenAiProvider`/`CodexProvider`各1本)、`polaris-tui`側の`render_effort_picker`表示3本、`handle_model`の2段階ウィザード動作(通し/1画面目でキャンセル/2画面目Escで1画面目へ戻る)3本、フッター表示1本)。`cargo test --workspace` 540→544件全緑(polaris-provider: 58→60、polaris-tui: 113→118)、clippy clean、fmtはこのセッションで触った範囲は全てclean(既存の無関係1ファイル・2箇所のみ残存)。実バイナリをtmuxで検証した——モック用HTTPサーバーを127.0.0.1:8992に立て、`/model`でモデル選択後にeffort画面が開き6段階全てが1画面に表示されること、`max`を選んで実際にメッセージを送信すると`POLARIS_BASE_URL`宛のリクエストbodyの`reasoning_effort`が`max`になっていること、フッターが`gpt-5.4 max · ...`と表示されること、2画面目でEscを押すと1画面目のモデル一覧へ戻り、そこでさらにEscを押すと元の`max`設定を保ったままウィザード全体がキャンセルされることを確認した。

**「バージョンは進めない方針にする」は継続しており、この作業を含めてもタグ付け・`CHANGELOG.md` への `[0.4.0]` エントリ追加・`git push` のいずれも行っていない。**

続けて利用者から「5.6sol, extra highを選んだところerror: provider: HTTP error: status 400 Bad Request: { "error": { "message": "Invalid value: 'extra high'. Supporte」となったため修正する」との実際のバグ報告を受けた。原因は単純だった——`render::EFFORT_CATALOG`の表示名(`"extra high"`、空白入り)をそのまま`provider.set_effort()`経由でAPIへ送っていたため、空白を含むリテラル文字列がそのままreasoning effortの値として送信され、実サーバーに拒否されていた。

根拠を探したところ、`polaris-auth`の既存コード(`effort_for_plan_type`、このセッションより前から存在)が既に「`"low"`/`"xhigh"`という空白なしのトークンだけが実際にAPIへ送る値として正しい」ことの一次証拠になっていた(ChatGPTのプラン種別からreasoning effortを自動決定するロジックで、plus→`"low"`、pro系→`"xhigh"`と、既にこの2つの具体的な文字列を使っていた)。これを踏まえ、`render::effort_wire_value(name: &str) -> &str`(新設)を追加し、ピッカーの表示名(UI用・`(current)`判定用)と実際にAPIへ送る値を分離した——`"low"`/`"medium"`/`"high"`はそのまま、`"extra high"`は`"xhigh"`へ、`"max"`/`"ultra"`(ChatGPTアプリ側だけの上位プラン向け表示で、API側にリテラルな受理値が存在するという根拠がどこにも無い)も同じく`"xhigh"`(確認できている最高値)へ変換する。`handle_model`側は`provider.set_effort(Some(render::effort_wire_value(picked_effort)))`という形で呼び出し、`effort_name`(footer/通知表示用)は引き続き表示名のまま保持する——実際に送る値だけが変わる設計にした。

新規テスト6本(`render.rs`側の`effort_wire_value`3本——全エントリが空白を含まないことの網羅チェック、既に安全な値がそのまま通ること、`extra high`/`max`/`ultra`が全て`xhigh`へ変換されること、`lib.rs`側の`handle_model`が実際に表示名でなく変換後の値をproviderへ渡すことを確認する回帰テスト1本)。`cargo test --workspace` 544→548件全緑(polaris-tui: 118→122)、clippy clean、fmtはこのセッションで触った範囲は全てclean(既存の無関係1ファイル・2箇所のみ残存)。実バイナリをtmuxで検証した——モック用HTTPサーバー(空白を含むeffort値を受け取ると実際に400を返すよう再現)を127.0.0.1:8993に立て、`/model`→`extra high`選択→メッセージ送信で以前は再現していた400が発生せず、サーバー側が受け取った`reasoning_effort`が`'xhigh'`(空白なし)になっていることを確認した。

**「バージョンは進めない方針にする」は継続しており、この作業を含めてもタグ付け・`CHANGELOG.md` への `[0.4.0]` エントリ追加・`git push` のいずれも行っていない。**

本節執筆時点での作業ツリーの状態: `cargo test --workspace` 548件全緑、`cargo clippy --workspace --all-targets -- -D warnings` clean、`cargo fmt --all -- --check` は無関係な既存1ファイル(`polaris-provider/src/openai.rs`、diff2箇所)のみ残存。未コミットの変更は `CHANGELOG.md`・`Cargo.toml`(ワークスペース)・`Cargo.lock`・`README.md`・`crates/polaris-cli/Cargo.toml`・`crates/polaris-cli/src/main.rs`・`crates/polaris-cli/tests/subcommands.rs`・`crates/polaris-core/src/project.rs`・`crates/polaris-core/src/session.rs`・`crates/polaris-provider/src/codex.rs`・`crates/polaris-provider/src/lib.rs`・`crates/polaris-provider/src/openai.rs`・`crates/polaris-tui/Cargo.toml`・`crates/polaris-tui/src/approver.rs`・`crates/polaris-tui/src/lib.rs`・`crates/polaris-tui/src/persist.rs`・`crates/polaris-tui/src/render.rs`・`crates/polaris-tui/src/time.rs`・新規 `crates/polaris-tui/src/slash.rs`・新規 `crates/polaris-tui/src/sessions.rs`・`docs/filemap.md`・`docs/superpowers/CURRENT.md`・TUI v2/harness-design 関連のドキュメント3件、および「Castor/Spica/Regulus 命名」作業も同じ作業ツリーにまとめて乗っている。

続けて利用者から「claude code のgithub repoも取得し解析した上で、この表示部分の改良(ツール呼び出しのライブ表示)を検討したい。なお、表示改良は次のバージョンに進めたいので、今の実装でバージョンを確定させる」との指示を受けた。これを受けて`v0.4.0`「`Regulus`」を本節執筆時点の実装内容で確定させた——`CHANGELOG.md`に`[0.4.0]`エントリを追加し(TUI v2・オンボーディング・codex互換サブコマンド・スラッシュコマンド全16個・非同期化・`/resume`・`/model`(2段階ピッカー)・`/permissions`・`/skills`・`/fork`・`/export`・`/pwd`・ロゴ変更・effort値バグ修正をまとめて記載)、`c24c1f7`以降の全ての未コミット変更を1コミットにまとめ、`v0.4.0`として git tag した(push はまだ行っていない——別途明示的な承認が要る)。ツール呼び出しのライブ表示改良(Claude Code CLI自身の`⏺`表示を参考にする案)は次のバージョン(`v0.5.0`以降)へ持ち越す。

## 未統合の worktree

`worktree-feat-tui-live-progress`（パス `.claude/worktrees/feat-tui-live-progress`、branch `worktree-feat-tui-live-progress`）は、TUIフルスクリーン化計画(9タスク)を完了させ`main`へ`git merge`済み(コミット`319e624`)。**内容自体はmainに統合済みで、worktreeとしては用済み**だが、`git worktree remove`が「別の生きたClaude Codeセッション(pid確認済み、このセッションと同じ会話履歴から再開されたプロセス)がロック中」との理由で拒否されたため、削除せず残っている。次回、そのpidが生きていないことを確認できれば `git worktree remove` → `git branch -d worktree-feat-tui-live-progress` で片付けてよい。

**過去に存在した `worktree-feat-m4-core` は、このセッションが中断・再開する間に別のセッションが `docs/superpowers/plans/2026-08-20-polaris-m4-core.md` の全12タスクを完了させ、main へマージし、削除した。** このセッションは自分の再開時点でその worktree もSDD台帳(`.superpowers/sdd/2026-08-20-polaris-m4-core/progress.md`)も既に消えていることを確認し、Task 2 の実装のために再ディスパッチしていたsubagent(停止扱いになっていたもの)を、目的の worktree が存在せず作業がmainへ既に統合済みと確認したうえで、再開せずに破棄した。

## main の `cargo fmt --check` drift

M3b 完了時点(HEAD `b9db4e1`)では「fmt clean」だったが、TUI・TUI v2 の実装コミット群のどこかで rustfmt を通さないまま入った差分が蓄積していた。TUI onboarding のマージ作業の一環で `cargo fmt -p polaris-tui` を実行し、`polaris-tui` 配下(`persist.rs`・`render.rs`・`onboarding.rs`)の drift は解消済み。HEAD `c24c1f7` で `cargo fmt --all -- --check` を実測すると残り3件(`polaris-core/src/project.rs` 1件、`polaris-provider/src/openai.rs` 2件)——いずれも TUI/onboarding 作業とは無関係な既存ファイルで、まだ直っていない。`cargo test --workspace` は452件全緑、`cargo clippy --workspace --all-targets -- -D warnings` は clean。

## M2 の進捗

全 12 タスク完了。

| | タスク | 状態 |
| --- | --- | --- |
| 1 | `polaris-sandbox` と方針の型 | 完了 |
| 2 | 拒否を事前に予測する述語 | 完了 |
| 3 | macOS の Seatbelt プロファイル生成 | 完了 |
| 4 | Linux の landlock 適用 | 完了 |
| 5 | `run_confined` と適用失敗の区別 | 完了 |
| 6 | プロジェクトルートの解決 | 完了 |
| 7 | 拘束ヘルパとバイナリの退避 | 完了 |
| 8 | `write` と `edit` ツール | 完了 |
| 9 | `bash` ツール | 完了 |
| 10 | 承認境界 | 完了 |
| 11 | 監査ログが方針と書込先と結果を運ぶ | 完了 |
| 12 | ループと CLI への接続、予算の再測定 | 完了 |

### 最終レビューが見つけたこと

ブランチ全体の最終レビューは、M2 の中核機能が本番経路で一度も動いていないことを発見した。macOS の Seatbelt プロファイルに `(allow sysctl-read)` が無く、Rust は `main` に入る前にメインスレッドのガードページを張る際 `sysconf(_SC_PAGESIZE)` を引く。macOS ではこれが sysctl へ落ちるため、拒否されるとページ長が取れず、続く `mmap` が EINVAL で失敗して `fatal runtime error` から SIGABRT に至る。`--confined-apply` ヘルパは既定の `workspace-write` と `read-only` の両方で必ず落ち、`write` と `edit` はワークスペースの内側でさえ失敗し、その abort が方針違反による拒否と同じ形でモデルへ届いていた。

12 タスクすべてが個別レビューを通り、254 件のテストが緑で、変異検証も各タスクで実施したうえで、これがすり抜けた。原因は 1 つである。**実バイナリを実プロファイルの下で走らせるテストが 1 本も無かった。** ツール層のテストはすべて `/bin/sh` の代役ヘルパを使っており、代役はどんなプロファイルでも起動する。受け入れ基準 3 は代役に対して成立し、本物に対しては成立していなかった。

修正 1 回で 6 件を閉じ、範囲を絞った再レビューで 9 件の変異すべてが捕捉されることを確認した。最も重いのは、ヘルパを「正常終了して何も書かない」に変えた変異である。終了コード 0、stderr 空であるから、内容の表明だけがこれを捕まえられる。finding 1 が緑のまま出荷できた形そのものであり、否定だけを主張するテストがなぜ役に立たないかの実例になっている。

再レビューはさらに 2 件を差し戻した。`/dev/null` が両プラットフォームで書けず `cmd > /dev/null` がシェル本体を一度も実行せずに落ちること、そして findings 1・2 に答えたテスト自身が絶対に落ちない表明を 1 行抱えていたことである。前者は `bash` を壊したまま出荷することになり、後者はこのマイルストーンの教訓そのものを成果物の中で再現していた。どちらも修正済みである。

`/dev/null` の許可は両プラットフォームで最小の権利に絞った。macOS は `(allow file-write-data (literal "/dev/null"))` で、`file-write-create` だけでも `file-write-mode` だけでも開かない。Linux は `PathBeneath` に `AccessFs::WriteFile` 1 つで、`Truncate` は ABI V3 のためこの V2 ruleset では不要である。read-only にもこの 1 行を出している。書いた内容をカーネルが捨てる以上、read-only が守る性質は減らず、分岐を設けると `--sandbox read-only` の `bash` だけが同じ誤解を招く拒否を出し続けることになる。実 `/usr/bin/sandbox-exec` に対して手で確認した。`> /dev/null` は通り、普通のファイルへの書き込みは拒否されファイルも作られず、`rm /dev/null` は拒否され、`/dev/zero` も拒否のままである。

### 検証

HEAD `ba83f0c` で、ホスト 265 件、カーネル 6.19.7 のコンテナ 260 件、いずれも 0 失敗。clippy は `-D warnings` で clean、fmt も clean、`git status --short` は空。強制は両プラットフォームで実機検証を通っており、macOS は実 `sandbox-exec` に対して 3 モードすべて、Linux はコンテナでカーネルレベルの拒否を観測した。実バイナリを実プロファイルの下で走らせるテストは `crates/polaris-cli/tests/confined_helper.rs` にあり、両プラットフォームで走る。

## M2.5 の進捗

全 10 タスク完了。ChatGPT のサブスクリプション認証（OAuth）で polaris を実モデルへ繋ぐ、独立クレート `polaris-auth`（PKCE S256、`~/.polaris/auth.json` への保管、トークン交換と更新、ログインフロー）と `polaris-provider/src/codex.rs`（Responses API への変換、SSE の畳み込み、401 での再試行）を追加し、`polaris-cli` へ `login`/`logout` サブコマンドと `POLARIS_PROVIDER` を配線した。

実装に要した値は逆解析ではなく、OpenAI が配布している codex CLI 0.147.0 と、OpenAI 自身が Codex for Open Source の案内で第三者クライアントを名指ししている事実から得た。`~/.codex/` には読み書きとも一切触れない。refresh token のローテーションで `codex login` 側を失効させる事故を原理的に起こさないためである。

### 最終レビューが見つけたこと

ブランチ全体の最終レビューは、Task 3 と Task 5 でそれぞれ「様子見」と裁定していた 2 件の Minor が、組み合わさると実際に到達可能な不具合になっていることを発見した。トークン更新のたびに `account_id` が空文字へ書き換わる欠陥である。`refresh_token` には応答が省略した場合の既存値へのフォールバックがあるのに、`account_id` には無かった。サーバの更新応答が `id_token` を省略することは珍しくなく、そのたびに実在する `account_id` が `""` へ上書きされ、`~/.polaris/auth.json` へ永続化され、`chatgpt-account-id: ` という空ヘッダとして送信され続ける。401 を受けて再更新しても同じ経路で再び空文字になるため、モデルには的外れな「`polaris login` をやり直すこと」というメッセージだけが返り続ける。仕様が要求している受け入れ基準 3（期限切れトークンからの更新確認）の手順が README に無かったことも同時に見つかった——その手順を実際に手で踏んでいれば、この欠陥に気づけたはずだった。

修正は `refresh_token` と同じ形のフォールバックを `account_id` へも通す 1 回のラウンドで片付けた。同じ修正で Task 5 の繰り越し（`ensure_fresh`/`force_refresh` の成功時の永続化を検証するテストが 1 本も無かった）も閉じている。範囲を絞った再レビューは、単体テストではなく本番の呼び出し経路 `ensure_fresh` で実際にフォールバックが効くこと、ディスクへ永続化された値まで検証されていることを独立に確認し、出荷可の sign-off を出した。

再レビュー自身が新たに見つけた 2 件の Minor は、修正せず裁定して繰り越した（最終レビューのスキル規約が「2 回目の修正波は無い」と定めているため）。

- `account_id` のフォールバック連鎖は、サーバが明示的に空文字の claim を送ってきた場合（`null` でも省略でもなく）にはまだ空文字で止まりうる。今回の修正による退行ではなく既存の挙動だが、修正のコメントと報告書はどちらも「もう起きない」と書いており、その主張は偽になった
- `force_refresh`（401 再試行の経路そのもの、今回の欠陥が実際に通っていた道）は同じ配線変更を受けたのに、回帰を検知するテストが 1 本も無い。`save_to` の呼び出しを削る変異も、`account_id` の受け渡しを `None` に変える変異も、344 件すべて緑のまま通り抜ける

いずれも「今日の時点では正しいと確認済みのコードにある、検知の穴」であり、Task 3+5 の組み合わさり方とは違って他の繰り越しと連鎖しないことを再レビューが確認している。

### 検証

HEAD `dbb5e82` で 344 テスト、clippy `-D warnings` clean、fmt clean、`git status --short` 空。常時コンテキストへの影響はゼロトークンで、codex 側のツールワイヤ形式（482 トークン）も openai 側（492 トークン）と並んで独立に固定した——将来どちらかの形式が肥大しても気づけるようにするためで、以前は openai 側しか測っていなかった。

### 実キーでの初回実行と、そこで判明した2件

M2.5 完了後、初めて実際の ChatGPT サブスクリプションで一気通貫を実行した（`polaris login` → `POLARIS_PROVIDER=codex polaris -p "…"`）。受け入れ基準 1・2・3 をすべて実バックエンドで確認した——保管先とパーミッション（`-rw-------`）、`~/.codex/auth.json` が全過程で不変であること、実際の一発実行、期限切れからの更新まで。

実行して初めて2件の食い違いが出た。

- 設計時にバイナリの文字列から拾った既定モデル名（`gpt-5.1-codex-max`・`gpt-5.2-codex`・`gpt-5.3-codex`）が、実カタログに1つも存在しなかった。実バックエンドは 400 で「ChatGPT アカウントでの Codex 利用ではサポートされていない」と明確に拒否していた（認証自体は通っており 401 ではなかった）。`codex debug models` で実カタログを取得し、既定を `gpt-5.6-sol` へ差し替えた
- 利用者からの追加要求で、ChatGPT のプラン（`chatgpt_plan_type`。`access_token` の JWT に既にある claim）から `reasoning.effort` を自動で決める経路を足した。`plus` は `low`、`pro` で始まる値は `xhigh`。Pro の利用量ティア（5x/20x 等）は `plan_type` だけでは区別できないと分かったため、pro 系は一律 `xhigh` に倒す裁定を利用者から得た。どちらにも当たらない値は `reasoning` キー自体を送らずサーバの既定へ委ねる

いずれも仕様の「保証しない範囲」が最初から明記していた形の食い違いであり、動かして初めて見える種類のものだった。修正は 350 テストで再検証済み。`~/.local/bin/polaris`（`codex` と同じ場所）へ PATH を通した。

## 委譲して待っているもの

なし。

この欄には、子エージェントへ投げて結果を待っている作業だけを書く。投げた対象、待っている成果、投げた時点のコミットを残す。待ちが無いときは「なし」と書く。

## 測定値

| 指標 | 値 | 測ったテスト |
| --- | --- | --- |
| 常時コンテキストの下限（システムプロンプトとツール定義のみ） | 496 トークン | `budget.rs` の `always_on_context_stays_within_budget` |
| 憲法を上限まで充填し実環境と skill 100 件を与えた場合 | 672 トークン | `constitution.rs` の `full_always_on_context_stays_within_budget` |
| 真の同時最大（憲法と環境を同時に飽和させ skill 100 件） | 855 トークン | `constitution.rs` の `absurdly_long_cwd_cannot_push_the_assembled_system_over_budget` |
| codex 側ツールワイヤ形式での下限（openai と並行して独立に固定） | 486 トークン | `budget.rs` の `always_on_tokens_counts_the_wire_shape_not_the_bare_tool_spec` |
| 上限 | 990 トークン | |
| ツール本数 | 5 / 上限 6 | |
| テスト | 350 件（ホスト。Linux コンテナは M2 完了時点で 260 件を確認、M2.5 以降は polaris-auth/provider のみで Linux 固有のサンドボックス経路には触れていない） | |

常時コンテキストは、実際に送信されるシステムプロンプトとツールスキーマを `tiktoken_rs::o200k_base()` で数えた実測値である。見積ではない。

### `crates/` 配下を全て英語化した

利用者の指示で、`crates/` 配下のソース（エラーメッセージ・doc/インラインコメント・ログ出力・CLI ヘルプ・ツールスキーマ）を全クレート（`polaris-tools`・`polaris-sandbox`・`polaris-auth`・`polaris-provider`・`polaris-core`・`polaris-skills`・`polaris-cli`）で英語へ翻訳した。`docs/`・README.md・AGENTS.md は対象外のまま日本語で残している。7クレートをクレート単位で順に翻訳し、各クレートでテスト件数の不変とビルド成功を確認した。

ツールスキーマの英語化で常時コンテキストの実測値が動いた。下限は 582→496 トークン、真の同時最大は 941→855 トークンへ下がった（英語の方が同じ内容を tiktoken で少ないトークン数に符号化するため）。上限 990 に対する余裕は広がった。codex 側のツールワイヤ形式（openai とは別形式）も、既定モデルの実測と並んで独立に固定するテストを新設した。

作業中、翻訳を担当したエージェントがセッションの利用上限に2回当たり、いずれも未コミットの作業が残った状態で中断した。1回目は稼働中のエージェントを `SendMessage` で再開しようとしたが、届いた先が文脈を持たない別インスタンスだったため失敗し、元のエージェントの完了を待つ形に切り替えた。2回目はツリーの状態（コミット未実施、失敗していたテスト2件の原因）を確認したうえで新しいエージェントへ引き継がせ、無事に完了させた。テスト用のバイト境界フィクスチャ（"あ" が3バイトの UTF-8 であることを利用したテスト）で2文字だけ日本語が取り残されていたのを検出し、意味を保つ形（同じく3バイトの "★" へ置換）で自分で直接修正した。

**数値を書くときは、それを産んだテスト名を必ず添える。** この表は実装者とレビュアの実測で三度訂正されている。以前ここにあった「176 トークン」「23 件」、および `constitution.rs` のコメントにあった「525」は、どのテストも測っていない値だった。

## 決めたこと

- subagent の契約は型定義へ畳み込み、呼び出しは 60 バイト程度に収める。現行 orchestia の 1 体あたり約 2,900 バイトに対して 98 パーセントの削減
- 常時コンテキストは skill 数から独立させる。発見はローカルのルータが担い、skill 名の一覧を常時載せない
- ルータは加算のみで、憲法ブロック、ツール定義、サンドボックス方針、承認境界には触れられない
- 選ばれた skill はメッセージ末尾へ差し込む。接頭辞を動かさないことでプロンプトキャッシュを維持する
- 監査ログに署名は付けない。インプロセスでは署名する主体と行為する主体が同一であり、ログ以上のことを証明しない
- subagent は深さ 1 で固定し、動的な分割は継続波で扱う
- サンドボックスは自作せず、macOS の `sandbox-exec` と Linux の landlock へ委譲する。共通の抽象は置かない
- ファイルシステムへの変更はすべてプロセス境界を越える。`full-access` であっても越える。試験する経路と本番の経路を同一に保つためである
- 強制が適用に失敗した状態は、拒否とは別の事象として扱い、必ず硬い失敗にする
- 述語は助言であり、強制するのは OS である。errno から方針違反を復元できないため、両者の食い違いは差分テストで既知の一覧として管理する
- 承認は `write` と `edit` では事前に予測して止め、`bash` では試行して理由を返す。前者は対象パスが一つに定まり、後者は定まらない
- 不変条件は、検出できる形ではなく表現できない形にする。`AlwaysOn` のカプセル化がその最初の適用である
- 切り詰めた出力は、切り詰めたことを本文で述べる。印の無い部分的な結果は、完全な答えとして提示された誤った答えである
- 代役ヘルパは本物より制約に強い。本物の成果物を本物の制約の下で走らせる経路を最低 1 本持つ
- サブスクリプション認証は自分専用の store を持ち、他社のクライアント（`codex`）の資格情報には読み書きとも一切触れない。refresh token のローテーションで相手を失効させる事故を、共有ではなく独立によって原理的に防ぐ
- 「ほぼ常に関連する」skill の選定は、コーパス固有の skill 名ではなく description の書き方の慣習（trigger 形式・量化語・固有名詞の有無）だけに依存させる。新規 skill が同じ慣習で書かれていれば、コード変更なしに同じ扱いを受ける
- 常時関連候補は、常時オンのシステムプロンプトではなく `lookup` の戻り値（ツール結果、メッセージ末尾）にだけ載せる。skill 数に依存しないという既存の不変条件を一切緩めない
- BM25 のインデックスは `lookup` 呼び出しごとに毎回組み立て、キャッシュは持たない。skill 数が数百〜千のオーダーである限り、その前に必ず挟まるモデルとの往復に比べて無視できるという判断による。この判断自体は未計測

裁定の全文は各計画の台帳の `Ruling:` 行にある。M2 の台帳には 20 件、M2.5 の台帳にはこの文書へ移した最終レビューの裁定を含めて記録している。M3b の台帳（8 件、うち 4 件は最終レビューの再レビューでの裁定）は次の通り。

- plan の Step 8 の誤り（`--all-targets` は test-only 呼び出しに対して dead_code を防がない）を認め、`#[allow(dead_code)]` を一時的に許容
- plan 自身のテスト期待値の誤り（`stem("creates")` は `"create"` ではなく `"creat"`）を実装ではなく期待値側の誤りと判定
- `docs/filemap.md` の再生成を Task 4 の計画外コミットとして許容——plan の `cargo test --workspace` という明示の制約を満たすために必要だった
- 最終レビューの Minor 4 件（コメントの記述漏れ、部分文字列フォールバックの打ち切り通知欠如など）は非 load-bearing と判定し、2 回目の修正波を行わず裁定して繰り越した

## M2 で閉じた繰り越し

M1 と M3a から持ち越していたもののうち、M2 で解消したもの。

- ハードリンク経由のパス方針の迂回。述語の `st_nlink > 1` 検出と承認ゲートで緩和した。保証ではなく緩和である旨は仕様の「保証しない範囲」に明記した
- 監査ログの記録がサンドボックス方針と書込先を運べない。`Record` が両方を運び、全フィールドが遮蔽と上限の両方を通る
- `secret_screen::is_excluded_path` が未使用のまま公開されている。固有の被覆を `is_denied` へ畳んだうえで削除した。この照合の過程で、`Path::extension()` が `.pem` のような裸のドットファイルに対して `None` を返す実際の穴が見つかり、拡張子判定を 8 件すべて `name.ends_with(".ext")` へ変えた
- サイズ上限のテストが 12 バイトしか書いておらず、上限そのものを試していない。`set_len(MAX_READ_BYTES + 1)` で上限を跨ぐようにした
- プロジェクトルートがプロセスの cwd である。`project::resolve_root` が `.git` と `.polaris` を目印に最寄りを採り、サンドボックスのルートと状態ディレクトリの両方がこれを使う

## 繰り越す指摘

### M3 へ（担当を付けて送る）

- macOS の制限プロファイルで `/dev/stdout`、`/dev/stderr`、`/dev/fd/N` が拒否される。fdesc 越しの再判定という別の測定を要するため、M2 では意図的に触れていない。Linux では `/proc/self/fd` 経由で解決するため既に通っており、この課題は実質 macOS だけである
- `mktemp` が失敗する。`$TMPDIR` が宣言した書込可能ルートの外にあるためで、作業用ルートを宣言するかどうかを決める必要がある
- macOS のプロファイル下で `chmod 666 /dev/null` が終了コード 0 を返す。`/dev/null` の許可の有無にかかわらず 0 なので今回の変更による退行ではないが、誰かの目が要る

### M4 へ

- **6 本目のツールと予算の衝突は、M4 の最終タスクではなく M4 の仕様で決める。** レビュアの実測で、6 本目を足すと下限 670 / 上限 1029 となり、上限 990 を超える。梃子は 2 つあり、`edit` のスキーマが 136 トークンあることと、`ENVIRONMENT_LIMIT` の 200 が実測コストの 8 倍にあたることである。M2 は 941/990 のまま出荷した。存在しないツールのためにスキーマを削るのは、仕様自身が記録している「腐った数値」の誤りを繰り返すことになる

### M3b で閉じた繰り越し

- skill 内容をシステムプロンプトではなくセッションへ押し込むと skill 数に比例して増える懸念。`near_universal` は `lookup` の戻り値のみに載り `assemble_always_on` には触れない設計とし、`budget.rs` の新規テストで固定した。意図した末尾投入と意図しない静的カタログを区別する仕組みという当時の要求そのものが、この設計選択によって満たされた
- 検索結果とクエリの byte 上限の再検討。`MAX_RESULTS`/`MAX_LIST_BYTES` の値自体は変えていないが、`near_universal` をバイト上限の先頭側へ置く並び替えにより、常時候補が大きい description のコーパスで真っ先に切り詰められる欠陥（最終レビューが発見）を閉じた

### M4 へ新たに繰り越すもの（M3b 由来、時期未定）

- `near_universal` の並び順（常時候補を先頭に置く）自体を検証するテストが無い。上限件数のテストはバイト上限に達しない小さいフィクスチャのため、並び順を元に戻す変異を検出できない。1 つの近傍候補 + 複数 KB 級の description を持つ content 候補、というフィクスチャで閉じる
- 部分文字列フォールバック（BM25 が 0 件のときに発火）が `MAX_RESULTS` を超えるヒットを静かに打ち切り、`list_candidates` の「showing N of M」通知を出さない。既存の BM25 経路も同じ挙動で、この変更による退行ではないが、直すなら両経路をまとめて直す
- `near_universal` の選定基準が拾えない書き方の実例が既知で残っている（`spec-first-development` のように trigger 形式でない description）。基準を広げるかどうかは、コーパス固有のハードコードにならない範囲でのみ検討する
- 壊れた global `config.toml` が project 側の `skills.paths` も巻き添えにする。層構造を全部か無かから file 単位の復旧へ変えることになり、Task 1 の裁定を反転させる
- YAML が既に禁じている入力に対する端の挙動が三件。タブ混在インデントの誤 dedent、末尾空行の clip chomping との差異、平文スカラー直後のインデント行の取り込み
- skipped 一覧の見た目が三件。同名が skills と skipped の両方に出る、非 ASCII の並び順が未検証、探索パス間で重複除去していない

### M2.5 で閉じた繰り越し

Task 5 が繰り越していた「`ensure_fresh`/`force_refresh` の成功時の永続化を
検証するテストが 1 本も無い」は、最終レビューの修正でその半分（`ensure_fresh`
側）を閉じた。`force_refresh` 側は次項へ持ち越す。

### M2.5 が新たに繰り越すもの（時期未定）

- `account_id` のフォールバック連鎖が、サーバが明示的に空文字の claim を送った
  場合にまだ空文字で止まりうる。`refresh_token` が既に持つ
  `.filter(|s| !s.is_empty())` を `account_id` 側にも足せば閉じる。1 行
- `force_refresh` に回帰検知が無い。`save_to` の呼び出しを削る変異も、
  `account_id` の受け渡しを外す変異も、テストが 1 本も落ちない。
  `ensure_fresh` 側に足した成功時のテストと対になる形を `force_refresh` にも
  書けば閉じる
- 実バックエンドが空の `chatgpt-account-id` ヘッダをどう扱うかは未確認。
  `exchange_code`（新規ログイン）経由でなお `account_id` が空になりうる経路が
  理論上残っており、実際に拒否されると分かった時点で、その場でハードエラーに
  すべきかを決める

### 時期未定

- 5 MiB の読み取り上限はファイルを縛るが、モデルが大きな `limit` を指定した場合の返却バイト数は縛らない
- クライアント生成がタイムアウト定数を実際に適用することを固定するテストが無い。停止した応答を打ち切る挙動のテストはある
- 憲法の見出しフォールバックが `## ` でしか止まらず、`# ` や `---` で次節が始まる AGENTS.md では EOF まで拾う
- `with_home` の SAFETY コメントが mutex の効き目を実際より広く書いている。`tempfile::tempdir()` が lock の外で `TMPDIR` を読む
- `edit` が書込可能ルートの外にある存在しないファイルを指したとき、`MutationFailed`（方針の問題ではない）を返す。ENOENT であり読み取りは開いているので嘘ではないが、承認ゲートが先に発火するためエージェント経由では到達しない。1 往復ぶんの損である
- `codex` の実際のTUIは、ターン実行中「Working (Ns · esc to interrupt)」という経過秒数付きの表示を出し、Escで途中中断できる。polaris の `polaris-tui::run()` は `ratatui::crossterm::event::read()` によるブロッキング待ちで駆動しており、`agent::run().await` の実行中に別のキー入力を割り込ませる経路が無い(タイマー表示・中断のどちらにもポーリングかバックグラウンドタスクへの再構成が要る)。実装すればターン中の応答性が上がるが、イベントループの構造そのものを変える規模のため、この回のスコープには含めなかった

## 進め方

2026-08-17 以降は自走する。選択肢は推奨値を採り、判断は台帳に `Ruling:` として残す。

次の操作には到達しても実行せず、そこで止める。push、`main` へのマージ、リリース公開、`reset --hard` や `clean -fdx` などの局所破壊操作、リモート削除。v1.0.0 のタグ付けも準備までとする。

## 次の一手

本節は2026-08-26、プロンプトキャッシュ改善(`235ec5d`)を`v0.7.0`「`Zubenelgenubi`」としてコミット・タグ付けした直後に書き直した。`v0.6.0`はorigin push済み。`cargo test --workspace`(全緑)・`cargo clippy --workspace --all-targets -- -D warnings`(clean)・`cargo fmt --all -- --check`(clean)を実測で確認済み。

優先度順:

1. **プロンプトキャッシュ修正の A/B 実測。** 手順は「プロンプトキャッシュの利用率」節の「未実施」に書いた。`235ec5d`前後のビルドを両方用意して比較する。ここで9割台に乗らなければ、`reasoning` item を回収していない件が効いている疑いが立つ
2. **`Folder::take_item`の`reasoning` item欠落への対応方針を決める。** A/B実測が9割台に乗らなかった場合、あるいはツール呼び出しを跨いだ推論連続性を狙う場合に着手する。`Message`型の拡張を伴うため設計判断が要る
3. **`v0.7.0`分の`git push`(main分2コミット・`v0.7.0`タグ分)の実施を利用者に確認する。** タグ・CHANGELOGは確定済みだが、push自体はまだ明示的な承認を得ていない
4. **`worktree-feat-tui-live-progress`の後始末。** ロックしていたセッションのpidは確認できなくなった。`git worktree remove`を試し、拒否されれば中身を見て利用者に確認する

そのうえで v1.0.0 のタグ付けの判断へ進む。

キャッシュヒット率の記録は、上の A/B 実測がその作業にあたる。設計が主張する「接頭辞を動かさないからキャッシュが効く」という性質を観測に変える。

M4 の仕様は、6 本目のツールと 990 トークン上限の衝突（上の「M4 へ」参照）と、M3b が新たに繰り越した項目（並び順のテスト不在、部分文字列フォールバックの打ち切り通知）を先に見ておく。

M1 の受け入れ基準のうち無人では確認できなかった「実キーでの一発実行」は、
`polaris login` の経路が揃ったことでキー無しでも満たせるようになった。
`polaris login` → `POLARIS_PROVIDER=codex polaris -p "…"` の手順は README に
記した。実際にブラウザでの認可を経て確認する作業は、利用者の手による一回きりの
実行が要る。
