# リファレンス

BonDriver と ini、`dt295` の各コマンド、デーモン (`dt295 serve`) の設定の説明。
導入の手順は [INSTALL.md](../INSTALL.md) にある。

## BonDriver (TVTest)

BonDriver は個体を自分では開かず、DLL のフォルダーの `dt295.exe` (`dt295 serve`) に選局を頼んで TS を受け取る。

- 応答する `dt295 serve` が無ければ、`OpenTuner` で BonDriver が起動する (ウィンドウは出ない)。
  最後の TVTest を閉じると、`[Server] IdleExitSec` 秒 (既定 5) 後に `dt295 serve` も終了する。
- 同じフォルダーの DLL (別の名前に複製したものも) は、どれも同じ `dt295 serve` を使う。
  どの DLL が起動したかによらず、関係は変わらない。
- `dt295 serve` は、DLL のフォルダーのファームウェアを使い、個体ごとの状態ファイル (`<シリアル番号>.state`) もそこに作る。
  ログは DLL のフォルダーの `dt295-serve.log` (1 MiB を超えたら、次に起動するときに `.log.1` に移す)。
- BonDriver と `dt295 serve` は `%TEMP%` のソケットでつながり、設定は要らない (下の "`dt295 serve`" セクション)
- 別の TVTest が同じ個体で視聴しているときは、選局は失敗する (視聴中の TVTest からは奪わない)。
  同じ TVTest のチャンネルの切り替えは、すぐに切り替わる。
- ストリームが止まったとき (`dt295 serve` が終了した、個体が抜かれた、録画ソフトに個体を取られた) は、同じチャンネルを頼み直す (間隔は 2 秒から 30 秒まで延びる)。
  個体を挿し直すと、TVTest を開いたままで映像が戻る。
- C/N (`GetSignalLevel`) は、`dt295 serve` に 1 秒ごとに問い合わせた値

### チャンネルとサービス

- 地デジは物理チャンネルごとに、その TS の全サービスを出力する。
  TVTest のサービスの切り替えで、サブチャンネルやワンセグも映る。
- BS / CS110 は ini の `[BS]` / `[CS110]` の行がチャンネルになる。
  `ServiceID` の無い行は TS の全サービスを出力し、TVTest のスキャンですべて登録される。
  `ServiceID` を書いた行はそのサービスのみ。
  見本の ini は 1 TS 1 行で、1 サービス 1 行の ini は `dt295 lineup --per-service` で作れる。
- CS110 はサービスごとに ECM が別で、ECM が 5 つ以上の TS (ND06 など) は、5 つ目の ECM からのサービスが 0.5 秒ずつ遅れて表示
- 映像の付いたデータ放送 (NHKデータ1 など) は、既定で無効の状態でスキャンに登録される。
  データのみのサービス (Gガイドなど) は TVTest の仕様で映らず、通常は登録もされない。
- 未契約の有料サービスもスキャンには載るが、映像は出ない (ログに `no video / audio in the stream although the card works`)
- 複数台のときは台数分 DLL と ini を複製し (`BonDriver_DT295-1.dll` など)、ini の `DeviceSerial` で個体を区別する。
  ログは DLL の名前で、状態ファイルは個体で分かれる。
- 同じ個体を別の名前の DLL (地デジ用と衛星用など) でも使える。
  状態ファイルは個体ごとなので、指定は要らない。
  同時に視聴できるのは、個体ごとに 1 つ。

### `BonDriver_DT295.ini`

DLL と同じフォルダーに、同じ名前 (拡張子だけ `.ini`) で置く。
無ければ既定値で動く。
相対パスは DLL のフォルダーが基準。
以前の版の `[Tuner] Firmware`, `TrcFirmware`, `StateFile`, `DeviceIndex` は読まない (残っていれば、その旨をログに出力)。
文字コードは UTF-8 (BOM の有無は問わない) か BOM 付きの UTF-16LE で、どちらでもなければ Shift_JIS として読む (Windows ではシステムの ANSI コードページ。Linux の `dt295` も同じく Shift_JIS)。
見本は `src/dt295-bondriver/BonDriver_DT295.ini`。

