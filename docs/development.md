# 開発者向け資料

このプロジェクトに手を入れる人向け。
使い方は [usage.md](usage.md)、デバイスの仕様は [device.md](device.md)。

## 概要

- 実装は Rust (`src/`、Cargo workspace、edition 2024、resolver 3)
- 視聴に使うのは System1 の経路 (trc ファームウェア + EP 0x81)。
  2 チューナーの機種 (PIX-DT295W、PIX-DT195W) の 2 つ目のチューナーのみ System2 の経路 (EP 0x86) を使う (下の "2 チューナーの機種" セクション)。
- 大きい未解決の課題は CS ND08 の視聴中のドロップ (下の "未解決の課題" セクション)
- 個体を開くのは `dt295 serve` (デーモン) で、BonDriver はそのクライアント (下の "BonDriver と `dt295 serve`" セクション)。
  ほかの `dt295` のコマンド (`capture`, `logo` など) は、デーモンが開いていない個体を自分で開く。
- raw-TS 経路 (スクランブルのまま配信し、解除は TVTest に任せる) の内部は [raw-ts.md](raw-ts.md)

## 構成

| crate | 種類 | 内容 |
|---|---|---|
| `src/dt295-usb` | lib | USB アクセス。Windows は windows-sys で WinUSB を直接、Linux は nusb |
| `src/dt295-chdef` | lib | デバイスに依存しないチャンネル定義。設定ファイルの文字コード, Win32 のプロファイル API と同じ ini の読み方, C の数値の書式, 地デジと BS / CS110 の周波数の表 |
| `src/dt295-core` | lib | ハードウェアの土台。機種, レジスターとコマンドのプロトコル, I2C, フロントエンド (選局, ロック待ち, C/N), チャンネル表, ファームウェアの転送, `dt295 serve` とクライアントの要求と応答 (`serve`) とソケット (`ipc`), 共有する型 (`System` / `Status` / `TsSink`) |
| `src/dt295-rawfw` | lib | 自前コードのみの raw-TS 経路 ([raw-ts.md](raw-ts.md))。cold stream の worker, FR monitor のホスト側, ホスト面の demod I2C。`dt295-core` の上に乗る |
| `src/dt295-frmon` | lib | 自前の boot0 monitor (FR CPU) と、必要な FR 命令の小さな assembler (オンチップのコードのビルドと確認に使う) |
| `src/dt295` | lib | ドライバーのコア (認証, System1 と System2 のストリーム, ファームウェアの起動), I/O の無い TS 処理, 状態ファイル。`dt295-core` と `dt295-rawfw` の公開 API を re-export する |
| `src/dt295-bondriver` | cdylib | `BonDriver_DT295.dll` (中身は Rust, `IBonDriver2` は C++ の薄い shim)。`dt295 serve` のクライアント。`bontest` と見本の ini もここ |
| `src/dt295-cli` | bin | `dt295` のサブコマンド (デーモンの `serve` と、そのクライアントの `tune` も) |

| ファイル | 役割 |
|---|---|
| `dt295-usb/src/` | 制御, バルク, 割り込み転送, バルク IN の読み出しキュー `ReadQueue`, 個体の一覧 (4 つの機種の PID) |
| `dt295-core/src/model.rs` | 機種 (`Model`: PID からの判定, チューナーの数) |
| `dt295-core/src/{proto,fwdl}.rs` | レジスター, doorbell, コマンドと ACK 待ち, ファームウェアの転送 |
| `dt295-core/src/{i2c,fe1,signal}.rs` | I2C, フロントエンド (選局, ロック待ち, C/N)。`fe1::FrontEnd` が復調のどの組か (2 チューナーの機種は 2 組) と、TS の出力 (`Port`) を表す |
| `dt295-chdef/src/{text,profile,num,plan}.rs` | 設定ファイルの文字コード, Win32 のプロファイル API と同じ ini の読み方, C の数値の書式, 地デジのチャンネル表と BS / CS110 の周波数の表 (BonDriver と `dt295` が共有) |
| `dt295-chdef/src/{bda,chset,px}.rs` | ほかの BonDriver のチャンネル定義ファイル (`[Tuner] ChannelFile`): BonDriver_BDA の ini, PT の ChSet.txt, BonDriver_PX シリーズの ini を、そのドライバーと同じ読み方と番号の振り方で読む |
| `dt295-core/src/{chan,bonini}.rs` | TSID からのトランスポンダー, 衛星の ini の行, ini のチューニング空間, ini の `[Tuner]` (個体, raw-TS, ほかの BonDriver のチャンネル定義。ファームウェアと状態ファイルは ini のフォルダーのもの) |
| `dt295-core/src/{serve,ipc}.rs` | `dt295 serve` とクライアント (`dt295 tune`, BonDriver) の要求と応答の行, 両者がつながるソケット (Unix ドメインソケットか、ループバックの TCP) |
| `dt295-rawfw/src/` | 自前コードのみの raw-TS 経路 ([raw-ts.md](raw-ts.md))。`rawstream` が cold stream の worker, `frmon_host` が FR monitor のホスト側, `fjcore` がホスト面の demod I2C と cold 立ち上げ |
| `dt295-frmon/src/` | 自前の boot0 monitor (FR CPU) と FR 命令の小さな assembler |
| `dt295/src/{device,fw}.rs` | 公開 API `Device`, ファームウェアの起動と残った状態からの回復, ほかのスレッド向けの `Live` (ストリーム中の C/N など) |
| `dt295/src/{auth,cas,dtvdec}.rs` | 起動時の認証, セキュアセッション (keepalive, B-CAS の第 2 段), リンク鍵 |
| `dt295/src/s1/` | System1 ストリームと worker (下の "仕組み" セクション)。`decoder_init.bin` は純正の初期化のキャプチャで、手で編集しない |
| `dt295/src/s2.rs` | 2 チューナーの機種の 2 つ目のチューナー (`Tuner2`): 選局と System2 ストリームの worker, raw-TS のストリームの開始と停止 |
| `dt295/src/raw.rs` | raw-TS のストリームの worker (`Device` と `Tuner2` が共有) |
| `dt295/src/ts/` | I/O の無い TS 処理 (PAT, PMT, セクションの組み立て, リンク暗号, PSI keeper, 全サービスのルート `route.rs` など)。単体テストの大半はここ |
| `dt295/src/{state,watch}.rs` | 状態ファイルと、リンク鍵と PSI のキャッシュの扱いの規則 (`dt295 serve` と `dt295` のほかのコマンドが共有), ストリームを始め直すかの判断 (`CardWatch`: B-CAS カードと止まったファームウェア、後者のみなら `StallWatch`) |
| `dt295-bondriver/src/{ffi,driver}.rs`, `cpp/` | `CreateBonDriver` と shim。`IBonDriver2` の中身 (チューニング空間, 選局の要求, TS の FIFO, raw-TS の prime) |
| `dt295-bondriver/src/{config,client,fifo,logging}.rs` | ini の読み込み (ログの設定と DLL の名前から決まるパス), `dt295 serve` のクライアント (起動, 選局の要求, C/N の問い合わせ, ストリームの回復), TS の FIFO, ログ |
| `dt295-cli/src/` | サブコマンドごとのファイル。`tuner.rs` は `capture`, `logo`, `serve` が共有する個体の起動と停止の合図, `rawts.rs` は `capture` と `serve` が共有する raw-TS の開始と最初の TS の待ち (どちらのチューナーにも使う `RawTuner`), `bonini.rs`, `ch2.rs`, `sections.rs` は ini, .ch2, TS のパケットの読み取り |
| `dt295-cli/src/serve/`, `tune.rs` | `dt295 serve` と `dt295 tune` (Linux と Windows)。`conf` は設定ファイルと接続先, `unit.rs` は個体のスレッド, `tuner2.rs` は 2 チューナーの機種の 2 つ目のチューナーのスレッド, `raw.rs` は両方のスレッドが使う raw-TS のストリーム |
| `packaging/arch/` | Arch Linux のパッケージ (PKGBUILD と makepkg の install スクリプト) |
| `packaging/linux/` | パッケージが共有する systemd のユニット, sysusers, tmpfiles, udev のルール |
| `packaging/nfpm/` | リリースの deb, rpm, Arch Linux のパッケージ (nfpm の設定と、インストール, 削除のときのスクリプト) |
| `packaging/examples/` | mirakc 向けの局ロゴの受信のスクリプトと systemd の timer |
| `.github/workflows/release.yml` | リリースのビルドと公開 (下の "リリース" セクション) |
| `.github/workflows/actions.lock` | workflow が使う action のコミットの固定 (`gh actions-lock` が生成する lockfile) |
| `CHANGELOG.md` | リリースごとの変更点 (リリースノートの元) |
| `tools/` | 純正ドライバーの USB 通信の観測 (USB ETW) |

