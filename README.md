# agmsg-tui

`agmsg` の team、member、room、送信、live tail、既読化、運用 audit、Health & Trends、agent 管理をターミナル内で扱います。SQLite は常に read-only 接続し、全ての書き込みは agmsg script に委譲します。agent reset は `YES` 確認後に `reset.sh` を呼び、対象projectを保持するため `AGMSG_RESOLVE_PROJECT=0` をrunner側で常時付与します。

## Build and run

```bash
cargo build --release
./target/release/agmsg-tui
```

既定値は以下です。CLI option または同名の環境変数で上書きできます。

- DB: `~/.agents/skills/agmsg/db/messages.db` (`--db` / `AGMSG_DB`)
- teams: `~/.agents/skills/agmsg/teams` (`--teams-dir` / `AGMSG_TEAMS_DIR`)
- scripts: `~/.agents/skills/agmsg/scripts` (`--scripts-dir` / `AGMSG_SCRIPTS_DIR`)
- audit: `~/bin/agmsg-audit` (`--audit-script` / `AGMSG_AUDIT`)
- reports: `~/tmp` (`--report-dir` / `AGMSG_REPORT_DIR`)
- UI state: `~/.config/agmsg-tui/state.json`（sidebar幅、last team、team別draft、通知toggle）
- identity: `AGMSG_IDENTITY`（CLI optionなし、env変数のみ）— 未設定でも起動はできますが、以下がすべて無効化されます:
  - Agents画面 `X`/`Del` の自分自身への reset 拒否ガード
  - ROOM の自メッセージ `▏` マーカー
  - `c` composer の `from` デフォルト自動選択（未設定時は roster 先頭にfallbackし、status lineに警告を表示）
  未設定で起動すると status line に一度だけ warning が出ます。

実DBの read-only 診断:

```bash
./target/release/agmsg-tui --diagnose
```

## Accessibility / mobile (Phase 10)

- **Dark bg 前提**: このTUIは dark background の端末を前提にデザインしています。light bg 端末では code block の背景色・inline code の背景色が読みにくくなります（`AGMSG_TUI_THEME` で syntect のtoken色は切替できますが、chrome側の色は dark-bg 前提のままです）。
- **NO_COLOR**: `NO_COLOR` env var か `--no-color` flag（flagがenvより優先）で全UIから色を除去します。Modifier（BOLD/ITALIC/UNDERLINED/DIM/REVERSED）は残ります。fenced code blockの背景・左バーのみ構造的な目印として色を保持します。
- **color-blind safe palette**: `--palette safe` か `AGMSG_TUI_PALETTE=safe` で Okabe–Ito パレット（色覚多様性対応）に切替します。agentのhash色、audit scoreの色、focus borderの色（cyan→yellow）が対象です。
- **syntect theme**: `AGMSG_TUI_THEME=<name>` で code block のtheme（既定 `base16-ocean.dark`）を切替できます。存在しないtheme名を指定するとstatus lineに警告を出して既定themeへfallbackします。
- **狭幅 1-pane mode**: 端末幅が60列未満になると自動で1-pane表示に切替わります（TEAMS/MEMBERS/ROOMのうち`Focus`が指しているpaneだけを全幅表示）。`Tab`/`Shift-Tab`は3-pane時と同じくTeams→Members→Room→Teamsを巡回するので、狭幅時も別のキー操作は不要です。status lineに`[<60cols: 1-pane mode]`が表示されます。

## Keys

- `j` / `k`, `g` / `G`: 移動・filter後の先頭/末尾へjump
- `Ctrl-D` / `Ctrl-U`: 半ページ移動、`u`: 次の未読、`[` / `]`: team巡回
- `Tab` / `Shift-Tab`: teams → members → room の focus 切替
- `Enter`: teamを開く / 2KB超messageを展開
- `c`: composer
- `r` / `R`: 選択messageのrecipient全体 / team全recipientを `inbox.sh` で既読化
- `/`: body/from/to の全DB検索、`Enter`で先頭hit、`n` / `N`で次/前hit
- `y`: 選択messageの全文をOSC 52（macOSでは`pbcopy`にもfallback、失敗時は`report-dir`にログしてstatusで通知）でclipboardへコピー
- `x` / `X`: 選択messageのfold/unfold（`x`単体は500字以下だとno-op、statusで通知）
- `f`: 選択team内の全foldable messageを一括fold/unfold
- `s`: 選択messageと同じsenderの直近messageへジャンプ
- MEMBER: `Enter`で宛先指定composer、`I`でinfo、`F`でfilter、`M`で既読化
- `Ctrl-A` / `a`: audit dashboard を開く
- `H`: Health & Trends を開く（再度`H` / `Esc`でMainへ戻る、`q`は終了）
- Health: `j`/`k`でteam選択、`t`で7d/30d切替、`R`で非同期refresh
- Health: delivery mode、bridge生死（`●`全稼働 / `◐`一部 / `○`停止 / `-`なし）、最終message、stale unread、team/agent trafficを表示
- Health: 表示中は60秒ごとにauto-refresh。幅60列未満ではteam表のsparkline列を省略
- `A`: Agents管理画面を開く（再度`A` / `Esc` / `q`でMainへ戻る）
- Agents: `t` / `Tab`でteam・identity focus、`n`でagent作成/join、`R`でrename
- Agents: team focusの`T`でteam rename、`L`で現identityのteam離脱
- Agents: identity focusの`X` / `Del`でreset（`YES`完全一致確認、self-reset拒否）、`r`で再読込
- Agents: identity focusの`Enter`でidentity info popup（`Esc`/`Enter`で閉じる）
- Agents: spawn/join/rename/reset/leave はすべて非同期実行（confirm直後にUIへ制御が戻り、spinner表示）
- `?`: help
- `q`: 即時終了。Mainの`Esc`はsearch/filter/popupの解除専用
- composer: `Tab`でfrom、`Shift-Tab`でtoを切替、`Ctrl-S`で非同期送信、`Esc`でteam別下書き保存、`Ctrl-K`で下書き消去
- composer: 2048B超で黄、4096B超で赤の警告（4096B超は送信をblock）
- audit: `R`でrefresh、`h`/`l`でteam matrix切替、`j`/`k`でaction選択、`g`で選択先頭へジャンプ
- audit: `D`でzombie reset command表示、`M`でstale unreadを`inbox.sh`経由で既読化
- audit: `Enter`で詳細、`E`/`x`で`~/tmp/agmsg-report-<YYYYMMDD-HHMM>.md`へexport、`Ctrl-A`/`Tab`でmainへ戻る
- audit: 表示中は60秒ごとに非同期auto-refresh（手動refreshとの重複実行を抑止）

send/read/audit/Health/agent管理の全subprocessは10秒でtimeoutします。send/read/Health実行中もpoll・描画・navigationは継続し、同じ操作の連打は抑止します。poll失敗時は1/2/4秒から最大30秒まで指数backoffし、回復をstatusへ表示します。

tmux越しのOSC 52を使う場合は、tmux 3.3+で`set -g allow-passthrough on`を設定してください。

## Stop / recovery

通常は `q` で終了します。`Esc`は解除専用です。応答しない場合は `Ctrl-C` で停止してください。通常終了・エラー・panic のいずれでも raw mode と alternate screen を復帰します。

## Scope

Phase 1〜11として、audit dashboard、Health & Trends、room可読性改善、syntax highlight、全DB検索、状態永続化、通知/burst alert、agent管理、subprocess非同期化・timeoutを実装しています。agent生成は既存`spawn.sh --no-wait`へ委譲し、TUI自体はPTYを管理しません。