| キー | 既定 | 内容 |
|---|---|---|
| `[Tuner] RawTS` | 0 | `1` で raw-TS モード (完全に自前コードのみ)。レジデント (チップに常駐させる第三者のコード) も補助コア (demod を動かす副 CPU) も純正ファームも使わず、ホストがチップを起こし、ホスト面の I2C で選局し、取り込みエンジンを設定して自前の pump で配信する。スクランブルされた TS をそのまま出し、B-CAS の解除は TVTest の CAS プラグイン (PC 側) に任せる。`0` (既定) は DLL のフォルダーの純正ファームウェアで動かし、内蔵リーダーで解除する通常モード。最初の選局は cold (電源投入直後) の個体が必要。個体のモードは `dt295 serve` がその個体を開いたときの選局で決まり、開いている間は変わらない |
| `[Tuner] DeviceSerial` | なし | 使う個体の USB シリアル番号 (例 `CD23456789`。大文字小文字は区別しない)。無ければ、つながっている 1 台を使う (複数台のときは必要)。`dt295 list` で確認可能 (1 台ずつ挿して確認し、本体にラベルを貼っておくとよい)。デバイス マネージャーの「デバイス インスタンス パス」 (`USB\VID_06B8&PID_103B\<シリアル番号>`) の末尾でも可 |
| `[Tuner] UHF` / `CATV` | 1 / 1 | 地デジのチューニング空間 (UHF 13-62ch, CATV C13-C63) を表示するか |
| `[Tuner] ChannelFile` | なし | ほかの BonDriver のチャンネル定義ファイル。指定すると、そのファイルのチューニング空間のみを、そのドライバーと同じ番号で表示 (`UHF` / `CATV` と `[BS]` / `[CS110]` は使わない)。下の "ほかの BonDriver のチャンネル定義" セクション |
| `[Tuner] LogFile` | 0 | `1` で DLL と同じ名前の `.log` にログを出力。前回の分は `.log.1` に保存。ログは DebugView (`OutputDebugString`) にも常に出力。`dt295 serve` のログは、これによらず `dt295-serve.log` |
| `[Tuner] LogLevel` | `info` | ログの詳しさ: `error`, `warn`, `info`, `debug`, `trace` (大文字小文字は区別しない)。ほかの値なら `info` とし、その旨をログに出力。`debug` と `trace` は、BonDriver が起動する `dt295 serve` にも渡す |
| `[Server] IdleExitSec` | 5 | BonDriver が起動した `dt295 serve` が、クライアントがいなくなってから終了するまでの秒数。`0` で終了しない |
| `[Server] StartTimeoutSec` | 5 | 起動した `dt295 serve` が応答するまで待つ秒数 (すぐに終了したときは待たない) |
| `[BS]` / `[CS110]` | なし | `名前=周波数 (kHz),TSID[,ServiceID]` を 1 行に 1 チャンネル。周波数は IF (衛星の周波数 - 10678 MHz), TSID は放送の transport_stream_id (16 進は `0x` を付ける。7 以下なら TS 番号で、TSID の下位 3 ビットがその値の TS)。`ServiceID` を省くか -1 にすると、その TS の全サービス。1 行も無ければその空間は非表示 |

組み込みの周波数 (ISDB-T の中心周波数 = 帯域の中心 + 1/7 MHz。純正の表と同じ):
- UHF 13-62ch: 473.142857 + (n-13)*6 MHz
- CATV C13-C21: 111.142857 + (n-13)*6 MHz, C22: 167.142857 MHz, C23-C63: 225.142857 + (n-23)*6 MHz

CATV は、地デジを CATV の周波数に変えて流すケーブルテレビ (周波数変換パススルー) 向け。
UHF 51ch 以上 (700 MHz 以上。チューナーの設定がここで変わる) と CATV は、実機では確認していない。
ケーブルテレビ独自の ISDB-C (64QAM) は、DT295 の復調器が対応していないので受信できない。

`[BS]` / `[CS110]` の行は、状態ファイルに溜まった SDT から `dt295 lineup` で作り直せる。

raw-TS モード (`RawTS=1`) は、スクランブルされたままの TS を TVTest に渡し、TVTest 側の CAS プラグインで B-CAS を解除する方式。
ファームウェアは要らず、ホストがチップを cold から自力で立ち上げる (完全に自前コードのみ)。
最初の選局だけ cold の個体が必要で、そのあと TVTest を閉じて開き直すだけなら差し直しは要らない。
通常モード (System1, 内蔵リーダーで解除) と raw-TS モードは `RawTS` で切り替わる。
切り替えたら、TVTest をすべて閉じて `dt295 serve` が終了してから開き直す (raw-TS にするときは、個体の差し直しも要る)。
内部の仕組みと機能差は [raw-ts.md](raw-ts.md)。

### ほかの BonDriver のチャンネル定義

`[Tuner] ChannelFile` に、ほかの BonDriver のチャンネル定義ファイルを指定すると、そのまま読む。
チューニング空間とチャンネルの番号がそのドライバーと同じになるので、そのドライバーで作った TVTest の `.ch2` や EDCB のチャンネル設定をそのまま使える。
相対パスは DLL のフォルダーが基準 (`dt295` コマンドでは ini のフォルダー)。
文字コードは ini と同じ扱い (ただし ChSet.txt は、PT のドライバーと同じく、Windows でも ANSI コードページによらず Shift_JIS として読む)。
読めなければチューニング空間を 1 つも出さず、ログに理由を出力する。

| 書式 | ファイルの例 | 読む内容 |
|---|---|---|
| BonDriver_BDA | `BonDriver_BDA.ini` | `[TuningSpace00]` から `[TuningSpace99]` (`[TuningSpace00]` が無ければ `[Channel]` が空間 0) の `TuningSpaceName`, `CHnnn`, `ChannelSettingsAuto` と `ChannelSettingsAutoOptions`, `ChannelGenerateN`, `FrequencyOffset`, `ReserveUnusedCh`。衛星の局発周波数は `[Satellite]`、変調方式は `[Modulation]` と `[Tuner] DefaultNetwork` から |
| PT3-ST など (BonDriver_PTx-ST_mod, px4_drv の BonDriver_PX4 も) | `BonDriver_PT3-S.ChSet.txt` | `$` で始まる空間の行と、チャンネルの行 (空間, チャンネル, PTx のチャンネル, TSID)。BonDriver_PTx-ST_mod の `-` / `+` と、周波数の微調整 (`36-1` など) にも対応。地デジ用か衛星用かは、PT が DLL の名前で決めるのと同じく、ファイル名の最初の `-` (`BonDriver_PT3-` などの `-`) の次が `T` なら地デジ用 (`BonDriver_PT3-T.ChSet.txt` など)、それ以外は衛星用 |
| BonDriver_PX シリーズ | `BonDriver_PX_W3PE_S0.ini` | `[BS]` / `[CS110]` の `CHANNEL_NUM` と `CHnnn` (`名前,サービス ID,周波数 (kHz),TSID`) |