設計上の決まり:

- 1 つ目のチューナーは System1 の経路のみで、trc ファームウェアが無いとストリームは始まらない。
  System2 の経路 (EP 0x86, trc ファームウェアなし) は 2 チューナーの機種の 2 つ目のチューナー専用。
- BonDriver は個体を開かない。
  個体は `dt295 serve` の個体ごとのスレッドが持ち、BonDriver はソケットで選局を頼んで TS を受け取る。
- ストリーム中にデバイスと I/O するのは worker のみ。
  ほかのスレッドは待つか、読み取り専用の値を参照するのみ。
  `dt295 serve` は `status` の要求に worker が毎秒読んだ値を返し、デバイスには触らない。
  BonDriver の `GetSignalLevel` は、その値を 1 秒ごとに問い合わせたものを返し、待たない (TVTest は常に呼ぶので、ここで I/O すると止まった個体にアクセスが集中する)。
- nusb は Windows では使わない。
  WinUSB の RAW_IO を常に有効にし、`PIPE_TRANSFER_TIMEOUT` も設定できないため、実機で確認した読み方 (同期 ReadPipe + タイムアウト, RAW_IO 付きの 16 本のキュー) を再現できない。
- C++ の shim が必要。
  TVTest は `CreateBonDriver` の戻り値を `dynamic_cast<IBonDriver2*>` するので、MSVC の RTTI 付きのクラス (x86 では thiscall) が必要。
  `CreateBonDriver` 自体は Rust がエクスポートする。
  FFI の入り口はすべて `catch_unwind` で panic を止める。
- CRT は静的リンク (`.cargo/config.toml` の `+crt-static`)
- `dt295 driver install` は、デバイスのセットアップクラスを USBDevice にしてから、Windows 付属の `winusb.inf` の「WinUsb Device」を `DiInstallDevice` でインストールする。
  純正ドライバーは WHQL 署名で優先度が高く、新しく挿した個体には毎回それが選ばれるので、個体ごとに一度 `install` が必要。

## ビルドとテスト

```
cargo build --release                                   # x64
cargo build --release --target i686-pc-windows-msvc     # x86
cargo test --workspace                                  # デバイス不要
cargo clippy --workspace --all-targets
cargo check --target x86_64-unknown-linux-gnu -p dt295-usb -p dt295 -p dt295-cli   # Linux 向けのビルド確認
cargo test -p dt295-usb -- --ignored                    # 実機のスモークテスト (ほかのプロセスが個体を使っていないこと)
```

- 実機での確認は `bontest` ([usage.md](usage.md))。
  `scan 3:0 2:0 0` のように衛星の後に地デジを選局すると、選局の切り替えまで確認できる。
- 試験用に DLL を別のフォルダーに置くときは、`dt295.exe` とファームウェアも同じフォルダーに置く。
  TVTest のフォルダーの状態ファイル (`<シリアル番号>.state`) をコピーすると、リンク鍵を引き継げる。
- バージョンは `src/version.rs` がビルドのときに作る (例: `0.1.0.r126.ge4c1e81`)。
  形は PKGBUILD の `pkgver` と同じで、`Cargo.toml` の version, コミット数, コミットを並べる。
  ソースに未コミットの変更があると `+dirty` が付き、git が使えないときは `Cargo.toml` の version のみになる。
  Windows の DLL と exe には `rc.exe` でバージョン情報のリソースを埋め込む (ファイル バージョンの 4 つ目はコミット数)。
