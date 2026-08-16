# 時間管理と接続の規律

対局 client は、局面と時計を別々に正しく扱う必要があります。

時計を local timer の予測で上書きしたり、相手の手番に行を送ったりすると、server の裁定とずれます。

## 時計は server の echo から再構成する

CSA server は、client の行末 LF を受け取った時点までを消費時間として計測し、着手 echo の `,T` に付けます。[^server-time]

bridge は、この確定値を `ClockLedger` に適用します。

`Total_Time`、`Byoyomi`、`Increment`、`Delay`、`Least_Time_Per_Move`、`Time_Roundup` を決め打ちせず、proposal に従って台帳を更新します。

local clock は、今 engine に許す探索時間を決めるためだけに使います。

`--margin-ms` は、network 往復、server 側の丸め、`bestmove` から socket write までの遅れを見越して engine の申告時間を短くします。

CSA の increment は着手前に加算されます。

USI へ渡す increment は着手後の規約なので、bridge は表現を変換してから `go` に載せます。

指定局面や中断再開で`BEGIN Position`に着手履歴が含まれる場合も、各手の`,T`を
対局中のechoと同じ規則で台帳へ適用します。`T`とincrementが同値なら残り時間は変わらず、
incrementを超える`T`はその側の残り時間を減らします。電竜戦の持時間ハンデはこの性質を使うため、
着手履歴の`T`を読み飛ばしてはいけません。[^denryu-handicap]

[^server-time]: [CSA サーバプロトコル ver.1.2.1: 持時間と着手時刻](https://www.computer-shogi.org/protocol/tcp_ip_server_121.html)

[^denryu-handicap]: [指定局面とフィッシャー加算、持ち時間ハンデの調査](https://zenn.dev/keiishida/articles/denryu-buoy-increment-time)

## 送ってよい行を限定する

対局中に client が送る行は、自分の手番でだけ許されます。

相手の手番に送信すると、`#ILLEGAL_ACTION` による反則負けになり得ます。[^turn]

bridge は、server から自分の着手 echo が返るまで次の局面を確定しません。

ponder は相手の手番に探索する機能ですが、CSA server へ着手を送る機能ではありません。

予測が外れた ponder の `bestmove` は捨て、`ponderhit` を経た探索だけを着手に使います。

[^turn]: [CSA サーバプロトコル ver.1.2.1: 対局中の送信規則](https://www.computer-shogi.org/protocol/tcp_ip_server_121.html)

## keepalive と切断

CSA は対局中、空の行または LF だけを keepalive として送れます。

同じ client からの空行は、前回の送信から 30 秒以上空ける必要があります。[^keepalive]

bridge の既定値は 40 秒で、指定値が 30 秒未満でも protocol の下限へ切り上げます。

keepalive は JSONL の heartbeat とは別物です。

前者は server との TCP 接続を保つための空行であり、後者は外部 reader が bridge の沈黙を読めるようにする記録です。

対局中の TCP 切断は、再接続で対局を続ける経路ではありません。

socket、engine I/O、log writer を分け、socket task を log I/O で止めない構成は、この制約から来ています。

[^keepalive]: [CSA サーバプロトコル ver.1.2.1: keepalive](https://www.computer-shogi.org/protocol/tcp_ip_server_121.html)

## 大会へ持ち込む前に

世界コンピュータ将棋選手権向けの client 文書は、ログイン、対局通知への応答、着手の送受信、ログアウトを最低限の要件として挙げています。[^client-requirements]

同文書は、server が計測した時間を正規の時間として使い、明示的な `LOGOUT` を優先するよう求めています。

bridge はこの流れに従いますが、特定大会での正式運用は、主催者が提示する接続試験と規程を通して確認してください。

[^client-requirements]: [世界コンピュータ将棋選手権出場クライアントに対応していただくこと ver.1.2.1](https://www.computer-shogi.org/protocol/tcp_ip_client_121.html)