- 選局するのは ISDB-T と、ISDB-S (BS / CS110 の右旋) のチャンネルのみ。
  ほかの変調方式 (DVB-S, QAM など)、左旋、定義の無い番号は、番号がずれないよう一覧に残し、選ぶと失敗してログに理由を出力する。
- TSID が 7 以下なら TS 番号で、TSID の下位 3 ビットがその値の TS を選ぶ (BonDriver_PTx-ST_mod と同じ解釈。BonDriver_BDA の CS110 の見本の `0x0` もこれ)
- サービス ID の欄は使わず、TS の全サービスを出力
- `dt295 lineup` で作る `[BS]` / `[CS110]` の行は使われない

## `dt295` コマンド

| コマンド | 内容 |
|---|---|
| `dt295 list` | 接続中の個体の一覧 (番号, シリアル番号, 使用中かどうか) |
| `dt295 capture` | 個体を起動して選局し、TS をファイルか標準出力へ書き出し |
| `dt295 state show` | 状態ファイルの内容の表示 |
| `dt295 lineup` | 状態ファイルの SDT から ini の `[BS]` / `[CS110]` を再作成 |
| `dt295 logo` | 局ロゴの受信 |
| `dt295 mirakc-channels` | mirakc の設定のチャンネルの生成 |
| `dt295 driver` | (Windows) ドライバーの確認と、WinUSB / 純正の切り替え |
| `dt295 serve` | 個体を開いたままにするデーモン (Windows では BonDriver が起動する) |
| `dt295 tune` | デーモンに選局を頼み、TS を標準出力へ書き出し |
| `dt295 etw-urbs` | 開発用: 純正ドライバーの USB ETW をアクセスの一覧に変換 ([development.md](development.md)) |

オプションの一覧は `dt295 <コマンド> --help` で表示される。
ほかの OS 専用のコマンドとオプションはヘルプに表示されない。

### 共通事項

- 個体は `--serial <シリアル番号>` か `-i <番号>` (`dt295 list` の index) で選択
- `-v` で DEBUG, `-vv` で TRACE のログを標準エラー出力に出力する。
  環境変数 `RUST_LOG` があればそちらが優先。
- 失敗すると終了コード 1 (`dt295 lineup` のみ下のとおり)
- Linux でデーモンを使わずに個体を開くには、udev のルールが必要 ([INSTALL.md](../INSTALL.md) の手順 4)
- `dt295 serve` が開いている個体は開けない。
  Windows では、TVTest がその個体を選局してから、最後の TVTest を閉じて数秒たつまで。

### `dt295 list`

```
index  serial        status  location
0      CD23456789    free    \\?\usb#vid_06b8&pid_103b#cd23456789#{a5dcbf10-6530-11d2-901f-00c04fb951ed}
```

使用中の個体は `busy` と表示され、index は付かない (`-i` は開ける個体だけを数える)。
`dt295 serve` が開いている個体と、WinUSB になっていない個体 (Windows) も `busy`。

### `dt295 capture`

```
dt295 capture --fw mb86m01_assp_osec_idle.bin --trc mb86m01_assp_osec_trc_m_h.bin --ter 557142857 -t 10 -o ter.ts
dt295 capture --fw mb86m01_assp_osec_idle.bin --trc mb86m01_assp_osec_trc_m_h.bin --sat 1049480000 --tsid 4010 --sid 151 -o bs.ts
dt295 capture --bondriver BonDriver_DT295.ini --channel BS15/TS1 -t 0 > bs.ts
dt295 capture --raw-ts --serial CD23456789 --channel 27ch -t 60 -o raw.ts
```

- 地デジは `--ter <Hz>`, 衛星は `--sat <IF の Hz>` と `--tsid <16 進>`。
  `--tsid` を省くとその周波数の最初の TS、7 以下なら TS 番号 (TSID の下位 3 ビットがその値の TS)、`--sid` を省くとその TS の全サービス。
- `--all-services` は、`--channel` の行に `ServiceID` があっても TS の全サービスを出力
- `-t` は秒数 (既定 10)、`-t 0` は止めるまで続ける。
  どの方法で止めても、ストリームを停止し、リンク鍵を状態ファイルに保存してから終了する。
  SIGKILL で強制終了されても、個体に残ったストリームは次の起動時に停止される。
