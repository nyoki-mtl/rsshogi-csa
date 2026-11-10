# 探索文脈付きプローブ

bridgeは既存のevents、wire、engineログに加え、`<run>-probe.jsonl` を出力する。
これは読み取り専用の観戦consumerへ探索途中の情報を渡す損失許容ログである。
対局結果、確定時計、死活判定の正本は従来の `events.jsonl` とする。

## envelope

改行まで書き終わったJSONオブジェクトを1レコードとする。
`schema` は1、`type` はengine、search、info、bestmove、unattributedのいずれかである。
`run_id`、`probe_seq`、`engine_generation` はJSON文字列である。
`probe_seq` はrun内で増加し、キュー満杯やサイズ制限による省略も番号を消費する。
`at` は入力イベントのbridge開始からの単調ms、`ts` はwriterが付けるUNIX msであり、時刻順で探索を推測しない。

`engine` のeventはstarting、ready、exitedである。
`search` のeventはstarted、ponder_hit、stop_requested、discarded、endedであり、reasonで終了理由などを示す。
startedはpositionとgoの送信Action発行を表し、エンジンでの実行成功の証明ではない。
bestmoveは結果のkindをmove、resign、winに分け、moveはusi、先読み候補はponderに保持する。
bestmoveをログへ記録しても、CSAへの採用を意味しない。
対応する探索がない出力はcontextをnullとしたunattributedで示す。

## 探索文脈

探索に属する各レコードはcontextを毎回持ち、開始行を読むことに依存しない。
起動時のinfo stringなど探索外のinfoはcontextがnullになる。

| フィールド | 意味 |
|---|---|
| game_start_seq | 対応するeventsのgame_start.seqの文字列。対局ID再利用時も区別する |
| game_id | CSA対局ID。単独の一意キーには使わない |
| search_id | run内の探索開始ごとに増加する文字列ID |
| root_sfen / root_side | PVを再生する開始局面と、その手番blackまたはwhite |
| base_ply | 開始時の確定本譜の着手数。提案内再生手は含み、ponder予測手は含まない |
| predicted_usi | ponderで仮定した相手の1手。通常探索はnull |
| origin | normalまたはponder。探索終了まで変更しない |
| state | thinking、pondering、stopping、discarded、finished |
| ponder_outcome | null、pending、hit、miss、cancelled |

ponderのrootは確定本譜へ予測手を適用した仮想局面である。
ponderhitではID、root、base_plyを維持し、stateをthinking、outcomeをhitへ更新する。
ponder missは採用資格を失うためstateをdiscarded、outcomeをmissとし、遅延infoもその文脈で記録する。
通常探索の時間上限によるstopはstoppingであり、猶予内の応答を引き続き採用できる。
終局、投了、fallbackで失効した探索も返答待ちの文脈を保持する。
bestmove排出またはエンジン交換まで同じプロセスへ次のgoを送らない。
交換中も元の手番の期限を維持し、再開時には経過分を本時間、足りない分を秒読みまたは自分側の加算から差し引く。
この補正は既存ClockLedgerのUSI予算生成を利用し、停止期限を先延ばしにしない。
stop_graceでの交換は応答がない状態への回復方針であり、OSがプロセスの死亡を確認したことは意味しない。
エンジン、スレッド数、GPU、CPU負荷の条件を変える場合はstopからbestmoveまでの時間を測り直し、猶予と交換方針を再評価する。

USIのinfo自体には探索IDがないため、bestmove後も旧探索のinfoを送る非準拠エンジンの由来を完全に識別することはできない。
既知の世代差や返答待ちを推測で最新探索へ割り当てることはしない。
rootだけでは反復などの履歴全体を表せないため、PV表示用の局面を終局裁定の根拠にしない。

## 構造化info

infoには存在したフィールドだけを記録する。
depth、seldepth、time_ms、nodes、multipv、hashfull、nps、currmove、string、pvを保持する。
未知のUSIフィールドは既存engine.logに残す。
欠損値は0で補わず、別探索へ直前の値を引き継がない。

scoreは `{kind:"cp",value:32,bound:"exact",perspective:"root_side"}` の形である。
boundはexact、lower、upperを区別する。
mateのvalueは `{kind:"plies",value:-5}`、距離不明の場合は `{kind:"unknown_distance",sign:"positive"}` またはnegativeとする。
符号付き距離、0、距離不明を巨大cpや整数の最大値へ潰さない。
通常のMultiPV番号がないPV情報はconsumerが1として扱い、stringのみのinfoで思考表の数値を更新しない。

## 資源と障害

probe専用の256件の有界キューへtry_sendし、ディスクやconsumerを待たない。
1行64KiB、PV512手、string4KiBに制限し、項目を切り詰めた場合はtruncated_fieldsへ記録する。
巨大な不正bestmoveは複製せず、サイズ制限のunattributedへ置き換える。
キューやサイズの上限で省略された行はprobe_seqの欠番として後続から判定できる。
最終行以降の欠落は完全に観測できないため、監査用途の無欠落保証はない。

作成失敗やI/O失敗はstderrへ通知し、対局接続、探索、既存events writerの障害方針へ波及させない。
終了時のprobe writer待ちは最大1秒とする。
Unixでは0640で作成し、設定された秘密値はstring値から伏せ字にする。
実行中のtruncateやローテーションを行わず、run単位で保持する。
初期実装ではengine.logとinfoを二重記録するため、ディスク監視は維持する。
consumer移行後の生info削減は別途判断する。
