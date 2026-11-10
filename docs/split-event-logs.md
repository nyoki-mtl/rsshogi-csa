# セッション共通ログと対局別events

0.3.0で追加した出力契約である。
既存のrunログを移動、短縮、上書きせず、bridgeが別の子ディレクトリへ出力する。
operatorとShogiLensの既存読取りや`/live`の形式選択は変更しない。

## 配置と形式の選択

```text
logs/
  <run>-events.jsonl
  <run>-wire.log
  <run>-engine.log
  <run>-probe.jsonl
  <run>-split/
    manifest.json
    session.jsonl
    games.jsonl
    games/
      0-<game_start_seq>.jsonl
      1-<game_start_seq>.jsonl
```

CLIは追加の引数なしで併記する。
旧consumerは従来どおり直下の`*-events.jsonl`を読む。
分割側は専用の子ディレクトリに置くため、旧形式のrunを重複列挙しない。
engine、wire、probeの物理ファイルはrun共通のままである。
移行中は旧eventsと分割eventsを二重保存するため、総保存量の削減を目的とする機能ではない。

manifestは`format: "rsshogi-csa-split-events"`、`version: 1`で形式を識別する。
これはevent envelopeの`schema: 1`とは別のversionである。
run_idは文字列で、session_events、games_index、legacy_events、engine_log、probe_logはmanifestのディレクトリからの相対pathを持つ。
probeは損失許容のため、manifestにpathがあっても作成失敗や欠落はあり得る。
manifestはsessionと一覧のファイル作成後に新規作成する。
manifestが存在しない、未完、未知のformat/versionの場合は、分割形式を有効と推測しない。

新consumerは分割形式を明示的に選ぶ。
同じrunの旧eventsと分割eventsを混ぜて再生したり、不完全な分割出力を旧形式で黙って補完したりしない。
明示的に旧形式を選び直すことと、混合補完は区別する。

## セッションと対局の境界

session.jsonlには起動、engine世代、接続とLOGIN、対局待ち、heartbeat、対局外の診断、終了を残す。
対局別eventsにはgame_start、合意と開始の状態、途中局面の既着手、時計、着手、go/ponder、局内の診断、proposal_endまたはgame_endを残す。
途中局面の既着手はgame_start.moves_so_farをそのまま保持する。
元のeventsの各レコードは分割側のどちらか一方へ出し、envelopeも同じbytesで保持する。
終局後の遅延診断は、閉じたファイルへ追記せずsessionへ残す。
損失許容の探索途中情報と生USI出力は、従来のprobeとengineログを参照する。

eventsへ追加するtypeは次のとおり。

| type | 契約 |
| --- | --- |
| proposal_end | game_start_seq、proposal_game_id、reason、detailを持つ。game_start前ならseqはnull |
| engine_generation | spawn要求ごとに1から増えるgeneration。起動成功や旧child回収の証明ではない |
| connection | eventはconnected、login_accepted、login_rejected、disconnected |

proposal_end.reasonはvalidation_rejected、stop_requested、rejected、disconnected、session_shutdownを区別する。
detailは診断文であり、状態判定に使わない。
接続とengine世代のenvelope.game_idはnullで、対局外の事実として記録する。
既存レコードのfieldや意味、event schemaは変更しない。
旧consumerには未知typeを保持する既存契約が引き続き必要である。

## 一覧と識別子

games.jsonlは追記専用の状態snapshot列である。
`type: "game"`はGameEntry、`type: "diagnostic"`は対局エントリを作れなかった診断を表す。
同じキーについて、ファイル上で最後にある完全なsnapshotを使う。
完結通知と直前の結果通知は同じend_event_seqになるため、seqだけで後者を捨てない。

| GameEntryのfield | 意味 |
| --- | --- |
| run_id / game_start_seq | 対局提案の一意キー。対局IDをキーにしない |
| game_index | 正常に記録した全game_startを0から数える。REJECTも数える |
| game_id | 提案時のID。STARTで受けたIDが異なる場合は開始snapshotで更新する |
| file | 一覧のディレクトリからの相対path。IDはpathに使わない |
| proposal_state | proposed、started、rejected、interrupted_before_start、interrupted、finished |
| result | START到達後のgame_endの結果。それ以外はnull |
| end_reason / end_event_seq | 明示的な終了原因と、その元eventsのseq |
| last_event_seq | このsnapshotが確認した最後の対局eventsのseq。現在のファイル末尾とは限らない |
| file_complete | writerが追記先を切り替えて閉じたという通知。結果確定とは独立 |

