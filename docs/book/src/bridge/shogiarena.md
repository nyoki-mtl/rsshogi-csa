# ShogiArena で対局を観戦する

`csabridge` は、対局の状態を追加専用の JSONL イベントログへ書きます。

[ShogiArena](https://github.com/nyoki-mtl/ShogiArena) 1.2.6 以降は、そのログを別のプロセスで読み、ブラウザに盤面と稼働状態を表示できます。

観戦プロセスを分離しているため、ダッシュボードを停止しても bridge の対局は止まりません。

## ShogiArena をインストールする

ShogiArena は Python パッケージとして PyPI からインストールします。

```powershell
python -m pip install --upgrade 'shogiarena>=1.2.6'
shogiarena --version
```

`shogiarena --version` が `1.2.6` 以上を表示することを確認してください。

仮想環境を使う場合は、ダッシュボードを起動するターミナルでその環境を有効にします。

## ダッシュボードを起動する

bridge が既定の `logs` ディレクトリへ記録する場合は、別のターミナルで次のコマンドを実行します。

```powershell
shogiarena dashboard watch --csa-log-dir .\logs --port 7777
```

ShogiArena は、指定したディレクトリ直下にある既存の `*-events.jsonl` を読み、その後に追加されたイベントも追跡します。

起動後に表示される URL をブラウザで開いてください。

```text
CSA dashboard available at http://localhost:7777/index.html
```

ブラウザでは、コマンドが実際に表示した URL を使います。

bridge 側にも同じログディレクトリを指定します。

```powershell
$env:RSSHOGI_CSA_PASSWORD = 'floodgate-300-10F,secret'

cargo run -p rsshogi-csa-bridge --release --bin csabridge -- `
  --name 'myengine' `
  --password-env RSSHOGI_CSA_PASSWORD `
  --engine 'C:\path\to\engine.exe' `
  --games 1 `
  --log-dir '.\logs'
```

ダッシュボードは bridge より先に起動しても、対局開始後に起動しても構いません。

既存のイベントログも起動時に読み直すため、対局の途中から観戦を始められます。

## 画面の役割

CSA 用ダッシュボードには三つのタブがあります。

- **Live View**：進行中の対局を盤面と棋譜で表示します。
- **Games**：検出した対局を一局一行で表示します。Game を選ぶと Live View でその局面を開きます。
- **Monitor**：bridge run ごとの待機、対局中、停止、alert、最終更新を表示します。

ペアリング待ちの run は対局ではないため、Games には空の対局行を作りません。

その状態は Monitor で確認できます。

## 複数の bridge をまとめて監視する

複数の bridge に同じ `--log-dir` を指定しても、各プロセスは固有の run ID を持つ別のイベントログを作ります。

ShogiArena はディレクトリ直下の全イベントログを読み、各対局を Games に、各 run を Monitor に表示します。

```powershell
shogiarena dashboard watch --csa-log-dir 'D:\csa-runs' --port 7777
```

サブディレクトリは再帰的に探索しません。

複数のログディレクトリを監視したい場合は、ディレクトリごとに別の ShogiArena プロセスと port を使います。

## 停止後に対局を再表示する

終了した対局を後から開く場合は、ダッシュボードの出力先を明示しておくと再表示しやすくなります。

```powershell
shogiarena dashboard watch `
  --csa-log-dir '.\logs' `
  --out-run-dir '.\shogiarena-csa' `
  --port 7777
```

`Ctrl+C` でダッシュボードを停止した後は、同じ出力先を `dashboard serve` へ渡します。

```powershell
shogiarena dashboard serve --run-dir '.\shogiarena-csa' --port 7777
```

アーカイブで再表示できるのは、ShogiArena が `game.db` へ保存できた終局済みの対局です。

JSONL イベントログは正本として `--csa-log-dir` に残り、`game.db` は再閲覧用の派生データとして出力先に作られます。

## 表示されない場合

対局が表示されない場合は、次の項目を確認します。

- bridge の `--log-dir` と ShogiArena の `--csa-log-dir` が同じディレクトリを指しているか。
- 対象が `<run_id>-events.jsonl` であり、`wire.log` や `engine.log` ではないか。
- イベントログが指定ディレクトリの直下にあるか。
- ペアリング待ちを Games ではなく Monitor で確認しているか。
- ShogiArena が表示した URL と port をブラウザで開いているか。

イベントログ自体の読み方は、[ログと対局後の確認](./logs.md)を参照してください。

ShogiArena 側の詳しい仕様は、[CSA 対局ログの監視](https://nyoki-mtl.github.io/ShogiArena/user-guide/csa-watch.html)にあります。
