# Error log

## 2026-07-20: Phase 9 navigation追加で80x24 helpの既存キーが範囲外

- Command: `cargo test`
- Error: `help_snapshot_covers_phase_six_keys_and_draft_at_80_by_24` が `X` を検出できず失敗（68/69 pass）。
- Cause: Ctrl-D/U・u・[/] を各1行で追加し、MAIN/ROOM後半を初期viewport外へ押し出した。
- Fix: navigationとread説明を複合行へ圧縮し、新旧キーを80x24初期表示へ収める。

## 2026-07-20: Phase 9 fold state test の旧HashSet API参照

- Command: `cargo test --no-run`
- Error: `expanded_messages.contains` が `HashMap<String, HashSet<i64>>` への変更後に存在せず E0599。
- Cause: S8-7 実装後、既存fold test 2 assertionが旧global HashSet APIを参照していた。
- Fix: active teamのsetを取得してmessage idの有無をassertする形へ更新。

## 2026-07-20: Phase 7 snapshot assertion patchのcontext不一致

- Command: `apply_patch`でT1 assertionを更新。
- Error: rustfmt前の複数行配列と想定した1行contextが一致せずhunkを適用できなかった。
- Fix: `nl -ba`で実行番号を確認し、実際の複数行contextへ最小patchを再適用。

## 2026-07-20: Phase 7 stats列snapshotの空白期待不一致

- Command: `cargo test agents_snapshot_contains_two_teams_and_three_identities_at_80_by_24`
- Error: `s  1/r  0` を期待したが、右寄せ後のrecv末尾paddingは画面文字列上で `r0  ` だった。
- Cause: 可視数字ではなくtrailing spaceを含む文字列をassertしていた。
- Fix: fixtureをsent=123/recv=456へ設定し、80x24で`s123/r456`が欠けず表示されることを直接検証。

## 2026-07-20: Phase 7 全team表示への変更時の括弧漏れ

- Command: `rustfmt --edition 2024 src/ui/agents.rs` / 絞り込み`cargo test`
- Error: `items.push` の追加漏れでclosing delimiter error。併せて`cargo test`へ複数filterを渡し引数エラー。
- Cause: selected team限定Listから全team flatten Listへの手動変換ミス。
- Fix: `items.push(ListItem::new(...))`へ修正し、テストfilterは単一substringで実行。

## 2026-07-20: Phase 7 Agents 80x24 snapshotでidentity名が省略

- Command: `cargo test`
- Error: `agents_snapshot_contains_two_teams_and_three_identities_at_80_by_24` で `opencode-review` が見つからない。
- Cause: name列を13文字固定にし、project列より先にidentity名を省略していた。
- Fix: name列を16文字へ拡張し、仕様どおりproject列を優先して短縮するよう修正。

## 2026-07-20: Phase 6 PTY clipboard確認でpbcopyがexit 1

- Command: release binaryをPTY起動し、Roomで`y`を入力。
- Error: OSC 52シーケンス出力後、sandbox内の`pbcopy`がexit status 1。
- Cause: 実行環境からmacOS pasteboard serviceへ接続できないため。OSC 52出力自体は成功。
- Fix: fallbackをbest-effort化し、pbcopy失敗がOSC 52の成功結果を上書きしないよう修正。実pasteboard反映はdeviationとして報告する。

## 2026-07-20: Phase 6 clipboard確認用tmux socketの作成拒否

- Command: `tmux -L agmsg-phase6 new-session ...`
- Error: `/private/tmp/tmux-503/agmsg-phase6` の作成が `Operation not permitted`。
- Cause: tmuxの既定socketディレクトリがsandboxの書き込み対象外として扱われた。
- Retry: `TMUX_TMPDIR`と明示`-S` socketの双方を許可済みパスへ指定したが、UNIX socket作成自体が同じ理由で拒否された。
- Fix: release binaryを直接PTY起動して`pbcopy` fallbackをend-to-end確認し、tmux/mosh経路はdeviationとして報告する。

## 2026-07-20: Phase 6 help snapshot のタイトル期待値不一致