end_reasonはgame_end、またはproposal_end.reasonに対応する。
START前に終局通知だけが来た場合はinterrupted_before_startとし、対局結果を捏造しない。
game_start前の拒否や中断診断はgame_indexとgame_start_seqがnullで、event_seqを診断の識別に使う。
検証拒否は対局添字を消費せず、存在しない開始seqを補わない。
一覧上の絶対添字は、旧`/live`のgame_startの数え方に合わせる。

## 順序と完結

対局ファイルはcreate_newで作り、最初のgame_startをflushしてから一覧に載せる。
終端レコードでは次の順序を守る。

1. 対局eventsへgame_endまたはproposal_endを追記してflushする。
2. 結果や終了理由をfile_complete=falseの一覧snapshotとして追記する。
3. writerがその対局への追記先を外し、flushしてファイルを閉じる。
4. file_complete=trueのsnapshotを一覧へ追記してflushする。

結果が存在しても、4の通知がなければ完結済みと推測しない。
ファイルは閉じられたが4の書込みに失敗した場合も、consumerから見た状態は未完結となる。
engine/probeの排出完了はこの通知に含まない。

eventsのseqはrun内の原採番を維持し、対局ごとに1へ戻さない。
probe_seqはprobe内の順序と欠落検出専用である。
probeのgame_start_seq（文字列表現）、search_id、engine_generationで関連付ける。
異なるstreamのseqや時刻の大小だけから完全な前後関係を保証しない。

## 非同期処理と障害

旧run writerは従来どおり独立taskで動く。
旧eventsへのwrite/flush後、同じenvelopeを128件の有界キューへ待たずに投入し、別taskが分割出力する。
分割側のfile open、write、一覧更新、キュー満杯の失敗で、旧run writerを終了させない。
独立した障害channelとstderrで通知し、現在局は切断せず、次局受付を抑止して最終終了コードを非ゼロにする。
初期のmanifest等を作れない場合はengineや接続を開始しない。

終局と提案終了後は、一覧の完結通知がflushされたというwriter応答を最大5秒待ってから次局を受け付ける。
待機は対局間の状態であり、coreの期限処理、keepalive、engine排出をブロックしない。
先に届いた次summaryは保持し、完結応答前にgame_startやAGREEを出さない。
遅い応答や誤ったseqの応答で古いゲートを解除しない。
確認できない場合は次局を開始せず終了する。
確認期限による受付終了とI/O失敗は区別し、終了時のdrainが正常なら前者だけで終了コードを非ゼロにはしない。
独自のsans-I/O consumerが同じ永続化契約を使う場合は、SessionCore::enable_log_completion_barrierを有効にし、Event::LogCommittedで終端source seqを返す。
CLIのshellはこれを常に有効にする。

分割writerの終了時drainは最大5秒で打ち切り、未確認のファイルを完結済みにはしない。
通常ログの元キュー全体の有界化は今回の対象外である。
engineログとprobeの障害をcriticalなevents/一覧の障害とは混同しない。

## クラッシュと回収範囲

プロセスクラッシュ後は、改行まで完成したJSONLの有効な接頭辞を回収対象とする。
read_index_prefixは完全行をparseし、未完の末尾をtrailing_bytesとして返す。
破損した完全行はエラーとし、読み飛ばして後続を採用しない。
結果あり、未完行あり、完結通知なしはそれぞれ区別する。
ファイル作成後で一覧登録前に落ちた場合は、孤立ファイルが残ることがあるが、一覧に載せた対局と偽らない。

flushはfsyncではない。
電断後のファイルとディレクトリの整合、OSキャッシュの永続化、一覧の自動修復は保証しない。
取得対象を一覧で特定した後の対局再生は対象eventsの大きさに比例するが、一覧自体の読取りや共有増分読取は別の処理である。
