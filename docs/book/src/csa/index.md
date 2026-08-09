# CSA インターフェース

CSA には、棋譜ファイル形式と対局サーバーの通信 protocol があります。

`rsshogi-csa` が扱うのは後者です。

`LOGIN`、`BEGIN Game_Summary`、着手、終局通知を parser と formatter の対象にします。

棋譜ファイルの読み書きは [`rsshogi`](https://crates.io/crates/rsshogi) の `records` feature が担当します。

この分離により、codec は `rsshogi`、非同期 runtime、socket に依存しません。

対局に必要な I/O、局面の検証、時計の再構成、USI への変換は `rsshogi-csa-bridge` が担当します。

CSA codec の API は [docs.rs](https://docs.rs/rsshogi-csa) を参照してください。