- `-o` を省くと標準出力へ書き出し (端末には出力しない)
- 出力先 (ファイルや読み手のパイプ) が数秒以上止まると、その間の TS は捨て、終了時に `bytes dropped` として表示する (止まった出力をストリームが待つと、どちらのモードでも TS が途切れるため)
- `--bondriver <ini>` は BonDriver の ini の個体 (`DeviceSerial`) と `RawTS` を使い、ファームウェアと状態ファイル (`<シリアル番号>.state`) は ini のフォルダーのものを使う (BonDriver が起動する `dt295 serve` と同じ)。
  コマンドラインでの指定が優先される。
  Linux でも同じ形式の ini を設定ファイルとして使える。
- `--raw-ts` は raw-TS モードで取り込む。
  純正ファームウェアも状態ファイルも使わず (`--fw`, `--trc`, `--state`, `--sid` は指定するとエラー)、スクランブルされた TS の全サービスをそのまま出力するので、B-CAS の解除は受け取る側で行う。
  `RawTS=1` の ini と、`-c` の設定で `raw-ts = true` の個体は、指定しなくても raw-TS モードになる。
  差したての個体の最初の選局はチップの立ち上げを含み、それ以降は別のプロセスからでも選局のみ (最初の TS まで 2 秒ほど)。
  差してから一度でも純正ファームウェアで起動した個体 (通常モードの TVTest や `dt295 capture` で使った個体) は、抜き差しするまで raw-TS で使えない。
- `--channel <名前>` は、`--bondriver` の ini のチャンネル (`[BS]` / `[CS110]` の行か `ChannelFile` のチャンネル) の名前 (例 `BS15/TS1`), 地デジの `27ch`, CATV の `C13ch`。
  大文字小文字は区別しない。
  `ChannelFile` の選局できないチャンネル (DVB-S や左旋など) の名前は、理由を表示してエラー。
- `--state <ファイル>` は、その状態ファイルのリンク鍵を使い、検証できた鍵を書き戻す。
  TVTest と交互に使うときは、`--bondriver` で BonDriver の ini を渡せば、同じ状態ファイルを使う (下の "困ったとき" セクション)。
- Linux でデーモンの個体を使うときは、デーモンを止めてから `-c /etc/dt295.conf` を指定 (リンク鍵が引き継がれる)
- 録画ソフトのチューナーのコマンドにもできるが、起動のたびに個体を初期化するので遅い。
  Linux では `dt295 serve` と `dt295 tune` を使う。

### `dt295 state show`

```
dt295 state show CD23456789.state
```

状態ファイルのリンク鍵と、衛星の PSI セクションをテーブルと TS の組ごとに表示する。
状態ファイルは `dt295 serve` が個体ごとに作る `<シリアル番号>.state` (Windows では BonDriver のフォルダー、Linux では `/var/lib/dt295`)。
以前の版の BonDriver の状態ファイル (`BonDriver_DT295.state`) も読める。

### `dt295 lineup`

```
dt295 lineup CD23456789.state                                          # [BS] / [CS110] の行を出力する
dt295 lineup CD23456789.state --bondriver BonDriver_DT295.ini          # ini との差分を表示する
dt295 lineup CD23456789.state --bondriver BonDriver_DT295.ini --write  # ini を書き換える (.bak を残す)
```

- 状態ファイルに溜まった SDT から、TV, 音声のみ, データ放送のサービスのある TS を 1 TS 1 行で出力する。
  `--per-service` を付けると、1 サービス 1 行 (`ServiceID` 付き)。
  SDT は TVTest で BS / CS を視聴したりスキャンしたりすると溜まるので、ひととおり視聴してから使う。
- 行の上のコメントのサービス名は、`--bondriver` の ini と同じ名前の `.ch2` (無ければ状態ファイルと同じ名前の `.ch2`) から取得 (`--ch2 <ファイル>` で指定、`--ch2 -` で使わない)
- 書き換えると空になるセクションがあるときは、`--write` でも書き換えない
- 終了コードは diff と同様で、0 は成功、1 は空になるセクションがある、2 は入力や ini の誤り

### `dt295 driver` (Windows)

純正ドライバーが入っている PC では、新しく挿した個体に純正ドライバー (`pxmb86m0x_usb`) が割り当てられる。
`dt295` は WinUSB で個体を開くので、個体ごとに一度切り替える:

```
dt295 driver status                 # 接続中の個体と、現在のドライバー
dt295 driver install                # すべての個体を WinUSB にする (--serial <シリアル番号> で 1 台だけ)
dt295 driver restore                # 純正ドライバーに戻す (StationTV X を使うとき)
```

- `install` と `restore` は管理者権限が必要で、UAC の確認の後、別のウィンドウで実行
- 切り替える個体は、TVTest などで開いていない状態が必要
- `install` がインストールするのは Windows 付属の WinUSB ドライバーで、独自の INF や証明書は使わない。
  Zadig でインストールした WinUSB でも同じように動作する。
- `restore` は PC のドライバーストアにある純正ドライバーを再インストールする。
  純正ドライバーが入っていない PC では、先に純正のドライバーパッケージをインストールしておく。
- `install` が途中で失敗したら、`restore` で純正ドライバーに戻してから再実行
- WinUSB で開けない個体は数えないので、一緒に挿していても問題なし

### `dt295 logo`

