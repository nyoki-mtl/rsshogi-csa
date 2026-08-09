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

## ドキュメント

- **[ガイド / mdBook](https://nyoki-mtl.github.io/rsshogi-csa/)**：workspace の目的と各 crate の役割
- **[Rust API リファレンス](https://docs.rs/rsshogi-csa)**：CSA codec の型と関数

## ライセンス

MIT License
