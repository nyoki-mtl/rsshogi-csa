# rsshogi-csa

`rsshogi-csa` は、floodgate、世界コンピュータ将棋選手権、電竜戦などのオンライン CSA 対局サーバーで、USI エンジンを対局させるための Rust workspace です。

対局クライアントは、CSA サーバーとの通信、時間管理、USI エンジンの監督を同時に扱います。

この workspace は、その境界を 2 つの crate に分けます。

- **`rsshogi-csa`**：CSA protocol の 1 行を型付き command model と相互変換する codec です。
- **`rsshogi-csa-bridge`**：CSA サーバーへ接続し、USI エンジンを起動して対局を監督する bridge です。

codec は I/O と盤面を持ちません。

bridge は codec を使い、対局に必要な session、時計、エンジンの状態を保持します。

観戦と事後検証は bridge と別のプロセスで扱います。

bridge は追加専用の JSONL イベントログを出力します。