- Command: `cargo test`
- Error: `help_snapshot_covers_phase_six_keys_and_draft_at_80_by_24` が旧タイトル文字列を期待して失敗。
- Cause: helpタイトルをMain文脈が明確な表記へ変更した際、assertionの文字列更新が漏れた。
- Fix: TestBackendの実表示 `Main Esc=clear only | q=quit` に期待値を合わせた。

## 2026-07-20: agmsg連絡文のshell quotingミス

- Command: `send.sh` のmessage引数にbacktick付きgit例を含めた。
- Error: 意図せずcommand substitutionされ、ops-hubの`.git/FETCH_HEAD`更新がsandboxに拒否された。
- Impact: sandbox拒否によりrepository変更なし。連絡文のcommand部分だけ欠落した。
- Fix: backtickを含まないplain textで即時再送した。

## 2026-07-20: tokio interval の Clippy 指摘

- Command: `cargo clippy --offline -- -D warnings`
- Error: audit auto-refresh 判定の nested `if` が `collapsible_if`。
- Cause: interval tick と Audit screen 判定を分けていた。
- Fix: 2条件を単一 `if` に統合。

## 2026-07-20: Audit 大表示後の snapshot assertion 不一致

- Command: `cargo test --offline`
- Error: `ui::tests::audit_renders_at_80_by_24` が旧 `TOTAL 83/100` 文言を期待。
- Cause: total score を3行の大表示に変更したため。
- Fix: `/100`、`10 AXES`、`PAIR MATRIX`、`ACTIONS` の領域単位 assertion に更新。

## 2026-07-20: Phase 2 統合後の rustfmt 差分

- Command: `cargo fmt -- --check`
- Error: `app.rs` と `main.rs` に formatter 差分。
- Cause: 並行変更統合後の改行・import順。
- Fix: `cargo fmt` を適用してから品質ゲートを再実行する。

## 2026-07-20: Phase 2 パッチ適用とテスト列挙の入力ミス

- Command: `apply_patch` / `rg` による作業確認
- Error: 一括パッチの hunk 境界不正、および test attribute 検索の正規表現不正。
- Cause: 複数ファイルの patch 区切りと bracket escape の誤り。
- Fix: ファイル単位の patch と `cargo test -- --list` に切り替えた。ソース変更前の失敗で機能影響なし。

## 2026-07-20: Phase 2 初回 cargo check の参照型不一致

- Command: `cargo check --offline`
- Error: pair matrix 描画の `&String` / `String` 比較で `E0277`
- Cause: 可視列が `Vec<&String>` のため iterator item が二重参照になっていた。
- Fix: 比較時に一段だけ dereference するよう修正。

## 2026-07-20: Phase 2 初回 clippy の needless borrow

- Command: `cargo clippy --offline -- -D warnings`
- Error: `exec.rs` の `&Path` をさらに参照していた。
- Fix: `ensure_script_exists(script_path)` へ修正。

## 2026-07-20: 指定リポジトリパスの初期化失敗

- Command: `cargo init --bin /Users/remma/dev/agmsg-tui`
- Error: `.git` 作成時に `Operation not permitted`
- Cause: 実行環境の書き込み許可が `/Users/remma/dev/ops-hub` 配下に限定され、指定先が許可外。
- Workaround: `/Users/remma/dev/ops-hub/shared/results/codex/agmsg-tui` に独立 git repository として実装を継続。

## 2026-07-20: crates.io index 更新失敗

- Command: `cargo test`
- Error: `Could not resolve host: index.crates.io`
- Cause: 実行環境のネットワーク制限。
- Workaround: 既存の Cargo registry cache を repository 内の gitignore 済み `CARGO_HOME` に複製し、`--offline` で検証する。

## 2026-07-20: ratatui 0.29.0 cache 不足

- Command: `cargo test --offline`
- Error: `failed to download ratatui v0.29.0`
- Cause: 指定versionのregistry metadataはあるがcrate本体がローカルcacheに無く、networkも利用不可。
- Workaround: API互換の確認と品質ゲートを完遂するため、ローカルcache済みの `ratatui 0.28.1` を使用。要件との差分として完了報告する。