BS / CS の局ロゴは各チャンネルの放送には入っておらず、BS-15 のエンジニアリングサービス (サービス 929) が全局分を送出する。
送出のある時期は 20 分ほどおきに 1 分弱ずつ届くが、毎回全局分とは限らない。
そのため、TVTest で視聴しているだけではほとんど取得されない (BS-15 の TS1 の視聴中に取得できるかは確認していない)。
まったく送出されない時期もあり、時間帯からは予測できないので、`no complete logo data` で終わったら日を置いて実行する。
地デジのロゴは各局の CDT に入っているので、TVTest でその局を視聴していれば取得される。

#### TVTest への追加 (Windows)

```
dt295 logo --tvtest C:\TVTest                           # 受信して TVTest に追加する (最長 45 分)
dt295 logo --tvtest C:\TVTest --schedule                # 毎日 03:55 に実行する (--schedule 04:55 などで時刻を指定)
dt295 logo --unschedule                                 # 毎日の実行を解除する
```

空いている個体で 929 を選局し、ロゴがそろったら個体を解放して、TVTest のロゴファイル (`LogoData`, `LogoData.ini`, `Logo` フォルダー) に追加する。
書き換える前の `LogoData` と `LogoData.ini` は `.bak` に残る。

- 個体と `RawTS` は、TVTest の BonDriver のフォルダーにある `BonDriver_DT295*.ini` から読み取り、ファームウェアと状態ファイルはそのフォルダーのものを使う。
  TVTest が使っている個体 (`dt295 serve` が開いている個体) は使わない。
  `RawTS=1` の ini の個体は、BonDriver と同じく純正ファームウェアを使わずに raw-TS で受信する (状態ファイルも使わないので、そのまま raw-TS の TVTest で使える)。
  `DeviceSerial` の個体が空いていなければ、ほかの空いている個体を使う。
  `--serial <シリアル番号>` でその個体のみを使う。
- 待つのは最長 `--wait <分>` (既定 45) で、空いた個体が無ければ空くのを待ち、受信に失敗したら再試行
- 新しいロゴは、TVTest の次回の起動から表示される。
  TVTest を起動したまま実行してもよいが、TVTest の終了時に古いロゴへ戻ることがあるので、確実に反映するには TVTest を閉じてから実行する。
- `--schedule` のタスクはログオン中のユーザーで動作し、PC が停止していた日は次の起動時に実行される。
  結果は TVTest のフォルダーの `dt295_logo.log` に追記される。
  タスクには dt295.exe のパスが登録されるので、dt295.exe を使い続ける場所に置いてから登録する。
- ロゴはめったに変わらないので、たまの実行で十分
- `--dry-run` は書き込まずに結果のみ表示する。
  `--ts <ファイル>` は、929 を `dt295 capture` で録った TS から取り出す。

#### mirakc 向け (Linux)

mirakc は放送からロゴを取得せず、設定の `resource.logos` で指定した画像を配信する (EPGStation はそこから取得する)。
`dt295 logo --mirakc <フォルダー>` は受信したロゴを、サービスごとの PNG (`<mirakc のサービス ID>.png`) として書き出す。
TS は `dt295 tune` から受け取る。

```
# BS / CS: 929 から (最長 45 分)
dt295 tune --serial AB12345678 --if-free --sat 1318000000 --tsid 40f1 --sid 929 \
  | dt295 logo --ts - --mirakc /var/lib/dt295/logos
# 地デジ: 1 チャンネルずつ (それぞれ 10 分ほど)
dt295 tune --serial AB12345678 --if-free --ter 557142857 --sid 1024 \
  | dt295 logo --ts - --mirakc /var/lib/dt295/logos --wait 15
# mirakc の設定のチャンネルに resource.logos を付ける
dt295 mirakc-channels --bondriver BonDriver_DT295.ini --logos /var/lib/dt295/logos -o channels.yml
```

- `--if-free` は録画中の個体を奪わない (下の "`dt295 tune`" セクション)。
  ロゴの受信中に録画ソフトが同じ個体を使うと、ロゴの受信は途中で終わる。
  mirakc 3.4 の番組表の取得も同じで、既定では 8 時と 20 時から 2 時間 20 分ほど (TS ごとの 48 チャンネルのとき) 個体を使うので、その時間は避ける。
- `--ts -` は、ロゴがそろうか、`--wait` が過ぎるか、入力が終わるまで読み込み
- 書き込むのは内容が変わったファイルのみで、透明のダミーしか届かないサービスのファイルは削除する。
  ロゴがそろわずに終了したときは、まだ無いファイルの追加のみ行う。
- 毎週実行する systemd の timer は [INSTALL.md](../INSTALL.md) の手順 10

### `dt295 mirakc-channels`

mirakc の設定の `channels:` を作る:

```
dt295 mirakc-channels --scan --serial AB12345678 -o channels.yml       # Linux: デーモンを通してスキャン (6 分ほど)
dt295 mirakc-channels --bondriver BonDriver_DT295.ini -o channels.yml   # TVTest のスキャンの結果から
```

出力の例 (チューナーの部分は手で書く):

```yaml
server:
  addrs:
    - http: 0.0.0.0:40772
tuners:
  - name: dt295-AB12345678
    types: [GR, BS, CS]
    command: dt295 tune --serial AB12345678 {{{extra_args}}}
    decoded: true               # デバイスがカードの復号まで済ませて出す
channels:                       # ここから下が dt295 mirakc-channels の出力
  - name: "UHF 27ch"
    type: GR
    channel: "27"
    extra-args: "--ter 557142857"
    services: [1024, 1025]
  - name: "BS15/TS1"
    type: BS
    channel: "BS15_TS1"
    extra-args: "--sat 1318000000 --tsid 40f1"
    services: [101, 102]
```

