# Changelog

このファイルは [Keep a Changelog](https://keepachangelog.com/ja/1.1.0/) に従い、
バージョンは [Semantic Versioning](https://semver.org/lang/ja/) に従います。

## [Unreleased]

## [0.1.2] - 2026-08-09

### Fixed

- 規定局数の完走後にCSA serverが接続を閉じない場合、socket readerがlog channelを
  保持して`csabridge.exe`が終了しない問題を修正した

## [0.1.1] - 2026-08-09

### Added

- 依存ゼロの`rsshogi-csa` codecをcrates.ioから利用できるようにした
- crates.io Trusted Publishingによるtag駆動のrelease workflowを追加した

### Changed

- `rsshogi-csa`と`rsshogi-csa-bridge`のversionを0.1.1へ更新した

## [0.1.0] - 2026-08-09

### Added

- workspace と 2 リポジトリ運用（private `develop` / public snapshot）の骨格を追加
- `rsshogi-csa`: CSA プロトコルの行コーデックを追加
  - `CsaCommand` による双方向の command model
  - 寛容な `parse_line` と正規形往復の `parse_line_strict`
  - `format_command` による正規形出力
  - `BEGIN Game_Summary` の入れ子サブブロック解析
  - transcript ベースのテスト
- `docs/architecture.md`: 設計判断の記録
- `rsshogi-csa-bridge`: CSA サーバーで USI エンジンを対局させる crate
  - `ClockLedger`: サーバーの時間計算を再現する純粋な台帳
  - `ClockRules` / `MoveBudget`: 時間規則と、時間切れにならない消費量の上限
  - `UsiGoTimes`: USI の `go` パラメータへの変換（byoyomi と increment の排他、安全マージン）
  - `GameState`: 盤面・時計・履歴を 1 本の経路で更新し、合法性の門番を持つ
  - `SessionCore`: `Event` → `Vec<Action>` の sans-I/O 状態機械
  - `BridgeConfig`: 投了閾値、安全マージン、keepalive、フォールバック方針
  - `RecordBody` / `LogRecord`: 観戦と証跡を兼ねるイベントログ
  - `shell`: socket / subprocess / ログ writer の I/O 層
  - `csabridge`: CLI バイナリ
- ShogiArenaとのevent log contractを固定するmulti-game fixture
- tagからWindows x86_64 binary zipとSHA-256を作るrelease workflow

### Added（続き）

- `heartbeat` / `bridge_stop` レコードを追加した。bridge の生死を
  イベントログから観測できるようにするためである。
  外部から kill された bridge は `game_end` も alert も残さず、
  ログが `phase: playing` のまま途切れるので、
  読み手からは長考中の対局と区別できなかった。
  - `heartbeat` は**沈黙を埋める**。直近 60 秒間に何のレコードも
    書いていなければ書く。対局中は指し手が流れるので自然に抑制される。
    保証されるのは「生きている bridge のログはレコード間隔が 60 秒を超えない」
  - `bridge_stop` は正常終了の証明で、ログの最終行に書く。
    `state{closing}` は shutdown の開始であって完了ではないため、
    「このログは完結している」と言えるのはこのレコードだけである
  - どちらもフィールドを持たない。間隔は設定にしない（読み手が交渉できないため）
  - 既存レコードは 1 バイトも変わらない。追加専用契約の行使である

### Removed

- `rsshogi-csa-watch` を退役させた。
  観戦・事後検証・棋譜変換は ShogiArena が `*-events.jsonl` を読んで行う。
  同じ利用者が既に持っている観戦 UI と二重に持つ理由が無く、
  observability を二重化すると劣化する、という判断による。
  設計の記録は `docs/architecture.md` の 0011 に残した
- bridge の外部挙動は変えていない。
  イベントログ・wire ログ・エンジンログの内容も形式も同じである。
  `record.rs` の型は public のまま残り、
  `wire_lines_round_trip_byte_for_byte` がスキーマの追加専用契約を
  引き続きバイト単位で固定する（workspace 内で唯一の機械的な守り手になった）

### Changed

- CLIのlogin passwordに`--password-env`と`--password-file`を追加した。
  従来の`--password`を含む3方式から1つを選ぶ
- 0.1.0の正式対応範囲をfloodgate通常接続に限定し、x1、公式test server、
  WCSC、電竜戦、buoy、駒落ちをexperimentalと明記
- 時間無制限の対局条件は、USIの0ミリ秒として誤変換せず明示的に拒否する
- GitHub Actionsを完全commit SHAへ固定し、release jobのwrite権限をjob内に限定

- `rsshogi` を 1.0.2 から 1.1.0 に更新した。
  1.1.0 の `Move32::to_csa` は手番記号を含んだ `+7776FU` を返すようになったため、
  bridge が手で記号を前置していた箇所を削除した。
  記号は駒の色から決まるので、盤面の手番と食い違う表記が作れなくなる
- `record.rs` の `side` / `my_color` / `result` を `&'static str` から
  `Side` / `GameResult` enum に変更した（wire 表現は不変）。
  `entering_king_rule` は `String` にした。
  イベントログを読み戻せるようにするための変更で、
  実ログ 1 行の往復がバイト一致することを golden test で固定している

### Fixed

- エンジンが対局中に死んだあと、手番が来ても何もせず時計を溶かす問題を修正
  （`max_engine_restarts` 回まで再起動し、枠を使い切っていれば即投了する）
- 死んだエンジンへの書き込みの `BrokenPipe` がブリッジプロセスを終了させ、
  切断＝負けになる問題を修正（I/O 失敗はイベントに翻訳する）
- 相手の手番中にエンジンが死ぬと投了してしまう問題を修正
- 重複したエンジン死亡報告が再起動枠を二重に消費する問題を修正
- wire ログにログインパスワードが平文で残る問題を修正。
  floodgate形式だけでなく、カンマを含まない通常のCSA passwordも秘匿する
- malformed、必須field不足、Position欠落または未終端の`Game_Summary`を受理できた問題を修正
- CSA指し手型が盤外座標と移動先`00`を受理できた問題を修正
- 指し手のecho待ち中に手動投了すると、相手手番で`%TORYO`を送れた問題を修正
- 同じlog directoryで複数bridgeを起動してもrunごとのJSONL fileが衝突しないようにした
- `engine_meta.options`をrun中に変化しないUSI optionへ限定した

### Notes

- `rsshogi-csa` は dependency を持ちません（dev-dependency も含む）
- `rsshogi-csa-bridge` は `rsshogi` / `rsshogi-csa` / `rsshogi-usi` / tokio / serde / clap に依存します
- `rsshogi-csa-bridge` は `publish = false` です
- イベントログ（`*-events.jsonl`）は追加専用の公開契約です。
  読む側はこの workspace の外にあります
- crates.io への公開はまだ行いません
