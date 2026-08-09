# rsshogi-csa

CSA プロトコルサーバーと CUI だけで対局するための Rust workspace です。

floodgateのようなヘッドレス環境で、
GUI やブリッジ用 GUI を挟まずに USI エンジンを直接参加させることを目的にしています。

## 0.1.1 の対応範囲

| 対象 | 状態 |
| --- | --- |
| floodgate（通常接続） | 正式対応 |
| x1連続接続、公式test server | experimental |
| 世界コンピュータ将棋選手権、電竜戦 | experimental |
| buoy、駒落ち | experimental |

experimentalは実装経路またはcodecを含みますが、0.1.1の相互運用保証には含みません。
時間無制限の対局条件はUSIへ安全に写せないため、0.1.1では`REJECT`します。

## Crates

| crate | 役割 | 状態 |
| --- | --- | --- |
| `rsshogi-csa` | CSA の行コーデック。command model / parser / formatter | 実装済み |
| `rsshogi-csa-bridge` | session 状態機械、時間台帳、USI エンジン監督、CLI | 実装済み、floodgate で ponder と連続対局を実測 |

## `rsshogi-csa`

CSA プロトコルの 1 行を型付きの `CsaCommand` に相互変換します。

```toml
[dependencies]
rsshogi-csa = "0.1.1"
```

- runtime dependency はありません
- I/O も盤面も持ちません。socket も時計も上位層の責務です
- 指し手は構文検証だけを行う文字列型で保持します
- `BEGIN Position` ブロックの中身は生の行のまま保持します

寛容なパースと正規形の往復検証を分けています。

```rust
use rsshogi_csa::{format_command, parse_line, parse_line_strict};

let command = parse_line("+7776FU,T12").unwrap();
assert_eq!(format_command(&command), "+7776FU,T12");

// 正規形と一致しない入力は strict では弾かれる
assert!(parse_line_strict("AGREE   game-1").is_err());
```

未知のトークンは捨てずに保持し、再フォーマットでそのまま出力します。
サーバー方言の差はこの経路で吸収し、typed variant を増やして追随しません。

### 棋譜形式との違い

CSA には「棋譜ファイル形式」と「通信プロトコル」の 2 つがあります。