- TS ごとに 1 チャンネルで、`services` はその TS のサービスすべて。
  mirakc は同じ TS のサービスを 1 台のチューナーでまとめて扱うので、同じ TS の番組の同時録画ができる。
- `--per-service` を付けると、1 サービスで 1 チャンネル (`extra-args` に `--sid`)
- mirakc のサービスの ID はチャンネルの構成によらないので、作り直しても変わらない
- raw-TS モード (`raw-ts = true` の個体) のときは、チューナーを `decoded: false` にして `filters.decode-filter` に解除ツールを設定する (上の "`dt295 serve`" の "raw-TS モード")
- `--scan` は録画ソフトが個体を使っていないときに実行する (使われていたら奪わずに失敗する)。
  UHF を 1 つずつ選局し (`--catv` で CATV も)、衛星は TS を 2 つずつ選局して SDT から全サービスを集める。
  BS の予備のサービスも、スキャン時に放送していなくても含まれる。
  放送が始まれば mirakc のサービスの取得 (毎日 2 回) で見つかる。
- `--on-air` を付けると、衛星のサービスを 1 つずつ選局し、現在放送中のもののみを残す。
  不要なサービスは `services` から削除する。
- `--bondriver` は、TVTest のスキャンの .ch2 で有効なサービスを使う。
  Windows でも動作するので、TVTest のフォルダーで作成して Linux にコピーできる。
- 同じ局が 2 つの地デジのチャンネルで受信できるときは、番号の小さい方のみを使い、もう一方は「skipped」と表示
- `--logos <フォルダー>` は、PNG があるサービスの `resource.logos` をチャンネルの後ろに付ける。
  PNG の有無は実行したマシン上で調べるので、ロゴのフォルダーがあるマシンで実行する。
  mirakc の設定に `resource:` がすでにあるときは、`logos:` をその下に移す (同じキーが 2 つあると mirakc は読めない)。
  mirakc は設定を起動時に読むので、ロゴが増えたら作り直して mirakc を再起動する。
- 番組表の取得は 1 チャンネルずつ選局するので時間がかかる。
  チューナーが 1 台だと、EPGStation は別のチャンネルの録画が続くとき、次の録画の準備のために前の録画を止める (最後の 15 秒が欠ける)。
  mirakc の番組表の取得は、録画にチューナーを取られるとその回を打ち切り、次は 12 時間後になる。
  どちらも 2 台あれば起きない。

### `dt295 serve`

個体を開いたまま保持し、`dt295 tune` と BonDriver からの要求で選局して TS を送信するデーモン。
録画ソフトが `dt295 tune` を止めても、TVTest を閉じても、個体の後始末はデーモンが行う。
Windows では BonDriver が DLL のフォルダーの `dt295.exe` を起動するので (上の "BonDriver (TVTest)" セクション)、手で起動する必要はない。
Linux の systemd のユニットは [INSTALL.md](../INSTALL.md) の手順 5。

設定ファイル (Linux では `/etc/dt295.conf`):

```ini
[dt295]
; ファームウェア 2 つの置き場所
firmware-dir = /usr/lib/firmware/dt295
; 個体ごとの状態ファイル (<シリアル番号>.state)
state-dir = /var/lib/dt295
; dt295 serve が待ち受け、dt295 tune が接続するソケット
; (TCP にするときは代わりに listen = 127.0.0.1:41200 など)
socket = /run/dt295/dt295.sock

; 使う個体ごとに 1 つ (シリアル番号は dt295 list で分かる)
[unit AB12345678]

; raw-ts = true でその個体を raw-TS モード (完全に自前コードのみ) にする
[unit CD23456789]
raw-ts = true
```

- どれも省略できる (`[dt295]` は上の値が既定)。
  Linux では `/etc/dt295.conf` が無ければ、すべて既定で動く。
  未知のセクションやキーはエラーになる。
- `[unit ...]` を書くと、デーモンはその個体を起動時に開き、その個体のみを扱う。
  書かなければ、要求の個体 (指定が無ければ、つながっている 1 台) を最初の要求で開き、つながっている個体はどれも扱う。
- `[unit ...]` の `raw-ts` は既定 `false` (System1, 内蔵カードで解除)。
  `true` にするとその個体は純正ファーム無しでスクランブルされたままの TS を出し、解除はホスト側に任せる (下の "raw-TS モード")。
  `[unit ...]` の無い個体は、開いたときの要求 (BonDriver の `RawTS`) のモードで動き、開いている間は別のモードの要求を断る。
- コメント (`;` か `#`) は行頭に書く。
  値の後ろに書くと値の一部になる。
- `dt295 serve -c <ファイル>` で別の設定ファイルを読み込み
- `--firmware-dir` と `--state-dir` は、設定ファイルの `firmware-dir` と `state-dir` より優先される
- `--idle-exit <秒>` を付けると、クライアントがその秒数いなければ終了する (BonDriver が起動するとき)。
  選局中とストリーム中は、クライアントがいるものとして数える。
