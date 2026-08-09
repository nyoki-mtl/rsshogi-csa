# ログと対局後の確認

bridge は起動ごとに millisecond epoch を run ID として取り、同じ ID の三つの file を `--log-dir` に作ります。

同じ directory で複数の bridge を起動しても、既存 file を上書きしません。

| file | 内容 | 用途 |
| --- | --- | --- |
| `<run_id>-events.jsonl` | 意味を持つ session event | 観戦、事後検証、状態の集計 |
| `<run_id>-wire.log` | CSA server との送受信行 | protocol 上の証跡と通信の調査 |
| `<run_id>-engine.log` | engine の標準出力 | USI 応答と探索情報の調査 |

どの log も追記で書き、1 行ごとに flush します。

記録先が作れない場合は、対局を始める前に bridge が停止します。

記録のない対局は、あとから状態も原因も確認できないためです。

## JSONL event log

event log は 1 行 1 JSON record です。

既存 field の意味を変えず、新しい情報は record type または field の追加で表します。

最初の数行は、bridge と engine の情報、接続 phase、待機中の heartbeat のようになります。

```json
{"type":"bridge_start","schema":1,"seq":1,"game_id":null}
{"type":"state","phase":"waiting_pairing","schema":1,"seq":2,"game_id":null}
{"type":"heartbeat","schema":1,"seq":3,"game_id":null}
```

対局中は `game_start`、`go`、`move`、`ponder`、`alert`、`game_end` が並びます。

`game_start` には初期 SFEN、対局者名、色、時間設定、入玉規則が入り、`move` には CSA と USI の両方の表記、server が課した時間、残り時間、手の由来が入ります。

この log は外部 consumer との追加専用契約です。

reader を作る場合は、未知の `type` を捨てずに保持してください。

## 生死を読む

`heartbeat` は 60 秒ごとの固定 pulse ではありません。

直近 60 秒に別の record がなければ 1 行を書き、指し手や探索 event が流れている間は書きません。

したがって heartbeat がないことだけから、bridge が停止した瞬間は決められません。

`bridge_stop` は正常に shutdown を完了したときに最終行へ書かれます。

最終行が `bridge_stop` なら、その event log は完結しています。

`bridge_stop` がない log は、power loss や強制終了を含め、完結を確認できません。

長考や reader 側の poll delay もあるため、heartbeat の間隔だけで直ちに死と分類しないでください。

## wire と engine log

wire log には送受信方向と monotonic timestamp を付け、空行 keepalive も残します。

password の secret は wire log で伏せます。

engine log は raw USI stdout なので、`info` の大量出力は JSONL へ混ぜません。

対局結果の解釈は event log で行い、protocol の一行単位の調査は wire log、engine の沈黙や不正な応答の調査は engine log から始めます。
