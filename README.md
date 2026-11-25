# pix-dt295

PIXELA PIX-DT295 を純正ドライバーの代わりに動かす、ユーザーモードのドライバーとユーティリティツール

## できること

- Windows: TVTest 用の BonDriver `BonDriver_DT295.dll` (x64 / x86) で、地デジ, BS, 110度CS の視聴とチャンネルスキャン。
  個体は、BonDriver が起動するデーモン `dt295 serve` が扱う。
- Linux: 個体を開いたままにするデーモン `dt295 serve` と、録画ソフトがチューナーのコマンドとして呼ぶ `dt295 tune`
  - TS のキャプチャ
  - 接続中の PIX-DT295 の一覧, BS / CS のチャンネル定義の作り直し
  - 局ロゴの受信, mirakc のチャンネル設定の生成
  - WinUSB への切り替え (Windows のみ)
- raw-TS モード (完全に自前コードのみ): 純正ファーム無しでスクランブルされたままの TS を出し、B-CAS の解除をホスト側 (Windows は TVTest の CAS プラグイン, Linux は mirakc のパイプラインの b25 など) で行う。Windows と Linux のどちらでも使える

## 前提と制約

- 本体のカードリーダーに B-CAS カードの挿入が必要 (raw-TS モードは代わりにホスト側のリーダーのカードを使う)
- チューナー 1 つで、同時に受信できるのは 1 つの TS のみ (選択中の TS のサービスはすべて同時出力可能)
- パラボラアンテナの LNB には給電しない (ハードウェア上の制限のため、もし必要ならブースターを使う)

## ドキュメント

| ファイル | 内容 |
|---|---|
| [INSTALL.md](INSTALL.md) | 導入手順 |
| [docs/usage.md](docs/usage.md) | リファレンス |
| [docs/raw-ts.md](docs/raw-ts.md) | raw-TS モード (完全に自前コードのみ) の内部のまとめ |
| [docs/development.md](docs/development.md) | 開発者向け情報 |
| [docs/device.md](docs/device.md) | 判明しているデバイスの仕様 |

ソースの構成は [docs/development.md](docs/development.md) の "構成" セクションに記載。

## ライセンス

[MIT LICENSE](LICENSE)

ファームウェアは公式サイトで配布されているバイナリを使用し、リポジトリには含めていない。
