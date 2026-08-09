# CSA インターフェース

CSA には、棋譜ファイル形式と対局サーバーの通信 protocol があります。

`rsshogi-csa` が扱うのは後者です。

`LOGIN`、`BEGIN Game_Summary`、`AGREE`、着手、終局通知、未知の追加 field を command model として扱います。

棋譜ファイルの読み書きは [`rsshogi`](https://crates.io/crates/rsshogi) の `records` feature が担当します。

この分離により、codec は `rsshogi`、非同期 runtime、socket に依存しません。

対局に必要な I/O、局面の検証、時計の再構成、USI への変換は `rsshogi-csa-bridge` が担当します。

CSA codec の API は [docs.rs](https://docs.rs/rsshogi-csa) を参照してください。

protocol の実行時の流れは、[セッションとデータの流れ](../protocol/flow.md)を参照してください。

codec が保証するのは、行の parse と format です。

指し手が局面で合法か、proposal が対局可能か、どの deadline で engine を止めるかは bridge の責務です。
