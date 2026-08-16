# floodgate で 1 局指す

floodgate では password が `ゲーム名,secret` の形を取ります。

bridge はこの値を `LOGIN` の password field として送り、wire log では comma の後ろの secret を伏せます。

次の PowerShell 例は、1 局だけ指して終了します。

```powershell
$env:RSSHOGI_CSA_PASSWORD = 'floodgate-300-10F,secret'

cargo run -p rsshogi-csa-bridge --release --bin csabridge -- `
  --name 'myengine' `
  --password-env RSSHOGI_CSA_PASSWORD `
  --engine 'C:\path\to\engine.exe' `
  --games 1
```

`myengine`、ゲーム名、secret、エンジンのパスは、手元の設定へ置き換えます。

ログは既定でカレントディレクトリの `logs` に作られます。

`cargo run` での build 時間を避けたい運用では、あらかじめ release binary を build してから `csabridge.exe` を同じ引数で起動できます。

## エンジンへ渡す設定

`--setoption Name=Value` は繰り返し指定できます。

```powershell
--setoption 'Threads=4' `
--setoption 'USI_Hash=1024' `
--setoption 'USI_Ponder=true'
```

ponder の on/off は `USI_Ponder` だけで決めます。

bridge 固有の `--ponder` はありません。

エンジンが `USI_Ponder=false` 後にも予測手を返す実装であっても、bridge は自分が渡した `USI_Ponder` の値を見て ponder を開始しません。

engine executable の場所と作業ディレクトリが違うときは、`--engine-dir` を指定します。

引数を必要とする engine には、`--engine-arg VALUE` を引数ごとに繰り返します。

## 評価値の視点

USIの`info score`はengine側視点で、engine有利が正です。

Floodgateへ送るCSA score commentは先手固定視点へ変換し、先手有利を正、後手有利を負にします。

event JSONLのevalは、resign判定やengine logと同じUSIのengine側視点を維持します。

## 複数局を続ける

`--games` の既定値は `1` です。

`--games 0` は、operator が止めるまで対局を続けます。

```powershell
cargo run -p rsshogi-csa-bridge --release --bin csabridge -- `
  --name 'myengine' `
  --password-env RSSHOGI_CSA_PASSWORD `
  --engine 'C:\path\to\engine.exe' `
  --setoption 'USI_Ponder=true' `
  --games 0 `
  --log-dir 'D:\csa-runs'
```

対局中または待機中に標準入力へ `resign`、`stop`、`quit` と入力すると、operator command として扱います。

`resign` は対局中の投了を要求し、自分の手番になった時点で送信します。

`stop` は現在の対局を終えた後に停止し、`quit` は探索停止を要求して直ちに shutdown へ進みます。
