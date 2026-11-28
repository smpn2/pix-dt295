# 導入手順

PIX-DT295(W) と PIX-DT195(W) を Windows (TVTest) と Linux (mirakc + EPGStation) で使い始めるまでの手順。
PIX-DT295W と PIX-DT195 は、実機では確認していない (同じ構成の PIX-DT195W と PIX-DT295 と同じ処理で動かす)。
設定やコマンドの詳しい説明は [docs/usage.md](docs/usage.md) にある。

## 必要なもの

- PIX-DT295(W) か PIX-DT195(W) の本体と、本体に挿した契約済みの B-CAS カード
- 純正ドライバーパッケージのファームウェア 2 つ: `mb86m01_assp_osec_idle.bin` (起動用) と `mb86m01_assp_osec_trc_m_h.bin` (トランスコーダー用)。
  再配布できないので、ピクセラのサイトにある次のどちらかの zip から取り出す。
  - [brk_driver.zip](https://www.pixela.co.jp/support/info/2023/win11_stationtvx/brk_driver.zip): zip の直下
  - [stationtvx_updater12030002.zip](https://www.pixela.co.jp/products/tv_capture/pix_dt460/data/stationtvx_updater12030002.zip) (PIX-DT460 用): `Driver/selene/usb/x64/` などのフォルダー

  どちらも中身は同じで、SHA256 は次のとおり。
  PIX-DT195(W) (Mac 用の機種) にも、この Windows 用のファイルを使える。

  ```
  3c4721204a987a5e8a818be4ac14ee2709a4786413a7ef77ddb33573370dd9d3  mb86m01_assp_osec_idle.bin
  a3520f60e80f08008621dcf44679474906e43a4de36770d769cd7642b19ddd0b  mb86m01_assp_osec_trc_m_h.bin
  ```

  どの機種にも、Mac 用のソフト (StationTV for Mac、Xit for Mac) に入っている同じ名前の 2 つも使える。
  アプリの中の `DT195Lib.framework/Versions/A/Resources/` にあり、SHA256 は次のとおり。

  ```
  6d251c471ceb9ab8494fcd0feb256f39c19528f38bafab9bb7e4ec627e4412dd  mb86m01_assp_osec_idle.bin
  07082da1cd77dff76c6565a5e8b4282d53a6dae6ba26c9dd880214a063711aff  mb86m01_assp_osec_trc_m_h.bin
  ```
- ソースからビルドするときは Rust 1.100 以上、Windows では MSVC も (BonDriver の C++ の shim 用)

## 入手

[GitHub のリリース](https://github.com/smpn2/pix-dt295/releases) から、使う OS と CPU のファイルをダウンロードする。
ソースからビルドするときは下の "ビルド" セクション。

| OS | ファイル |
|---|---|
| Windows | `pix-dt295-<版>-windows-<CPU>.zip` (x64, x86, arm64) |
| Debian / Ubuntu | `pix-dt295_<版>-1_<CPU>.deb` (amd64, i386, arm64, armhf) |
| Fedora / RHEL 系 | `pix-dt295-<版>-1.<CPU>.rpm` (x86_64, i386, aarch64, armv7hl) |
| Arch Linux | `pix-dt295-<版>-1-<CPU>.pkg.tar.zst` (x86_64, i686, aarch64, armv7h) |
| そのほかの Linux | `pix-dt295-<版>-linux-<CPU>.tar.gz` (x86_64, i686, aarch64, armv7) |

32bit 版の TVTest には x86 の zip を使う。
Linux の `dt295` は静的リンクなので、ディストリビューションを問わない。

ファイルはすべて GitHub Actions がタグのコミットからビルドしたもので、Sigstore で署名したビルドの来歴 (SLSA build provenance) が付いている。
改ざんされていないことは [GitHub CLI](https://cli.github.com/) で確認できる:

```
gh attestation verify pix-dt295-0.2.0-windows-x64.zip --repo smpn2/pix-dt295
```

`Verification succeeded!` と、ビルドした workflow とコミットが表示されれば問題ない。
タグと workflow まで照合するコマンドと `SHA256SUMS` の使い方は、リリースノートの "検証" セクションにある。

## ビルド

### Windows

```
cargo build --release                                   # x64: target\release\
cargo build --release --target i686-pc-windows-msvc     # x86 (32bit 版の TVTest 用): target\i686-pc-windows-msvc\release\
cargo build --release --target aarch64-pc-windows-msvc  # ARM64: target\aarch64-pc-windows-msvc\release\
```

生成されるのは `BonDriver_DT295.dll`、`dt295.exe`、`bontest.exe` (BonDriver の動作確認用)。
Visual C++ 再頒布可能パッケージは不要。

### Linux

ビルドできるのは `dt295` コマンドのみ。USB には usbfs を直接使うので、libusb は不要。

```
cargo build --release -p dt295-cli                                          # target/release/dt295
# 静的リンク (別のマシンやコンテナーで使うとき)
cargo build --release -p dt295-cli --target x86_64-unknown-linux-musl       # x64
cargo build --release -p dt295-cli --target i686-unknown-linux-musl         # x86
cargo build --release -p dt295-cli --target aarch64-unknown-linux-musl      # ARM64
cargo build --release -p dt295-cli --target armv7-unknown-linux-musleabihf  # ARMv7
```

musl 向けは `.cargo/config.toml` で Rust 付属の `rust-lld` を使用するため、Windows からもクロスビルドできる。

### macOS

ビルドできるのは `dt295` コマンドのみ。macOS 上で Xcode Command Line Tools が必要。

```
cargo build --release -p dt295-cli --target x86_64-apple-darwin   # x64
cargo build --release -p dt295-cli --target aarch64-apple-darwin  # ARM64
```

## Windows: TVTest から使う

1. 個体のドライバーを WinUSB に切り替える (個体ごとに一度。UAC の確認が表示される)。
   TVTest などで開いていない状態で:

   ```
   dt295 driver install
   dt295 driver status        # WinUSB になったかの確認
   dt295 list                 # シリアル番号 (例 CD23456789)
   ```

   純正のアプリ (StationTV X) に戻すときは `dt295 driver restore`。
   PIX-DT195W には Windows の純正ドライバーが無いので、挿しただけではドライバーが割り当てられず、`install` が必要。
   PIX-DT195 も、Xit の Windows 用ドライバーを入れていなければ同じ。

2. TVTest の BonDriver のフォルダーに、`BonDriver_DT295.dll`, `BonDriver_DT295.ini` (見本はリリースの zip と `src/dt295-bondriver/BonDriver_DT295.ini`), `dt295.exe`, ファームウェア 2 つを置く。
   ini は DLL と同じ名前にする。
   個体が 1 台なら設定は要らず、複数台のときは `[Tuner] DeviceSerial` に使う個体のシリアル番号を書く。
   BonDriver は同じフォルダーの `dt295.exe` (`dt295 serve`) を起動して個体を扱わせ、TVTest を閉じると `dt295 serve` も終わる。

3. TVTest でドライバーに `BonDriver_DT295.dll` を選び、チャンネルスキャンする。
   地デジと、見本の ini の BS / CS は、TS の全サービスが登録される。

4. 複数台のときは、台数分 DLL と ini を複製し (`BonDriver_DT295-1.dll` と `BonDriver_DT295-1.ini` など)、それぞれの ini の `DeviceSerial` で個体を区別

5. PIX-DT295W と PIX-DT195W の 2 つのチューナーを同時に使うときは、DLL と ini をチューナーごとに複製し (`BonDriver_DT295-1` と `BonDriver_DT295-2` など)、2 つ目の ini の `[Tuner]` に `TunerNumber=2` を書く。
   2 つの TVTest は、同じフォルダーの `dt295 serve` を通して同時に視聴できる。
   チャンネルスキャンは DLL ごとに行う。

6. (任意) BS / CS の局ロゴ。
   通常の視聴では TVTest にほとんど取り込まれないので、`dt295 logo` で受信して TVTest のロゴファイルに追加する:

   ```
   dt295 logo --tvtest C:\TVTest              # 受信して追加する (最長 45 分)
   dt295 logo --tvtest C:\TVTest --schedule   # 毎日 03:55 にタスク スケジューラで実行する
   ```

   ロゴが送出されていない時期は受信できずに終了するので、日を置いて実行する。

更新するときは、TVTest をすべて閉じて (数秒で `dt295 serve` も終わる)、DLL と `dt295.exe` を置き換える (ini と状態ファイルはそのまま使える)。
個体を TVTest の中で直接開いていた版 (0.5.0 まで) から更新するときは、状態ファイルの名前を `BonDriver_DT295.state` から `<シリアル番号>.state` (例 `CD23456789.state`) に変える。
変えなくても動くが、そのときは最初に復号できないことがあり、個体の抜き差しで直る。

詳しくは [docs/usage.md](docs/usage.md) の "BonDriver (TVTest)", "`dt295 driver`", "`dt295 logo`", "困ったとき" セクション。

## Windows: raw-TS モード (任意)

System1 モードがデバイスの中でデスクランブルするのに対し、raw-TS モードはスクランブルされたままの TS を TVTest に渡し、解除は TVTest の CAS プラグインに任せる。
ini の `[Tuner] RawTS=1` で切り替わる (切り替えたら、TVTest をすべて閉じてから開き直す)。内部の仕組みと機能差は [docs/raw-ts.md](docs/raw-ts.md)。

- ファームウェアは要らない。レジデント (チップに常駐させる第三者のコード) も純正ファームも使わず (ユニットのコアと補助コアで動くのも自前のコードのみ)、ホストがチップを cold (電源投入直後) から自力で立ち上げる (完全に自前コードのみ)
- B-CAS の解除は TVTest 側 (CAS プラグインと PC/SC リーダーの契約済みカード) が行う。System1 と違い、デバイスのスロットのカードは使わない
- 最初の選局は cold の個体が必要。閉じて開き直すだけなら差し直しは要らない

## Linux: mirakc から使う

`dt295 serve` (デーモン) が個体を開いたまま保持し、mirakc はチューナーのコマンドとして `dt295 tune` を呼ぶ。
EPGStation は mirakc を Mirakurun として使う。
動作を確認した構成は Arch Linux (mirakc 3.4.90, mirakc-arib 0.24.40)。
コマンドはリポジトリのルート (リリースの tar.gz なら展開したフォルダー) で実行する。
`dt295` のみを別のマシンにコピーするときは、`packaging/` も一緒にコピーする。

パッケージでインストールすると、`/usr/bin/dt295`、dt295 ユーザーと `/var/lib/dt295`、udev のルール、`dt295.service` がインストールされるので、下の手順 1, 3, 4 は不要。
手順 5 は `/etc/dt295.conf` を書いて `systemctl enable --now dt295` するだけでよい。

- リリースのパッケージ: `apt install ./pix-dt295_0.2.0-1_amd64.deb` (Debian / Ubuntu), `dnf install ./pix-dt295-0.2.0-1.x86_64.rpm` (Fedora / RHEL 系), `pacman -U ./pix-dt295-0.2.0-1-x86_64.pkg.tar.zst` (Arch Linux) のようにインストールする。
  更新は新しい版を同じくインストールしてから `systemctl restart dt295` (インストールではデーモンは再起動しない)。
- Arch Linux でリリース前の HEAD を使うとき: リポジトリを clone して `packaging/arch/` で `makepkg -si` とすると、コミット済みの HEAD からパッケージ `pix-dt295-git` をビルドしてインストールできる (リリースの `pix-dt295` とは入れ替わる)。
  更新は `git pull` の後に同じく `makepkg -si` して `systemctl restart dt295`。
  makepkg は PKGBUILD の `pkgver` をビルドしたバージョンに書き換える (この変更はコミットしなくてよい)。

1. `dt295` をインストールする (上の "入手"、"ビルド" セクション):

   ```
   install -m 755 target/release/dt295 /usr/local/bin/dt295   # リリースの tar.gz なら ./dt295
   ```

2. ファームウェア 2 つを `/usr/lib/firmware/dt295/` に配置 (名前は純正のまま)

3. デーモン用のユーザーと、状態ファイルを置くディレクトリを作成する:

   ```
   useradd --system --home-dir /var/lib/dt295 --shell /usr/sbin/nologin dt295
   install -d -o dt295 -g dt295 /var/lib/dt295
   ```

4. dt295 グループとログイン中のユーザーが個体にアクセスできるように、udev のルールを配置する:

   ```
   install -m 644 packaging/linux/60-dt295.rules /etc/udev/rules.d/
   ```

   `dt295 list` に個体のシリアル番号が表示されれば、アクセスできている。

5. `/etc/dt295.conf` に使う個体を書く (個体ごとに `[unit <シリアル番号>]`。ほかの設定は [docs/usage.md](docs/usage.md) の "`dt295 serve`" セクション):

   ```ini
   [unit AB12345678]
   ```

   systemd のユニットは、`packaging/linux/dt295.service` の `ExecStart` のパスを書き換えて配置する:

   ```
   sed 's|^ExecStart=/usr/bin/|ExecStart=/usr/local/bin/|' packaging/linux/dt295.service > /etc/systemd/system/dt295.service
   systemctl enable --now dt295
   journalctl -u dt295                          # "opened and booted" が出れば準備完了
   dt295 tune --serial AB12345678 --ter 557142857 --sid 1024 | head -c 10000000 > /tmp/t.ts   # 受信のテスト
   ```

   状態ファイル (`/var/lib/dt295/<シリアル番号>.state`) は初回の起動時に作成されるので、Windows からコピーする必要はない。

6. mirakc と mirakc-arib をインストールする (手順は mirakc のドキュメントを参照)。
   mirakc のユーザーを dt295 グループに追加してソケットを使えるようにし、dt295 の後に起動するよう設定する:

   ```
   useradd --system --home-dir /var/lib/mirakc --shell /usr/sbin/nologin mirakc
   usermod -aG dt295 mirakc
   install -d -o mirakc -g mirakc /var/lib/mirakc/epg
   ```

   `/etc/systemd/system/mirakc.service.d/dt295.conf`:

   ```ini
   [Unit]
   After=dt295.service
   Wants=dt295.service
   ```

7. mirakc の設定 (`/etc/mirakc/config.yml`) を作成する。
   チャンネルは、録画ソフトが個体を使っていないときにスキャンして作る (6 分ほど):

   ```
   dt295 mirakc-channels --scan --serial AB12345678 -o /tmp/channels.yml
   ```

   設定ファイルに次を書き、その後ろに `/tmp/channels.yml` の内容を追記する:

   ```yaml
   epg:
     cache-dir: /var/lib/mirakc/epg
   server:
     addrs:
       - http: 0.0.0.0:40772
   tuners:
     - name: dt295-AB12345678
       types: [GR, BS, CS]
       command: dt295 tune --serial AB12345678 {{{extra_args}}}
       decoded: true
   ```

   個体が複数なら、`tuners` に個体ごとに 1 つ書く。
   PIX-DT295W と PIX-DT195W は、2 つ目のチューナーを `dt295 tune --serial <シリアル番号> --tuner 2 {{{extra_args}}}` でもう 1 つ書く。
   TVTest のスキャン結果 (ini と .ch2) があれば、`dt295 mirakc-channels --bondriver <ini>` でも作れる。

8. mirakc を起動する。
   最初にサービスと番組表を取得し、1 台で TS ごとの 48 チャンネルの場合は 2 時間 20 分ほどかかる (以後は毎日 8 時と 20 時に取得)。
   `curl localhost:40772/api/services` でサービスの一覧が返れば起動できている。

9. EPGStation の `config.yml` の `mirakurunPath` に `http://<このマシン>:40772/` を設定

10. (任意) 局ロゴ。
    `dt295 logo --mirakc` で PNG を作り、`dt295 mirakc-channels --logos` で mirakc の設定に追加する。
    週に 1 回で十分。
    BS / CS は最長 45 分、地デジは 1 チャンネル 10 分ほどかかるので、録画や mirakc の番組表の取得 (8 時と 20 時から 2 時間 20 分ほど) と重ならない時間にする。
    `packaging/examples/` のスクリプトと systemd の timer (日曜の 3 時) をコピーし、`/usr/local/bin/dt295-logos` の `unit` を使う個体のシリアル番号に書き換えてから有効にする:

    ```
    install -m 755 packaging/examples/dt295-logos /usr/local/bin/
    install -m 644 packaging/examples/dt295-logo.service packaging/examples/dt295-logo.timer /etc/systemd/system/
    install -d -o dt295 -g dt295 /var/lib/dt295/logos
    systemctl enable --now dt295-logo.timer
    ```

    初回の実行が終わったら、mirakc の設定のチャンネルを `dt295 mirakc-channels ... --logos /var/lib/dt295/logos` で作り直し、mirakc を再起動する。

更新するときは、録画中でないこと (`curl localhost:40772/api/tuners` の `isFree`) を確認してから、`dt295` を置き換えて `systemctl restart dt295`。

詳しくは [docs/usage.md](docs/usage.md) の "`dt295 serve`", "`dt295 tune`", "`dt295 mirakc-channels`", "`dt295 logo`" セクション。

## Linux: raw-TS モード (任意)

System1 がデバイスの中でデスクランブルするのに対し、raw-TS モードは純正ファーム無しでスクランブルされたままの TS を出し、B-CAS の解除をホスト側で行う (完全に自前コードのみ。内部は [docs/raw-ts.md](docs/raw-ts.md))。
上の "Linux: mirakc から使う" の構成に、次の違いを加える。

- ファームウェア (手順 2) は要らない
- 解除用に、ホストに PC/SC リーダーと契約済みの B-CAS カード、pcsc-lite (pcscd) と libarib25 (`b25`) が要る

1. `/etc/dt295.conf` の個体に `raw-ts = true` を書く:

   ```ini
   [unit CD23456789]
   raw-ts = true
   ```

2. 解除ツール (pcsc-lite と libarib25) を入れ、pcscd を起動してリーダーと B-CAS カードを認識させる (`pcsc_scan` など)。

3. mirakc は、チューナーを `decoded: false` にし、`filters.decode-filter` に `b25` を設定する (手順 7 の `tuners` を次のようにする):

   ```yaml
   tuners:
     - name: dt295-CD23456789
       types: [GR, BS, CS]
       command: dt295 tune --serial CD23456789 {{{extra_args}}}
       decoded: false
   filters:
     decode-filter:
       command: /usr/bin/b25 /dev/stdin /dev/stdout
   ```

   libarib25 の `b25` は `-` (標準入出力の意味) を受け付けないので、`/dev/stdin /dev/stdout` を渡す。

これで EPGStation からの録画と、ブラウザでの視聴まで、純正ファームを使わずに通る。
