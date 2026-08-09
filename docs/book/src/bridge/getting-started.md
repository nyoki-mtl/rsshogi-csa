# 接続前の確認

CSA サーバーでは、接続できたことだけでは対局できません。

ログイン後に届く `Game_Summary` を読めなければ、bridge は対局を受理せず `REJECT` します。

先にエンジン、資格情報、記録先を揃えてから接続してください。

## 必要なもの

- USI プロトコルを話すエンジン実行ファイル
- 接続先から発行されたログイン名と password
- 接続先の host と port
- JSONL、wire、engine の 3 種類のログを書けるディレクトリ

`csabridge` は、起動時にエンジンへ `usi`、指定した `setoption`、`isready` を送ります。

GPU を初期化するエンジンではこの準備に時間がかかることがあります。

対局提案の後に暖機を始めると合意期限に間に合わないため、bridge は TCP 接続より先にエンジンを準備します。

## 資格情報を渡す

password は、次の三つのうち一つだけを指定します。

| 方法 | 指定 | 使いどころ |
| --- | --- | --- |
| 環境変数 | `--password-env NAME` | PowerShell のその session だけで渡すとき |
| ファイル | `--password-file PATH` | 定常運用で process arguments に値を残したくないとき |
| 直接指定 | `--password VALUE` | 一時的な確認だけをするとき |

`--password-file` は UTF-8 のファイルを読み、末尾の CR/LF だけを取り除きます。

環境変数とファイルなら、PowerShell history や process list に secret が出る経路を減らせます。

wire log では secret を `*****` に置き換えますが、shell history に入力した値までは消せません。

## 接続先を置き換える

既定の接続先は `wdoor.c.u-tokyo.ac.jp:4081` です。

別の CSA サーバーへ接続するときは、主催者から指定された値を `--host` と `--port` に渡します。

```powershell
cargo run -p rsshogi-csa-bridge --release --bin csabridge -- `
  --host 'server.example.jp' --port 4081 `
  --name 'assigned-name' --password-env RSSHOGI_CSA_PASSWORD `
  --engine 'C:\engines\engine.exe'
```

この例は接続先の資格情報と server mode の CSA protocol を前提にします。

大会やテストサーバーの account 発行、password の書式、許可された game name はここから推測しないでください。

公式 CSA protocol の標準ポートは 4081 ですが、実際に使う値は各運営の通知に従います。[^server-port]

[^server-port]: [CSA サーバプロトコル ver.1.2.1](https://www.computer-shogi.org/protocol/tcp_ip_server_121.html) の通信モデルを参照。