- 棋譜ファイル形式（`V2.2` / `V3.0` ヘッダ、`$EVENT`、`%TORYO` などを含む `.csa` ファイル）は
  [`rsshogi`](https://crates.io/crates/rsshogi) の `records` feature が担当します
- 通信プロトコル（`LOGIN`、`BEGIN Game_Summary`、`+7776FU,T12`、`#WIN`）はこの crate が担当します

両者は文法が一部重なりますが、別物です。

## `rsshogi-csa-bridge`

CSA サーバーと USI エンジンの間に立つ層です。I/O を持たない部分が入っています。

CSA サーバーの時間計算は決定的な整数演算で、各手の消費時間が `,T` として両者に返ります。
そのため残り時間は推定する必要がなく、サーバーの台帳をそのまま再構成できます。
自分の手番が来た時点では、自分の手のエコーも相手の手も届いているので、在庫は常に確定しています。

```rust
use rsshogi_csa::{Color, TimeConfig, TimeUnit};
use rsshogi_csa_bridge::{ClockLedger, ClockRules, MoveBudget};

// floodgate-600-10F 相当の時間設定
let control = TimeConfig::new(TimeUnit::SECOND).with_total_time(600).with_increment(10);
let rules = ClockRules::from_time_config(&control);
let mut ledger = ClockLedger::new(rules.clone(), rules);

// 11 秒使った手のエコー `+7776FU,T11` を適用する
ledger.apply_move(Color::Black, 11);
assert_eq!(ledger.remaining_units(Color::Black), 599);

// 時間切れにならない消費量の上限
assert_eq!(ledger.budget(Color::Black), MoveBudget::Units(608));

// USI の go パラメータ。安全マージンは自分側からだけ引く
let times = ledger.usi_times(Color::Black, 2_500);
assert_eq!(times.btime_ms, 596_500);
assert_eq!(times.wtime_ms, 600_000);
assert_eq!(times.binc_ms, Some(10_000));
```

相手の手番での探索（ponder）のスイッチは `--setoption USI_Ponder` だけです。
ブリッジ側に別のスイッチはありません。指定しなければエンジンの既定に従います。

`go ponder` と `ponderhit` と `stop` を送るのはブリッジの仕事ですが、
その意思表示は `USI_Ponder` 一箇所に置きます。
ただし判定はブリッジ側で行います。
YaneuraOu v9.40 は `setoption name USI_Ponder value false` を受けた後でも
`bestmove 2g2f ponder 8c8d` を返すため（実測）、
エンジンがこの設定を守ることを当てにできないからです。

予測が当たれば同じ探索がそのまま自分の手番の探索になり、外せば `stop` を送って捨てます。
`bestmove` にはどの `go` への応答かを示す印がないため、
捨てた探索が `bestmove` を返すまで次の `go` は出しません。
その待ち時間を縛るのは stop grace ではなく本来の着手期限です。
時計は満額残っているので、解放が少し遅いだけのエンジンを投了させる理由がありません。

`byoyomi` と `binc` / `winc` は USI で併用できないため、宣言に応じて排他的に選びます。
CSA の増秒は前払いですが、残り時間はサーバーの `mytime` をそのまま持つ表現にしてあるので、
USI の後払い規約とそのまま対応し、変換に補正が要りません。

## 使い方

最短の起動方法は`--password`です。

```powershell
cargo run -p rsshogi-csa-bridge --release --bin csabridge -- --name myengine --password 'floodgate-300-10F,secret' --engine C:\path\to\engine.exe --games 1
```

shell historyやprocess argumentsへ値を残したくない場合は、環境変数またはfileを選べます。

```powershell
$env:RSSHOGI_CSA_PASSWORD = 'floodgate-300-10F,secret'
cargo run -p rsshogi-csa-bridge --release --bin csabridge -- --name myengine --password-env RSSHOGI_CSA_PASSWORD --engine C:\path\to\engine.exe --games 1
```

ファイルで渡す場合はUTF-8ファイルを用意します。末尾のCR/LFだけが除去されます。

```powershell
cargo run -p rsshogi-csa-bridge --release --bin csabridge -- --name myengine --password-file C:\secure\floodgate-password.txt --engine C:\path\to\engine.exe --games 1
```

エンジンの設定は `--setoption Name=Value` で渡します。繰り返し指定できます。

実行中は stdin で `resign` / `stop` / `quit` がオペレータ操作になります。

ログは `--log-dir`（既定 `logs/`）に 3 本出ます。

- `*-events.jsonl`: 対局の意味的な出来事。観戦と事後検証の両方に使う
- `*-wire.log`: login secretを伏せたサーバーとの送受信。大会での証跡
- `*-engine.log`: エンジンの stdout / stderr

自分の手には `,'* <評価値> <読み筋...>` を付けます（`--no-score-comment` で無効化）。
読み筋はその手より先の継続手を CSA 表記で並べたもので、floodgate が記録して棋譜ページに表示します。
本数は `--pv-comment-moves` で変えられます。

イベントログのレコード種別は `bridge_start` / `engine_meta` / `state` / `heartbeat` /
`game_start` / `move` / `go` / `ponder` / `game_end` / `alert` / `bridge_stop` です。

## 観戦と事後検証

このリポジトリは観戦 UI を持ちません。
`*-events.jsonl` を読むのは ShogiArena の仕事です。

イベントログは追加専用の契約として公開してあります。
新しいフィールドは足しますが、既存フィールドの意味も型も変えません。
`crates/rsshogi-csa-bridge/src/record.rs` の `wire_lines_round_trip_byte_for_byte` が
その保証をバイト単位で固定しています。

観戦 UI を対局マシン上で動かす場合は、loopback に bind したうえで
手元から ssh でポートを引きます。対局プロセスの外に何も出しません。

```bash
ssh -N -L 7777:127.0.0.1:7777 user@your-instance
```

経緯は [`docs/architecture.md`](docs/architecture.md) の 0011 / 0012 にあります。

## 配布物

`rsshogi-csa` codecはcrates.ioとGitHub source archiveで配布します。
`rsshogi-csa-bridge`はcrates.ioへ公開せず、Windows x86_64向けbinaryをGitHub Releaseで配布します。

tag `v*` のrelease workflowは次の2ファイルを生成します。

- `rsshogi-csa-<tag>-windows-x86_64.zip`: `csabridge.exe`、`README.md`、`LICENSE`
- 同zipの`.sha256`: SHA-256とファイル名

展開後は`csabridge.exe --version`で版を確認できます。

## 関連 crate

- [`rsshogi`](https://crates.io/crates/rsshogi): 盤面、合法手生成、宣言勝ち判定、CSA / KIF 棋譜形式
- [`rsshogi-usi`](https://crates.io/crates/rsshogi-usi): USI の行コーデック

## Development

```bash
just lint
just test
```

設計判断は [`docs/architecture.md`](docs/architecture.md) にあります。

## License

MIT