- `packaging/arch/` の PKGBUILD は HEAD を clone してビルドし、`check()` で `dt295-usb`, `dt295`, `dt295-cli` のテストを Linux で実行
- `dt295 etw-urbs` は、`tools/usb_etw_capture.cmd` で取った純正ドライバーの USB ETW をアクセスの一覧に変換 ([device.md](device.md) の "純正ドライバーの通信の観測" セクション)

## リリース

GitHub のリポジトリ (smpn2/pix-dt295) は、履歴を含めないスナップショット。
そこにタグ `v<版>` を push すると、`.github/workflows/release.yml` がビルドしてリリースを作る。

1. `Cargo.toml` の `[workspace.package] version` を上げ、`CHANGELOG.md` にその版の節 (`## <版>`) を追加してコミットする
2. GitHub のリポジトリにスナップショットとしてコミットし、push する
3. GitHub のリポジトリで、署名付きのタグを push する:

   ```
   git tag -s v0.2.0 -m v0.2.0
   git push origin v0.2.0
   ```

| job | 内容 |
|---|---|
| `meta` | タグと `Cargo.toml` の version の照合, `CHANGELOG.md` の節からリリースノートの作成 (節が無ければ失敗) |
| `windows` | x64, x86, arm64 のビルドと zip。テストは x64 と x86 のみ |
| `linux` | musl の x86_64, i686, aarch64, armv7 のビルド, tar.gz と、nfpm で deb, rpm, Arch Linux のパッケージ。テストは x86_64 と i686 のみ |
| `release` | `SHA256SUMS`, 全ファイルの attestation (`actions/attest`), ドラフトのリリースにファイルを添付してから公開 |

- ビルドの job は読み取り専用のトークンで動く。
  書き込みと署名 (OIDC) の権限を持つのは、リポジトリのコードを実行しない `release` のみ。
- キャッシュは使わない (ほかの run が書いたキャッシュをリリースに取り込まないため)
- 手動 (workflow_dispatch) で動かすと、リリースも attestation も作らずに、ファイルを run の artifact に残す (タグを打つ前の確認用)
- 埋め込まれるバージョン (`src/version.rs`) のコミット数とコミットは、GitHub のリポジトリのもの
- action は `.github/workflows/actions.lock` で各コミットに固定され、runner はタグが動いても記録したコミットを使う。
  `uses:` は版のタグのまま書き、lockfile は手で編集しない。
  action を追加, 更新したら `gh actions-lock` (GitHub CLI の拡張 `github/gh-actions-lock`) で lockfile を作り直し、workflow と一緒にコミットする。
  lockfile に無い action を使う job は失敗する。
- Renovate の action の更新の PR は lockfile も作り直す。
  作り直せなかった PR は、その branch で `gh actions-lock` を実行してから merge する。
- GitHub のリポジトリの設定で、Immutable releases (公開後のファイルの差し替えの禁止)、`v*` のタグを作れる人を制限する ruleset、Require lockfile (lockfile に載っていない workflow の禁止) を有効にしておく

## 仕組み

### 起動 (`Device::start`)

`dt295 serve` が System1 の個体を開いたとき (と、`dt295 capture` などが個体を開いたとき) に起動する。
起動前のファームウェアの状態によって処理を分ける:

- 0 (未ロード): コールドブート (idle ファームウェアを転送)
- 0x10: そのまま次へ
- 0xFFFF: EP 0x81 に残った TS を読み捨て (`fw::drain_ts`)、cmd3 で 0x10 へ
- ストリーム中 (0x01 / 0x02 / 0x04): 通常の停止手順 (`s1::to_idle`) で 0x11 に戻し、以降は次の 0x11 と同じ
- 0x11: idle ファームウェアをその場で再ロードして 0x10 にする (`fw::reload_idle`)。
  失敗したら cmd(2,1) -> cmd(2,0) で 0x10 に戻す。
- 0x12: cmd(2,0) で 0x10 へ

その後、認証 -> IDLE パラメーター -> cmd1 (-> 0x11) -> チューナーのリセットと B-CAS の初期化 -> フロントエンドの初期化。
状態ファイルのリンク鍵は `Device::start` の前に渡す (`Device::set_prior_link_keys`)。

### 選局

`dt295 serve` の個体のスレッドが、要求ごとに行う (BonDriver の `SetChannel` は、`dt295 serve` に要求を送る):

1. 前のストリームを止める。
   ファームウェアが止まっていれば、または前のストリームでデバイスが 10 秒以上何も出力しなかったら、再初期化する。
2. 選局してロックを待つ。
   待つのは地デジで最大約 3.4 秒 (復調 reg 0x80 の bit 7 が 16 回続けて立っていたら、約 1 秒で空きチャンネルとして打ち切る), 衛星で最大約 8 秒。
   地デジはロック後の書き込みの後でロックが外れていたら、戻るのをさらに最大約 1 秒待つ ([device.md](device.md) の "フロントエンドの制御" セクション)。
3. ロックしたチャンネルのみストリームを始める。
   `dt295 serve` が `ok` を返し、BonDriver の `SetChannel` が TRUE を返すのは、ストリームが動いたときのみ。
   ストリームの開始は全体で 7.5 秒以内 (ホストのタイムアウトは 10 秒で、2.5 秒は下の PSI のみのフォールバック用)。
   - ini の `ServiceID` の無いチャンネル: 全サービス (下の "全サービス" セクション)
   - `ServiceID` のある行: single-service で直接選局する。
     未契約の有料サービスなどで 1.2 秒何も届かなければ、その TS を full-mux の PSI のみで流す (`s1::psi_fallback`)。
     ホストからはサービスが見え、スキャンに載る。
4. 選局の途中でファームウェアが止まったら、その場で再初期化して、1 度のみ選局をやり直し (再初期化は約 1 秒)

### 全サービス

レジスターと、デバイスの振る舞いは [device.md](device.md) の "ルートのスロット" セクション。

- サービスを指定しないとき、デコーダー窓を AUTO_PID off、PROGRAM_NUMBER 0、0x101A の bit 12 を 1 にして、ルートのスロットを明示的に書く (`s1::Sel::Routes`)。
  初期化が入れたスロットと ECM の割り当てを消してから、最初のルート (NULL と SI) を書き、`cmd(4,0x20,0)` -> `cmd(5,0,2)`。
