# rsshogi-csa

`rsshogi-csa` は、floodgate、世界コンピュータ将棋選手権、電竜戦などのオンライン CSA 対局サーバーで、USI エンジンを対局させるための Rust workspace です。

CSA の接続先は棋譜を受け取るだけではありません。

サーバーが対局条件を提示し、両者の合意後に時計を開始し、着手を検証して消費時間付きで送り返します。

bridge はこの往復を保ったまま、USI エンジンを動かし、あとから検証できる記録を残します。

この workspace は、その境界を 2 つの crate に分けます。

- **`rsshogi-csa`**：CSA protocol の 1 行を型付き command model と相互変換する codec です。
- **`rsshogi-csa-bridge`**：CSA サーバーへ接続し、USI エンジンを起動して対局を監督する bridge です。

codec は I/O と盤面を持ちません。

bridge は codec を使い、対局に必要な session、時計、エンジンの状態を保持します。

観戦と事後検証は bridge と別のプロセスで扱います。

bridge は追加専用の JSONL イベントログを出力します。

現行の正式対応範囲は floodgate の通常接続です。

世界コンピュータ将棋選手権、電竜戦、公式テストサーバー、`x1` 拡張には codec または接続経路があっても、実運用の確認を完了していません。

大会固有の接続先、資格情報、持時間、利用規約は主催者の案内を正本にしてください。

最初に接続するなら、[接続前の確認](./bridge/getting-started.md)から読みます。
