# 引数と運用操作

最小の起動には `--name`、認証方法のいずれか一つ、`--engine` が必要です。

既定値を変える必要があるのは、接続先、エンジン、記録先、対局数が多くの場合です。

## 接続と認証

| 引数 | 既定値 | 説明 |
| --- | --- | --- |
| `--host HOST` | `wdoor.c.u-tokyo.ac.jp` | CSA サーバーの host |
| `--port PORT` | `4081` | CSA サーバーの TCP port |
| `--name NAME` | 必須 | `LOGIN` に使う名前 |
| `--password VALUE` | 必須の選択肢 | password を直接渡す |
| `--password-env NAME` | 必須の選択肢 | password を読む環境変数名 |
| `--password-file PATH` | 必須の選択肢 | password を読む UTF-8 file |

`--password`、`--password-env`、`--password-file` は併用できません。

## エンジン

| 引数 | 既定値 | 説明 |
| --- | --- | --- |
| `--engine PATH` | 必須 | USI engine executable |
| `--engine-dir PATH` | executable の directory | engine の working directory |
| `--engine-arg VALUE` | なし | engine へ渡す追加引数。繰り返し可 |
| `--setoption NAME=VALUE` | なし | `isready` 前に送る USI option。繰り返し可 |

同じ `Name` を複数回指定した場合、最後の指定だけをエンジンへ送ります。

`--setoption` の書式は `Name=Value` です。

`=` を含まない指定は起動時のエラーになります。

## 対局と記録

| 引数 | 既定値 | 説明 |
| --- | --- | --- |
| `--games N` | `1` | 対局数。`0` は停止まで継続 |
| `--log-dir PATH` | `logs` | event、wire、engine log の出力先 |
| `--no-score-comment` | off | floodgate 向け score comment を付けない |
| `--pv-comment-moves N` | `32` | score comment に載せる PV 手数。`0` は score のみ |
| `--turn +|-|*` | `*` | `%%GAME` の希望手番 |
| `--x1` | off | 接続を対局間で保つ x1 extension を要求 |
| `--game-name NAME` | なし | x1 の明示的な `%%GAME` に使う game name |

`--x1` と `--game-name` は通常の floodgate 接続では必要ありません。

未検証の server extension を使う場合は、大会前の接続試験で実際の server behavior を確認してください。

## 時間と障害時の詳細設定

次の引数は、橋渡しの policy を変えます。

通常の floodgate 運用では既定値から始め、通信環境と大会規程が分かったときだけ変更します。

| 引数 | 既定値 | 説明 |
| --- | --- | --- |
| `--margin-ms N` | `2500` | engine に申告する自分の時間から差し引く安全 margin |
| `--stop-grace-ms N` | `1000` | `stop` 後に `bestmove` を待つ時間 |
| `--usi-timeout-ms N` | `30000` | engineごとのusiok応答期限 |
| `--ready-timeout-ms N` | `300000` | GPU準備を含むreadyok応答期限 |
| `--login-timeout-ms N` | `30000` | LOGIN応答期限。成功後の組合せ待ちは無期限 |
| `--keepalive-ms N` | `40000` | 空行 keepalive の間隔。30 秒未満は 30 秒へ切り上げ |
| `--resign-cp N` | `0` | この score より良い投了を alert にする閾値 |
| `--max-degraded-moves N` | `2` | engine が答えられなかった連続手数の上限 |
| `--any-legal-on-panic` | off | fallback が尽きたとき投了でなく任意の合法手を指す |
| `--quit-grace-secs N` | `6` | `quit` 後に engine の終了を待つ時間 |

`--stop-grace-ms`は`--margin-ms`未満にする必要があります。
stopが遅れても、手番開始から計算したfallback期限は延長しません。
準備期限の3設定は正の整数で指定します。
接続試行は1回30秒、LOGIN成功まで最大3回です。
quitの書込み待ちも終了猶予に含みます。

`--x1 --game-name NAME`では、初回と継続する各局の終了後に一度ずつ登録します。
game nameなしでは大会側からのsummaryを待ち、追加登録しません。
非x1も接続維持で継続でき、正常終局後のEOFでは残り局数があれば再接続します。

### 持時間0秒のFischer対局

`--margin-ms`は、その手で実際に使える時間より小さくする必要があります。

たとえば`Time_Unit:1sec`、`Increment:2`で残り持時間が0秒なら、時間切れにならずに
記録できる消費時間は1秒までで、実時間の上限は1999msです。
既定の`--margin-ms 2500`では探索時間が残らず、engineへの`binc`も0になります。

この形式の大会では`--margin-ms`を2000未満に設定し、実際の接続先、network、engineで
余裕を確認してください。
その際は`--stop-grace-ms`もmargin未満に調整してください。
bridgeはmarginがその手の全予算を消費すると
`margin_exhausts_budget` alertを記録します。

`--resign-cp` は投了の可否を変えません。

engine が出した `bestmove resign` は送信し、score が説明しない場合だけ記録に alert を残します。

`--any-legal-on-panic` は engine が選んでいない手を指す opt-in です。

まず既定の投了 policy を使う方が、障害時に何が起きたかを追いやすくなります。