- worker が届いた TS の PAT と PMT を解析して (`ts::route::Router`)、PMT, ECM, PCR, ES のルートを、ストリームを止めずに追加する (`s1::route_write`)。
  読み出しを止めないよう、1 回に 4 スロットまで。
  PMT が変われば、不足している PID を追加する。
- 優先順は NULL と SI, PMT と ECM, PCR, 映像と音声, 字幕と文字スーパー (stream_type 0x06), データ放送 (0x0D)。
  59 スロットに入らない分はログに `PID(s) got no slot` と出して捨て、不要になった PID のスロットは、空きが無くなったときに再利用する。
- スクランブルされている PID は、PMT の CA_descriptor (B-CAS, CA_system_id 5) の ECM のスロットに結び付ける。
  ES の記述子が番組の記述子より優先で、ECM の PID が 0x1FFF ならスクランブル無し。
- ECM のルートは 4 つまで一度に書き、5 つ目からは前の ECM の 0.5 秒後に書く (`worker::ECMS_AT_ONCE`, `ECM_GAP`)。
  待っている ECM のスロットへの書き込みと、そのスロットに結び付く PID は ECM と一緒に待ち、ほかのルートは先に書く。
- ストリーム中に `Device::needs_recovery` が立ったら、`dt295 serve` と `dt295 capture` が `Device::start` から起動し直して、同じチャンネルのストリームを始め直す (`StallWatch`。中断は約 3 秒)。
  1 分以内に続けて 3 回までで、その後は前回から 1 分待つ。
- 映像と音声の判定 (`EsCheck::observe_av`) は、ECM の付いた映像と音声の PID 全体で判定する。
  ワンセグはスクランブルされていないので、カードの判定に使えない。

### 衛星: サービスの絞り込みと PSI keeper

- サービスの絞り込み: single-service のストリームでも、デバイスは PAT と SDT[actual] に TS の全サービスを載せてくる。
  そのままだと TVTest が TS の全サービスをその行に割り当て、ほかのサービスは何も映らない。
  衛星ではデバイスの PID 0x0000 / 0x0011 のパケットを NULL にし、PAT を NIT + 選局したサービス、SDT[actual] を選局したサービスのみに絞って送り直す。
  全サービスのときは PAT と SDT をそのまま出力する。
- PSI keeper: PAT, PMT, NIT[actual], SDT[actual|other] を、状態ファイルを通してプロセスをまたいでキャッシュする。
  未契約の有料サービスなどで TS にそれらが含まれない間は、キャッシュを再送する (PAT を見たことが無ければ、SDT のサービスの一覧から作る)。
  PMT は作らない: TVTest のスキャンは、PMT はあるが再生できる映像が無いサービスを落とし、PMT が届かないサービスは登録する。
  そのストリームでデバイスがまだ出していない表のみを再送する (デバイスも使う PID に重ねると連続性カウンターが崩れ、TVTest がドロップと数える)。

### リンク鍵

デバイスの鍵の扱いは [device.md](device.md) の "TS ストリーム (System1 / EP 0x81)" セクション。

コアはセクションの先頭の PAT ごとに、使っている鍵で復号して CRC を検証し、通らなければほかに知っている鍵を試し、通った鍵に切り替える (`s1::link_key_check`)。
切り替えはその PAT から効く (`s1::link_decrypt`)。
ホストは検証済みの鍵 (`Device::link_key`) を状態ファイルに保存する。

セッションが切れた後の起動で渡された鍵は、検証前でも適用されている。
その起動が検証前に終了しても鍵が失われないように、コアは鍵を渡した時点でホストに知らせ (`Device::set_link_key_journal`)、ホストはそれを `linkkey-pending` として状態ファイルに書く。
次のプロセスは検証済みの鍵と未検証の鍵を順に試し (`Device::set_prior_link_keys`)、通った鍵を `linkkey` に書く。
未検証の鍵は、そのプロセスが渡した鍵がまだ検証されていなければそれのみを残し (`Device::unproved_link_key`)、ほかは消す。

### 状態ファイル

形式は [state.rs](../src/dt295/src/state.rs) の先頭のコメント。

- 個体ごとに `<シリアル番号>.state` (`dt295 serve` の `state-dir`。BonDriver が起動したときは DLL のフォルダー)。
  `dt295` のほかのコマンドは、BonDriver の ini を渡すとそのフォルダーの同じファイルを使う。
- 変更があったときのみ書き込み (`dt295 serve` はアイドル中に毎秒確かめ、ストリーム中は検証できたリンク鍵をすぐに書く)
- 90 日間受信していない PSI セクションは、読み込み時に破棄

### C/N (`GetSignalLevel`)

ストリーム中は worker が毎秒、復調チップを読む (raw-TS の worker は、ストリームの開始時にロックしたときの値も publish する)。
`dt295 serve` は `status` の要求にその値 (`Live::cn_mdb`) を返し、3 秒以内のサンプルが無ければ選局時に読んだ値を返す。
BonDriver は 1 秒ごとに `status` を問い合わせ、`GetSignalLevel` はその値を返す。
選局の直後は `ok` で返った値、raw-TS で値が無いときは公称値の 30 dB、ストリームが無いときは 0。

### USB の読み出しと keepalive

- worker は EP 0x81 に 64 KiB x 16 本の読み出しを常時投入 (`ReadQueue`, WinUSB の RAW_IO)
- ファームウェアのイベントのポーリングは最短 100 ms 間隔 (毎回の読み出しの後だと、1 秒に約 240 回の制御転送になる)
- パケットの先頭ごとに同期バイトを確認し、ずれたらその手前までを渡して区切りを探し直す。
  single-service のストリームは、デバイスの PAT がそのサービスを載せるまで何も渡さない。
- keepalive は 12 秒ごと ([device.md](device.md) の "認証とセキュアセッション" セクション)。
  セッションの最初の 1 回はストリームが動き出した時点で送り、ストリームが無ければ認証の 5 秒後 (3 秒後だと short challenge が返ることがある)。
  ストリーム中は worker が、アイドル中は `dt295 serve` の個体のスレッドが 1 秒ごとに `Device::idle_tick` を呼ぶ。
  失敗 (`short challenge`) したら 4 秒後に 1 回のみ送り直す。

### B-CAS カードと USB の抜き挿し