- `listen` に `ホスト:ポート` を書くと、ソケットの代わりに TCP で待ち受ける (`socket` とどちらか一方)
- 待ち受ける場所は、コマンドラインの `--socket` / `--listen`、環境変数の `DT295_SOCKET` / `DT295_LISTEN`、設定ファイルの `socket` / `listen`、既定の順で決まる
- 前の `dt295 serve` が残したソケットのファイルは置き換える。
  そのソケットで別の `dt295 serve` が応答するときと、ソケットでないファイルがあるときは、消さずにエラーで終了する。
  ソケットの横に `<ソケット>.lock` を作る (ソケットを置き換えるときと消すときの排他に使い、消さない)。
  ロックできなければ、警告を出してロックせずに続ける。
- ソケットは dt295 グループが読み書きできるので (0660、Linux)、録画ソフトのユーザーを dt295 グループに追加
- ログは journald (`journalctl -u dt295`)
- 個体が抜かれたら閉じて、挿し直されたら再オープンする。
  ファームウェアが止まっていたら、選局の前に再初期化する。
  ストリーム中に止まったら、起動し直して同じクライアントに送り続ける (3 秒ほど途切れる)。

Windows では次の点が異なる:

- 設定ファイルは、`-c` で指定したときのみ読む
- `socket` の既定は `%TEMP%\dt295-<ハッシュ>.sock` で、ハッシュは `dt295.exe` のフォルダーのパスから求める。
  同じフォルダーの `dt295 tune` と BonDriver は、指定しなくても同じソケットにつながる。
- AF_UNIX を使えない環境 (Windows 10 1803 より前、`%TEMP%` のパスがソケットのアドレスに収まらない) では、既定はループバックの TCP で、ポートは `41200`-`41299` のうちフォルダーで決まる
- `firmware-dir` と `state-dir` は、既定で `dt295.exe` のフォルダー (設定ファイルを指定したときは、そのフォルダー)。
  設定ファイルの相対パス (`socket` も) は、設定ファイルのフォルダーが基準。
- ログは標準エラー出力 (BonDriver が起動したときは `dt295-serve.log`)

#### raw-TS モード (完全に自前コードのみ)

個体に `raw-ts = true` を書くと、その個体は純正ファームを一切使わず、ホストがチップを cold (電源投入直後) から起こしてスクランブルされたままの TS を出す。
B-CAS の解除は、録画ソフトのパイプラインに置いた外部ツールと、ホストに挿した PC/SC リーダーの契約済みカードで行う (内部の仕組みと機能差は [raw-ts.md](raw-ts.md)、導入は [INSTALL.md](../INSTALL.md) の "Linux: raw-TS モード")。

- ファームウェア (`firmware-dir`) も内蔵スロットの B-CAS カードも使わない
- 最初の選局は cold の個体が要る。
  デーモンが個体を開いたまま保持するので、以降のチャンネル変更は retune だけで速い。
  物理的に抜き挿しすると、次の選局で改めて cold から立ち上げる
- ストリームが止まったら、同じチャンネルを選局し直す (System1 のようなファームの再初期化はしない)
- mirakc では、チューナーを `decoded: false` にし、`filters.decode-filter` に解除ツールを設定する:

  ```yaml
  tuners:
    - name: dt295-CD23456789
      types: [GR, BS, CS]
      command: dt295 tune --serial CD23456789 {{{extra_args}}}
      decoded: false              # スクランブルされたまま出すので mirakc 側で解除する
  filters:
    decode-filter:
      command: /usr/bin/b25 /dev/stdin /dev/stdout
  ```

  libarib25 の `b25` は `-` (標準入出力の意味) を受け付けないので、`/dev/stdin /dev/stdout` を渡す。
  解除には pcscd (pcsc-lite) とリーダーのカードが要る

### `dt295 tune`

```
dt295 tune --serial AB12345678 --ter 557142857 > ter.ts             # 地デジ (周波数は Hz)、TS の全サービス
dt295 tune --serial AB12345678 --sat 1318000000 --tsid 40f1 > bs.ts # BS / CS (IF の Hz、TSID は 16 進)
dt295 tune --serial AB12345678 --ter 557142857 --sid 1024 > nhk.ts  # 1 サービスだけ
```

- `--tsid` と `--sid` の省略は `dt295 capture` と同じ
- `--serial` を省くと、デーモンが扱う 1 台 (設定ファイルの 1 台か、つながっている 1 台)
- 止めるまで TS を出力し続ける。
  どの止め方でも、デーモンがストリームを止める。
- 選局できなかったときは理由を表示して終了コード 1
- 同じ個体に新しい要求が来ると、デーモンは前のストリームを止めて切り替え
- `--if-free` を付けると、ストリーム中の個体は奪わずに失敗する。
  ロゴの受信やチャンネルスキャンなど、録画を止めてはいけない用途に使う。
  `--if-free` の要求と通常の要求が同時に来たときは、通常の要求が優先される。
  `--if-free` のストリームの開始中 (2-3 秒) に来た要求は、開始が完了してから切り替えるので、その分だけ待たされる。
