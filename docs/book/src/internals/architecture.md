# bridge の設計

bridge は対局を止めないことを優先し、protocol の行、状態機械、socket I/O、観測を別の責務に分けます。

この章は現在の設計を要約します。

設計判断の経緯と比較は、repository の [architecture.md](https://github.com/nyoki-mtl/rsshogi-csa/blob/main/docs/architecture.md) に残しています。

## codec は行だけを扱う

`rsshogi-csa` は CSA の command model、parser、formatter を持ちます。

I/O、盤面、非同期 runtime、`rsshogi` への依存は持ちません。

指し手は構文検証済みの文字列として保持し、`BEGIN Position` の内容も生の行として保ちます。

この crate は、未知の追加 field を落とさず、parse と format の往復で保てる形にします。

盤面解釈と合法手の確認は bridge の端で行います。

## session core は sans-I/O である

session core は CSA 行、USI message、tick、EOF、接続喪失を event として受け、server 送信、engine 操作、deadline、log record を action として返します。

tokio の socket や process handle はこの core に入れません。

そのため、危険な組み合わせである session phase、時計、終局 burst は transcript を再生する test で確認できます。

shell は core の action を実行し、socket、engine、log writer を別 task として監督します。

## engine の判断と bridge の判断

engine は評価、読み筋、投了、勝利宣言を判断します。

bridge は server から継続して裏書きされる盤面、時計、接続、phase、自分が送った設定を判断材料にします。

engine が返した手へ介入できるのは、bridge 自身の状態に根拠があり、差し替える手が元の手より悪くならない場合だけです。

非合法な `bestmove` は送らず、最後の読み筋または policy に従って処理します。

`bestmove resign` と、検証できない `bestmove win` は engine の判断を置き換えず、必要なら alert として残します。

engine が期限までに答えない場合、bridge は最後の読み筋、宣言勝ち、投了の順に fallback します。

任意の合法手を作る経路は `--any-legal-on-panic` を明示したときだけです。

## 観戦 consumer は別プロセスに置く

bridge が持つのは対局と追加専用 JSONL event log までです。

観戦、盤面描画、棋譜変換、事後分析は event log を読む別 process または別 repository の責務です。

HTTP server や UI の panic、port bind failure、遅い reader を対局 process の障害経路にしないためです。

この境界を守るには、`record.rs` の JSONL schema を変更するとき、既存 wire line が byte-for-byte で read back できる contract test を通します。