状態の値は [device.md](device.md) の "認証とセキュアセッション" セクション、カードのセッションの振る舞いは "TS ストリーム (System1 / EP 0x81)" セクション。

- worker は割り込みの bit 9 で B-CAS の状態を読み、`Live::card_events` を増やす。
  core は第 2 段の後と、ストリームの開始から 3 秒後にも読み (`Live::card_state`)、3 秒後に 5 なら警告する。
- 映像と音声の有無 (`ts::es::EsCheck`): 自分のサービスの PMT にある映像と音声の PID に、2 秒の間に PES の先頭 (00 00 01) が 1 つも無ければ「無い」で、2 回続くと `Live::es_clear` が `Some(false)` になる。
  映像も音声も無いサービスと、PSI のみのフォールバックは判定しない。
- 復帰の手順 (`CardWatch`。`dt295 serve` が使う):
  - 最後のイベントから 5 秒待ち、カードがあれば、ストリームを止めてカードのセッションをその場で張り直す (`Device::resume_in_place`)。
    待機中に第 2 段のみを送り、3 秒以内に状態が 2 か 3 になれば、選局はそのままでストリームのみを始め直す (中断は約 0.5 秒)。
  - 張り直せなかったら、`Device::start` からやり直して今のチャンネルを再選局 (中断は約 3 秒)
  - それでも映像と音声が無ければ、10 秒, 20 秒, 40 秒後に `Device::start` からもう一度 (1 回の抜き挿しで最大 4 回)
  - イベントが無くても、映像と音声が無いまま 15 秒たてば、選局ごとに 1 回は `Device::start` からやり直し
  - 状態が 0 の間はやり直さない。
    状態が 3 で映像と音声が無いのは契約が無いサービスなので、やり直さない (ログは `no contract for it?`)。
    自分のやり直しの 5 秒以内のイベントは数えない。
- USB を抜くと、worker の読み出しが「no device」で失敗し、`Live::device_lost` が立つ。
  `dt295 serve` はデバイスを閉じてストリームを終え、5 秒ごとに開き直す。
  ストリームが無いときは 5 秒ごとに `dt295::present_serials` (開かずに列挙する) で抜かれていないか確認する。
  BonDriver は終わったストリームを頼み直すので (下の "BonDriver と `dt295 serve`" セクション)、挿し直すと同じチャンネルに戻る。

### デーモン (`dt295 serve`)

`dt295 capture` を録画ソフトのチューナーのコマンドにすると、起動のたびにファームウェアを起動して認証し、録画ソフトに SIGKILL で止められる。
`dt295 serve` は個体ごとのスレッドがデバイスを開いたまま保持し、クライアント (`dt295 tune`, BonDriver) は選局を要求して TS を読むだけにする。
設定は [usage.md](usage.md) の "`dt295 serve`" セクション、プロトコルは [serve.rs](../src/dt295-core/src/serve.rs)、ソケットは [ipc.rs](../src/dt295-core/src/ipc.rs) の先頭のコメント。

- ソケットは Linux でも Windows でも Unix ドメインソケット (Windows は 10 1803 以降の AF_UNIX)。
  Windows の既定は `%TEMP%\dt295-<フォルダーのハッシュ>.sock` で、ハッシュは小文字にしたフォルダーのパスの FNV-1a。
  AF_UNIX を使えないときは、フォルダーで決まるループバックの TCP のポート (`41200`-`41299`)。
- 待ち受ける前に、そのパスにあるファイルを調べる: ソケットでないファイル (Windows は reparse タグが AF_UNIX でないもの) ならエラー、接続できれば別のデーモンがいるのでエラー、接続できなければ前のデーモンの残りとして消す。
  調べてから bind して待ち受けるまでは `<ソケット>.lock` をロックする (同時に起動した 2 つが、互いのファイルを消さないように)。
  ロックファイルを開けないか、ロックできない (別のユーザーの 0600 のファイル、ロックの無いファイルシステム) ときは、警告を出してロックせずに続ける。
  終了するときは、ロックしてから、ファイルが自分の作ったもの (Linux は device と inode、Windows はボリュームとファイルインデックス) のときだけ消す。
  Windows で作成時刻を使わないのは、同じ名前ですぐ作り直したファイルが前のファイルの作成時刻を引き継ぐため (NTFS の tunneling)。
- クライアントの接続先は、コマンドライン, 環境変数, 設定ファイル, 既定の順 (`serve::conf::client_target`)
- 設定ファイルに `[unit]` が無ければ、要求の個体のスレッドをその要求で作る (`Units::route`)。
  要求に個体が無ければ、いまつながっている個体 (`dt295::present_serials`) が 1 台のときにその個体 (抜かれた個体のスレッドは数えない。列挙できなければ、スレッドのある個体)。
  スレッドのモード (System1 か raw-TS) は最初の要求の `mode` で決まり、個体を開いている間は別のモードの要求に `error` を返す。
- 要求の行は接続ごとのスレッドで読み、メインのスレッドに渡す (何も送らない接続が、ほかの要求を待たせないように)。
  同時に読むのは 32 接続までで、それを超えた接続には `busy` を返して閉じる (応答の無いまま閉じると、BonDriver は終了中のデーモンとみなすため)。
- 個体のスレッドが `dt295 serve` の動いている間に終わっていたら (panic など)、次の選局の要求でスレッドを作り直す (`Units::route`)。
  個体は unwind で閉じているので、モードは設定ファイルの個体なら設定のもの、それ以外は要求のもの。
  作れなかったときは、終わったスレッドをリストに残す。
  `status` の要求では作り直さず、終わったスレッドの値 (`unit::Info`) は C/N と `--idle-exit` に使わない。
- ストリーム中に個体のスレッドが panic しても、unwind で書き込みのキューを閉じ、クライアントの接続を shutdown し、`busy` とストリームの値を消す (`unit::Streaming`、`unit::Busy`)。
  クライアントはストリームの終わりを受けて選局を頼み直し、その要求でスレッドが作り直される。
