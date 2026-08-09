# 参照資料

## CSA protocol

- [CSA サーバプロトコル ver.1.2.1](https://www.computer-shogi.org/protocol/tcp_ip_server_121.html)：行の書式、対局条件、時計、着手、終局、keepalive の一次資料。
- [世界コンピュータ将棋選手権出場クライアントに対応していただくこと ver.1.2.1](https://www.computer-shogi.org/protocol/tcp_ip_client_121.html)：大会 LAN における client の必須要件と運用上の注意。

本文書は protocol の用語と標準的な流れを説明する出典です。

bridge が受理する server behavior は、実際の `shogi-server` 実装を正本にします。

たとえば公式文書には login password の長さが書かれていますが、floodgate のゲーム名付き password はその範囲を超え得るため、bridge は prose の長さ制限で拒否しません。

## この repository の資料

- [Rust API reference](https://docs.rs/rsshogi-csa)：codec の公開型と関数。
- [設計判断の記録](https://github.com/nyoki-mtl/rsshogi-csa/blob/main/docs/architecture.md)：責務境界、時計、ponder、観測の詳細な理由。
- [CHANGELOG](https://github.com/nyoki-mtl/rsshogi-csa/blob/main/CHANGELOG.md)：release ごとの利用者に見える変更。
