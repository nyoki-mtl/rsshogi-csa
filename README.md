# rsshogi-csa

[crates.io](https://crates.io/crates/rsshogi-csa) |
[docs.rs](https://docs.rs/rsshogi-csa) |
[Documentation](https://nyoki-mtl.github.io/rsshogi-csa/) |
[MIT License](LICENSE)

`rsshogi-csa` は、floodgate、世界コンピュータ将棋選手権、電竜戦などのオンライン CSA 対局サーバーへ USI エンジンを参加させるための Rust workspace です。

- `rsshogi-csa` は CSA protocol の command model、parser、formatter を提供します。
- `rsshogi-csa-bridge` は USI エンジンを起動し、CSA サーバーとの対局を監督します。

## floodgate への参加

次の例は、環境変数でログイン文字列を渡して 1 局だけ参加します。

```powershell
$env:RSSHOGI_CSA_PASSWORD = 'floodgate-300-10F,secret'
cargo run -p rsshogi-csa-bridge --release --bin csabridge -- --name myengine --password-env RSSHOGI_CSA_PASSWORD --engine 'C:\path\to\engine.exe' --games 1
```

`myengine`、`secret`、エンジンのパスは利用する環境に合わせて置き換えてください。

## ShogiArena で観戦する

bridge が `logs` に書くイベントログは、[ShogiArena](https://github.com/nyoki-mtl/ShogiArena) 1.2.6 以降で観戦できます。

bridge とは別のターミナルで ShogiArena をインストールし、同じログディレクトリを指定します。

```powershell
python -m pip install --upgrade 'shogiarena>=1.2.6'
shogiarena dashboard watch --csa-log-dir .\logs --port 7777
```

コマンドが表示した URL をブラウザで開くと、進行中の盤面、対局一覧、bridge の稼働状態を確認できます。

複数の bridge をまとめて監視する方法と、停止後に対局を再表示する方法は、[ShogiArena で対局を観戦する](https://nyoki-mtl.github.io/rsshogi-csa/bridge/shogiarena.html)を参照してください。

## ドキュメント

- **[ガイド / mdBook](https://nyoki-mtl.github.io/rsshogi-csa/)**：workspace の目的と各 crate の役割
- **[Rust API リファレンス](https://docs.rs/rsshogi-csa)**：CSA codec の型と関数
- **[対局期限と接続の契約](docs/bridge-reliability.md)**：停止猶予、準備期限、次局登録、operatorの移行条件
- **[分割eventsの契約](docs/split-event-logs.md)**：0.3.0向けのセッション共通ログ、対局別eventsと一覧

## ライセンス

MIT License