- `status` の要求は、メインのスレッドが個体のスレッドの公開する値 (`unit::Info`) から答える (ストリーム中の個体のスレッドは要求を受けないため)
- `--idle-exit` は、個体のスレッドがどれも要求を処理しておらず、最後の接続からその秒数たったら終了する。
  終了するときは、個体を閉じる前に待ち受けをやめる (その間に来たクライアントが、応答の無い接続ではなく待ち受けの無いソケットを見て、新しいデーモンを起動するように)。
- アイドル中に `if-free` の要求とほかの要求が同時に来たら (`yield_to_queued`)、`if-free` の方に `busy` を返す。
  `if-free` の要求が 2 つなら、直前に処理した要求と同じ `client` の方を通す (チャンネルを変える TVTest を、別の TVTest の再試行より先にする)。
  `if-free` のストリームの開始中に来た要求は、開始が終わってからストリームのループで受けて切り替える。
- ストリーム中に `if-free` の要求が来たら、ストリームと同じ `client` の要求なら切り替え、ほかのクライアントなら `busy` を返す (BonDriver はチャンネルを変えるとき、前のストリームを止める前に新しい要求を送るため)
- 個体のスレッド (`serve/unit.rs`): 開けるまで 5 秒ごとに再試行する。
  アイドル中は毎秒 `idle_tick`、5 秒ごとに `dt295::present_serials` (開かずに列挙する) で抜かれていないか確認し、変わった鍵と PSI を保存する。
  要求が来たら、ファームウェアが止まっていれば、または前のストリームでデバイスが 10 秒以上何も出力しなかったら、再初期化してから選局する。
  途中でファームウェアが止まったら、1 度のみ再初期化してやり直す。
- TS は worker の sink から上限付きのキュー (64 KiB の読み出し 256 個分) を通り、書き込みスレッドがソケットへ書く。
  読み手が遅れても worker は止まらず、あふれた分は捨てて数える。
  書き込みが 30 秒進まなければ、相手がいなくなったとみなす。
- ストリームを止めるのは、接続が切れたとき、新しい要求が来たとき、個体が抜かれたとき、終了の合図のとき。
  相手の切断は、書き込みの失敗のほか、200 ms ごとにソケットを `MSG_PEEK` で調べて検出する (サービスの PAT が届くまで何も書かないため)。
  書き込みのスレッドとソケットを共有するので、ソケットをノンブロッキングにはしない (Linux は `MSG_DONTWAIT`、Windows は待ち時間 0 の `select` の後に `recv`)。
- `dt295 tune` は Linux ではソケットと標準出力を poll で同時に監視し、読み手が出力を閉じたら TS が届いていなくても終了する。
  標準出力へは行バッファーを通さずに書く (Rust の stdout は 0x0A のたびに書き出すため)。
  Windows では、ソケットが 1 秒静かなら標準出力のパイプに 0 バイトを書き込み、失敗したら (`ERROR_NO_DATA`、`ERROR_BROKEN_PIPE`、サーバーが切断した名前付きパイプの `ERROR_PIPE_NOT_CONNECTED`) 読み手が閉じたとして終了する。
  0 バイトの書き込みは、匿名パイプとバイトモードの名前付きパイプの読み手には届かない。
  メッセージモードの名前付きパイプでは読み手に 0 バイトのメッセージとして届くので、書き込まない (読み手が閉じたことには、TS を書いて失敗したときに気づく)。
  パイプの種類を `GetNamedPipeInfo` で調べられないとき (送信専用の名前付きパイプのサーバー側など) は、バイトモードとして扱う。
- カードの抜き挿しやファームウェアの停止で始め直すときは、同じクライアントへの送信を継続
- 再初期化の判定に使う「デバイスが出した量」は `Live::delivered` で、PAT を待つ間に捨てた分も入る
- 2 チューナーの機種の 2 つ目のチューナー (`tuner=2`) は別のスレッド (`serve/tuner2.rs`) が受け持つ。
  個体のスレッドを作るときに一緒に作る。
  個体のスレッドは個体を開いて起動した後 (raw-TS の個体は開いた後) に `Device::tuner2` を `Slot` に貸し出し、個体を閉じる前に返してもらう (`Tuner2` も USB のハンドルを持つため)。
  2 つ目のチューナーは個体を開いた経路で動き、別の経路の要求には `error` を返す。
  個体を開く前に来た要求は、貸し出されるまで最大 15 秒待つ (2 つ目のチューナーの要求で個体のスレッドを作ったときも)。
  System1 の経路では、2 つ目のチューナーのストリームが 10 秒間何も届けなければ (個体のスレッドが起動し直したときなど) 選局からやり直し、個体のスレッドはアイドル中に止まったファームウェアを起動し直す。
- 2 つ目のチューナーのスレッドも、要求の処理中かとストリームの C/N を公開する (`unit::Info`)。
  `--idle-exit` は両方のスレッドを見て、`status` は要求の `tuner` のものを答える。
- 2 つ目のチューナーのストリームも、panic のときは `unit::Streaming` と `unit::Busy` で片付ける (raw-TS の `serve/raw.rs` も)。
  借りた `Tuner2` は `tuner2::Lent` が持ち、unwind ではストリームを止めてから `Slot` に返す。
  `tuner=2` の要求のときに 2 つ目のチューナーのスレッドが終わっていれば、そのスレッドだけを作り直す (`Units::start_tuner2`)。
  新しいスレッドは同じ `Slot` から借りる。
- 個体のスレッドが panic したときは、unwind で 2 つ目のチューナーを返してもらう (`Unit` の `Drop`)。
  2 つ目のチューナーのストリームも終わり、`Tuner2` が USB のハンドルを放して個体が閉じる。
  個体のスレッドを作り直すときは、2 つ目のチューナーのスレッドも新しい `Slot` で作り直す。
  前のスレッドは、要求のチャンネルが無くなって終わる。

### 2 チューナーの機種

仕様は [device.md](device.md) の "PIX-DT195W" と "TS ストリーム (System2 / EP 0x86)" セクション。

- `Device` は個体を受け持つ (起動, 認証, カードのセッション, keepalive, 回復) とともに、1 つ目のチューナー (System1) を扱う
- 2 つ目のチューナーは `Device::tuner2` が返す `Tuner2` (1 つの `Device` に 1 つ) で、`Device` と USB のハンドルとデバイスの状態のロックを共有する。
  起動の回数 (`State::boot_gen`) で、選局の後に個体が起動し直したかが分かる (ストリームの開始は失敗し、選局からやり直す)。
