# 対局期限と接続の契約

## 着手期限

手番開始を`t0`、時計台帳の安全予算を`B`、`margin_ms`を`M`、`stop_grace_ms`を`G`とする。
`0 <= G < M`を起動時に検査する。
通常のstop予定は`t0 + B - M`、fallback期限は`t0 + B - (M - G)`で、手番開始時に固定する。
`B <= M`では待たずにfallbackする。
stopが遅れた場合も固定期限までしか待たず、期限を過ぎて起床した場合はそのstepでfallbackする。
OSのスケジューリングやネットワークの遅延そのものを上限内に収める保証ではない。

ponderhit、旧探索の排出、engine交換でも元の手番開始を維持する。
交換待ちや旧探索の排出中は、stop予定時刻で早めにfallbackすることがある。
`go.deadline_at`は従来どおりstop予定時刻であり、fallback期限やserver受理時刻ではない。
`MoveOverhead`をこの保証の代わりにはしない。

## 準備と再接続

| 待ち | 既定の期限 | 設定 |
| --- | --- | --- |
| usiok | 30秒 | `--usi-timeout-ms` / `BridgeConfig::usi_timeout_ms` |
| readyok | 300秒 | `--ready-timeout-ms` / `BridgeConfig::ready_timeout_ms` |
| TCP接続 | 1試行30秒 | 固定 |
| LOGIN応答 | 30秒 | `--login-timeout-ms` / `BridgeConfig::login_timeout_ms` |

準備期限の設定値は正の整数とする。
期限と同時刻にcoreが受け取った正常応答は受理する。
初期engineと交換engineで同じ期限を使い、失敗時は既存の上限（既定2回）まで交換する。
対局中の交換準備は、元の着手期限を延ばさない。
`engine_hang`と`engine_dead` alertのdetailに準備失敗を記録する。

接続失敗は1秒以上待って再試行し、LOGIN成功までの接続試行は最大3回とする。
coreの接続期限またはLOGIN応答期限に達した場合は終了する。
LOGIN拒否も終了し、無期限backoffは行わない。
正常LOGIN後の組合せ待ちは無期限である。
heartbeatはプロセスのログ活動を示し、engine準備完了やserver応答を証明しない。

Playing中のEOFは終了し、再LOGINで同じ対局を継続しない。
正常終局後のEOFは、継続局数が残っていればengineを保持したまま同一プロセスで再接続する。
非x1でも終局後に接続を維持して次summaryを受け取れる。
局数上限や`stop`で終了するときは、接続が残っていればLOGOUTを送信キューへ入れる。

## 次局登録

`--x1 --game-name NAME`を併用した場合だけ、初回LOGIN後と継続する各局の終局後に`%%GAME NAME TURN`を一度送る。
旧探索が残っている間は再登録を保留する。
REJECT通知でも次の登録へ戻るが、同じproposalの重複通知では登録し直さない。
局数上限、`stop`、`quit`では再登録しない。
`--game-name`なしの大会側進行へ追加の`%%GAME`を送らない。
これは通常登録の契約であり、電竜戦の運営拡張が同じ手順で動くとの認定ではない。

## I/Oと観測

engine世代ごと、CSAソケットごとに独立した単一writerと有界送信キューを持つ。
coreはwriteの完了やキューの空きを待たず、position、go、stopは投入順に処理する。
engine側の書込失敗やキュー満杯はengine障害としてcoreへ返し、CSA接続を落とさない。
旧engineと旧socketの遅延通知は世代で除外する。

CSAの部分送信は同じwriterがbyte位置を保持し、後続行はその行のLFの後に送る。
タイムアウトによる行全体の再送は行わない。
送信停滞だけを理由に対局中のソケットを捨てない。
CSAキューが満杯なら空行keepaliveを省略し、critical行の投入失敗はstderrへ報告して現在の対局後に終了する。
既にキューへ入った行と部分行は保持する。
この状態では新しいcritical行を送れていないため、その対局の正常継続は保証できない。

wire/engineログの送信行は、ローカルwrite完了を表す。
serverの受理や着手echoは別の事実である。
quitの投入、write、engine終了待ちを一つの`--quit-grace-secs`内に収め、超過時にchildをkillする。
強制終了後のOSによる終了確認は別枠の最大1秒とし、確認失敗はstderrと非ゼロ終了で通知する。
LOGOUTを含むCSA送信キューの終了時drainにも有限期限を設ける。

既存event schemaとcontract fixtureは変更しない。
通常イベントと通常ログのキューは引き続き無制限であり、F11の負荷測定と有界化は未実施である。
OS TCP Keepaliveの追加（F09）も別変更とし、TCP_NODELAYと空行keepaliveの既定40秒、下限30秒を維持する。
ログ保存失敗時はstderrと終了コードで通知し、現在の対局を完了して次局を受けない。

## codecの時間表現と未対応範囲

codecのTは通信上の整数値をそのまま保持し、単位の推測や正規化を行わない。
bridgeはsummary値とTをTime_Unitの単位として時計台帳へ適用する。
主な運用対象は`Time_Unit:1sec`と整数Tである。
`Time_Unit:1min`でも値を秒で送るshogi-server StopWatchClockとの組合せは未対応であり、使用しない。
数値の大小から単位を推測する処理はない。
対応再開には、対象server revisionを固定したsummary、着手echo、時間切れ裁定のfixtureが必要である。

小数Tは`InvalidNumber("T")`となる。
対局中のparse失敗行はalertを記録して適用しないため、小数Tを送るserverでの対局には使用できない。
対象大会が小数Tを送る証拠が得られた時点で、公開APIの互換性、正確な時間型、formatter、summary replay、時計をまとめて設計する。
CSA棋譜形式の対応範囲とネットワーク対局の必須条件を混同しない。

## operatorの移行

- preflightでも`stop_grace_ms < margin_ms`を検査する。600/500msは算術条件を満たすが、残る100msの実環境での十分性は別途測定する。
- GPUの初期化が5分を超える場合は`--ready-timeout-ms`を明示する。0による無制限化はできない。
- RustでBridgeConfigを直接構築するconsumerは追加3フィールドを指定するか、`BridgeConfig::floodgate`から設定する。
- Actionを網羅的にmatchするRust consumerは、新しい`Action::Probe`を処理する。`probe`モジュールの診断出力は損失許容とし、対局制御の成功条件に含めない。
- `go.deadline_at`と既存event schemaの移行は不要。通常ログのout行はwrite完了になったため、投入時刻として扱わない。
- binaryはrelease tagを固定し、Rust 1.95.0と同梱Cargo.lockで`cargo build --locked --release -p rsshogi-csa-bridge --bin csabridge`を実行する。
- 公式LOGIN形式、運営側game_name変更、公式大会での連続対局は未確認。公式テストセッションで実交換列を記録して資格確認する。