- 接続先は `dt295 serve` と同じく、コマンドラインの `--socket` / `--connect`、環境変数の `DT295_SOCKET` / `DT295_LISTEN`、設定ファイル (`-c <ファイル>`、Linux では既定で `/etc/dt295.conf`) の `socket` / `listen`、既定の順で決まる (`mirakc-channels --scan` も同じ)。
  Windows の既定は `dt295.exe` のフォルダーのソケットなので、BonDriver のフォルダーの `dt295.exe` なら TVTest と同じ `dt295 serve` につながる。

## `bontest` (BonDriver の動作確認)

TVTest と同じ手順で DLL を読み込んで試す。
DLL は絶対パスで渡す。
32bit 版の DLL には `target\i686-pc-windows-msvc\release\bontest.exe` を使う。

```
# チャンネルの一覧 (デバイス不要)
bontest C:\TVTest\BonDriver\BonDriver_DT295.dll

# UHF (空間 0) の 27ch (チャンネル 14) を選局し、8 秒間 C/N と TS の受信量を表示 (out.ts に保存もできる)
bontest C:\TVTest\BonDriver\BonDriver_DT295.dll 0 14 8 [out.ts]

# 1 回開いたまま空間 (と範囲) を順に選局し、1 チャンネルごとの所要時間を表示 (TVTest のスキャンと同じ呼び方。PSI は待たない)
# <空間>[:<最初>[-<最後>]] を並べる。例: CS110 の 1 つ目、BS の 1 つ目、UHF 全部
bontest C:\TVTest\BonDriver\BonDriver_DT295.dll scan 3:0 2:0 0

# チャンネルを選ばずに 60 秒開いたままにする (その間 dt295 serve が動き続け、閉じてから終わることを確認)
bontest C:\TVTest\BonDriver\BonDriver_DT295.dll idle 60

# 27ch を 10 秒受信して閉じ、90 秒後に DLL を読み込んだまま同じことをもう一度 (DLL を読み込んだままにするソフトの動き)
bontest C:\TVTest\BonDriver\BonDriver_DT295.dll reopen 0 14 10 90
```

DLL の ini とログ、DLL のフォルダーの `dt295.exe` (`dt295 serve`) と状態ファイルがそのまま使われる。

## 困ったとき

- 使っているバージョンの確認: `dt295 --version`, DLL はプロパティの詳細タブの製品バージョンか, ログの先頭の `version` の行
- TVTest で BonDriver を開けない: DLL のフォルダーに `dt295.exe` があるかを確認する。
  `dt295 serve` がすぐに終了したときは、`dt295-serve.log` に理由が出る。
- 選局が失敗し、BonDriver のログに `busy`: 別の TVTest がその個体で視聴している。
  `is not connected` や `no PIX-DT295 is connected`: 個体がつながっていない (挿せば、TVTest を開いたままで映像が出る)。
  `streams raw-ts ... not system1` (またはその逆): `RawTS` の違う TVTest が個体を開いている。
- TS が出ない、スキャンで何も見つからない: カードが挿さっているか、DLL のフォルダーに 2 つのファームウェアがあるかを確認する。
  BonDriver のログ (`LogFile=1`) の `SetChannel` の行と、`dt295-serve.log` の `streaming` と `stream ended` の行を確認する。
- `dt295-serve.log` に `the firmware faulted` や `booting the device again`: ファームウェアが応答しなくなった。
  `dt295 serve` が起動し直して選局をやり直す。
  ストリーム中なら、起動し直してストリームを始め直す (ログに `starting the stream over`。3 秒ほど途切れる)。
  `cannot boot the unit` が続くときは USB を抜き挿しする。
- 映像も音も出ない (字幕のみ。TVTest のスクランブルの数は 0): B-CAS カードが正常に認識されていない。
  カードを抜き挿ししたときは、挿してから 6 秒ほどで自動で復帰する (`dt295-serve.log` に `a B-CAS card was put in: starting the stream over`)。
  `dt295-serve.log` に `b-cas: card state 5 into the stream` が出るときは、カードの接触不良なので挿し直す。
- 長時間使った後に TS が欠ける、または出ない: USB を抜き挿しすると復旧する。
  TVTest は開いたままでよい (挿し直すと自動で再オープンし、同じチャンネルを再選局する)。
- raw-TS モードで、ログに `no pump for ... ms` や `continuity gaps` の WARN が出る (Linux): 非力な機器で pump が間に合わず、TS が欠けている。
  `systemctl edit dt295` で `[Service]` に `LimitRTPRIO=1` と `LimitNICE=-10` を追加し、デーモンを再起動する。
  優先度が上がったかは、ログの `raw-TS cold: stream thread at ...` の行で確認できる。
- 同じ個体を TVTest と `dt295 capture` で交互に使うと復号できない (ログに `no decodable PAT -- the link key does not match`): デバイスは、前のプロセスの終了から 1 分ほどでセキュアセッションが切れるまで、最初に渡されたリンク鍵で暗号化し続ける。
  鍵は状態ファイルに残るので、`dt295 capture` には `--bondriver` で BonDriver の ini を渡す (`dt295 serve` と同じ状態ファイルを使う)。
  抜き挿しでも直る。
- 夜中のスキャンで一部の局が見つからない: 放送休止中の局はロックしない。
  日中にもう一度スキャンする。
- 受信環境やチャンネル構成が変わった: 状態ファイルを消して再スキャン (状態ファイルは作り直される)