- `Tuner2` の選局は I2C の転送ごとにロックを取る (`LockBus`)。
  選局 (1-2 秒) の間、1 つ目のチューナーの worker を待たせないため。
  2 つ目の worker は EP 0x86 を読み、ロックが空いていれば毎秒 C/N を読んで keepalive を送る (`try_lock`。1 つ目の worker を待たせない)。
- カードのセッション (B-CAS の第 2 段) は起動ごとに 1 回で、どちらのチューナーの最初のストリームでも開く (`State::card_open`)。
  アイドル中の keepalive も、どちらかのストリームがセッションを始めた後から送る。
- `Device::set_tuner(1)` は、2 つ目のチューナーの TS を出力 A に出して 1 つ目の経路 (System1 と raw-TS の 1 つ目の取り込み) で受ける。
  `dt295 capture --tuner 2 --raw-ts` で使う。
- raw-TS では、2 つ目のチューナーは `Tuner2::stream_start_cold` で出力 B からチップの 2 つ目の取り込みを通り、1 つ目と同時に流す ([raw-ts.md](raw-ts.md) の "2 チューナーの機種" セクション)。
  `dt295 serve` の raw-TS の個体でも、2 つ目のチューナーは別のスレッドが受け持つ。
- Windows の TVTest は 1 プロセスに 1 つの BonDriver を読み込み、USB の個体を開けるのは 1 プロセスのみ。
  BonDriver は個体を `dt295 serve` に任せるので、2 つの TVTest (`TunerNumber` の違う DLL) で 2 つのチューナーを同時に使える。

### BonDriver と `dt295 serve`

BonDriver の中身は `dt295 serve` のクライアント (`client.rs`)。

- 接続先は DLL のフォルダーのソケット (`ipc::Target::of_folder`) で、設定は無い。
  同じフォルダーの DLL は、名前によらず同じデーモンにつながる。
- `OpenTuner` で `status` を送り、応答が無ければ DLL のフォルダーの `dt295.exe serve` を起動する (`CREATE_NO_WINDOW`。標準出力と標準エラー出力は `dt295-serve.log` に追記)。
  引数は `--socket` (か `--listen`), `--idle-exit` (`[Server] IdleExitSec`), `--firmware-dir` と `--state-dir` (どちらも DLL のフォルダー)、`LogLevel` が `debug` か `trace` なら `-v` か `-vv`。
  起動した `dt295 serve` には 100 ms ごとに問い合わせ、応答するか、終了するか (別の DLL が起動したデーモンが先に待ち受けたときなど)、`[Server] StartTimeoutSec` が過ぎるまで待つ。
  応答せずに接続を閉じられたら (終了中のデーモン)、100 ms ごとに 1 秒まで問い合わせ直し、待ち受けが無くなれば起動する。
- 要求には `client=<プロセス ID>-<モジュール>-<番号>` と `if-free`、`mode` (`RawTS`)、`DeviceSerial` があれば `unit`、`TunerNumber=2` なら `tuner=2` を付ける (`status` にも)。
  `<モジュール>` は DLL の static のアドレスで、名前を変えた DLL を 1 つのプロセスで開いたとき (BonDriverProxyEx など) にも、別のクライアントになる。
- 監視のスレッドが 1 秒ごとに `status` を送り、C/N を更新する (`dt295 serve` はメインのスレッドで答えるので、選局中でも待たない)。
  ストリームが終わっていたか (読み取りのスレッドが相手の切断を見た)、デーモンが応答しなければ、デーモンを起動し直して同じチャンネルを要求し直す。
  失敗したら間隔を 2 秒から倍にして 30 秒まで延ばす。
- `SetChannel` は、前のストリームを止める前に新しい要求を送る (デーモンは同じ `client` のストリームを置き換えるので、その間に別の TVTest に個体を取られない)。
  応答が来たら前のストリームの読み取りのスレッドを止め (join)、FIFO を空にしてから新しいストリームを読み始める (前の局の TS が新しい局の TS の後に混ざらないように)。
- `SetChannel` で、個体が無い (`not connected`) か別のクライアントが使っている (`busy`) と断られたとき、またはデーモンが応答しなかったときも、監視のスレッドが同じチャンネルを要求し続ける。
  ロックしない、モードが違うなどの断りでは要求し直さない。
- `status` の問い合わせが続く間はクライアントがいるので、TVTest がチャンネルを選んでいなくてもデーモンは終了しない
- raw-TS の prime ([raw-ts.md](raw-ts.md) の "開くときの prime") は、`OpenTuner` で `.ch2` の最初の地デジ局を要求し、ストリームが始まったら止める

### チャンネルスキャン (`mirakc-channels --scan`)

すべて `dt295 serve` への `if-free` の要求で、録画中なら `busy` で止まる (`mirakc/{scan,psi}.rs`)。

- 地デジは UHF (と `--catv` の CATV) を 1 つずつ選局し、PAT に載る TV とラジオのサービスと、SDT の network_id を取得する。
  ロックしないチャンネルは約 4.5 秒で `error no lock`。
- 衛星はネットワークごとに既知の TS (BS15 0x40F1、ND02 0x6020、ND04 0x7040) を 16 秒読み、その SDT[other] にある別の TS をもう 1 つ読んで、最初の TS のサービスも集める。
  SDT[other] は約 13 秒で 1 周し、CS110 はネットワーク 6 と 7 で別。
- TV とラジオ (service_type 0x01 / 0x02) をすべて出力する。
  SDT のフラグは本サービスと予備のサービス (BS朝日 151 と 152 など) で同じで、見分けに使えない。
- `--on-air` のときは 1 つずつ選局し、4 秒以内に PAT が届いたもののみ保持

### 局ロゴ (`dt295 logo`)

ロゴの送出の状況と使い方は [usage.md](usage.md) の "`dt295 logo`" セクション。
TVTest のロゴファイルの形式は [logo/tvtest.rs](../src/dt295-cli/src/logo/tvtest.rs) の先頭のコメント。

- BS / CS のロゴは、サービス 929 (BS-15、TSID 0x40F1、service_type 0xA4) のデータカルーセル。
  component_tag 0x79 が BS (モジュール LOGO-00..05)、0x7A が CS (CS_LOGO-00..05) で、1 モジュールが 1 つの大きさ (logo_type 0..5) の全局分。
  1 回の送信は BS 約 9 秒、続けて CS 約 30 秒で、それ以外の時間はこの PID に 1 パケットも届かない。
  ロゴの PID は 0x0779 (BS) と 0x077A (CS)。
  929 は single-service で選局すればカルーセルが通る。
- 地デジのロゴは各局の CDT (PID 0x29、table_id 0xC8、data_type 0x01) で、single-service の出力も通す。
  セクションは 1 つが logo_type 1 種類分 (226-322 B で 2 パケット) で、約 100 秒ごとに始まり、2 つ目のパケットは約 50 秒後に届くので、6 種類の 1 周は約 10 分。
  サービスとロゴの対応は、SDT の logo_transmission_descriptor (0xCF、伝送種別 1 と 2 の logo_id) で取得する。
- `dt295 logo` は PAT -> PMT (stream_type 0x0D) -> DII / DDB でモジュールを組み立てる。
  正しい PAT が 15 秒届かなければ個体を解放してやり直し、DII の download_id が変わったら古いモジュールは捨てる。
  受信を待つ間に TVTest がロゴファイルを書き換えることがあるので、マージの直前に読み直す。
  `--schedule` は schtasks に XML で登録する (StartWhenAvailable、InteractiveToken、最長 3 時間)。
  `--log` は標準出力と標準エラー出力をファイルに向け直してからコンソールを離す。
- mirakc 向けの種類は mirakc が放送から取得するときの優先順 (5、2、4、1、3、0) で、93 B 以下の透明のダミーを除いて選ぶ。
  ファイル名の `<network_id * 100000 + service_id>` は mirakc のサービス ID。
- 929 の無い TS は CDT を読む (`logo/cdt.rs`)。
  終わるのは、SDT が指定するロゴがどれも 6 種類そろったときか、最初に届いたロゴがもう一度届いたとき。
  どれかのロゴが 2 度届いたのみでは終わらない。
  CDT のロゴは TVTest には足さない (TVTest は視聴中に自分で取得する)。
- `--ts -` の読み取りは別スレッドで、入力が止まっても `--wait` で終了

### ログ

BonDriver は `[Tuner] LogFile=1` でファイルにも書く。
BonDriver が起動した `dt295 serve` のログは、DLL のフォルダーの `dt295-serve.log`。
主な行:

- BonDriver の選局ごと: `SetChannel(<空間>, <チャンネル>) <名前> <MHz> MHz: streaming, C/N ... dB (... ms)`、失敗したら `streaming` の代わりに理由
- `dt295 serve` の選局ごと: `tune ...: streaming (... ms)`、ストリームの終わりに `stream ended after ... s (<理由>): ... bytes`
- ストリームの停止時 (`dt295 serve`): `s1 worker: received N bytes (K null packets left out), P PAT(s), ...` と `s1 worker: N read(s) (M full 64K), longest gap ... ms; K continuity error(s) in the device's TS...; T TEI packet(s)`
- 異常 (連続性エラー、TEI、100 ms を超える読み出しの間隔、復調の誤り、アンロック) のあった分のみ: `s1 stats min N: ... continuity error(s) on PID ..., TEI ..., error seconds: ..., longest gap ... ms, ... MB; demod C/N ..., err 0xEB sum ... max ..., 0xF0 sum ... max ...`
- 衛星のストリームの停止時: `psi: TS xxxx -- re-sent PAT xN, PMT xN, SDT xN, NIT xN | cache: ...`

## 未解決の課題

- 保留中: CS ショップチャンネル (ND08, IF 1733 MHz) の視聴中のドロップ
  - 症状: 1 秒から 25 秒の塊で、全 PID の TS が欠ける (1 回に連続性エラーが数個から 1300 個, TEI 付きのパケットは 0)。
    塊の数は時間帯で増減する。
  - 分かっていること: デバイスが USB に出した時点で欠けている (worker の連続性エラーと TVTest のドロップがほぼ同じ数で、FIFO のあふれも無い)。
    無料放送なのでスクランブルでもない。
    塊の間のみ復調の誤りカウンター 0xEB が 0 でなくなり、C/N も下がるので、受信信号の乱れが有力。
    別々の Windows と Linux で同じ時間帯に発生し、同じ系統の ND20 (1973 MHz) では発生しない。
  - 疑い: LTE の 1.7 GHz 帯の上り (1710-1785 MHz、夜中に減る) の混入, 共通の部分 (V-ONU, 幹線, 分配器), 放送そのもの
  - 次に確認すること: 2 台で同時に受信して、塊が同じ秒に起きるか。
    スマートフォンを V-ONU や分配器の近くで送信させて再現するか。
  - 手がかり: ログの `s1 stats min N: ...` の行と、停止時の `s1 worker: ...` の行
- BonDriver が `dt295 serve` を通すようになってから、PIX-DT295 の実機では確認していない
- Linux で確認していないこと: 複数の個体を並べた運用, 契約していない有料チャンネルの録画, B-CAS カードの抜き挿し, TS ごとのチャンネルでの EPGStation の録画
- 全サービス: TVTest 本体のスキャン (1 TS 1 行の衛星を含む) は未確認。
  BonDriver の出力では、受信できる 48 TS (CS 12, BS 26, 地デジ 10) のすべてで、デバイスの PAT が約 1 秒以内、PMT と SDT[actual] が約 3 秒以内にそろう (PMT が届かないのは、放送に PMT が無いデータのサービスのみ)
- BS / CS のロゴ: 今のコードの TVTest での視聴中の受信は未確認 (`dt295 logo` は受信できる)
- CATV と UHF 51ch 以上 (700 MHz 以上) は、実機で確認していない。
  静的解析では、周波数で変わるのは 700 MHz 以上の 2 つのレジスターのみ ([device.md](device.md))。
## 方針

- ファームウェアの `.bin` と状態ファイル (デバイスのリンク鍵を含む) はコミットしない (`.gitignore` 済み)
- 第三者から提供された解析を引くときは「第三者から提供された」「第三者提供の」と書き、提供元のツール名やファイル名は書かない
