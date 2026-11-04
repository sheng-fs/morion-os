<div align="center">

# Morion OS

[English](./README.md) | [简体中文](./README.zh-CN.md) | [日本語](./README.ja.md) | [한국어](./README.ko.md) | [Español](./README.es.md) | [Français](./README.fr.md) | [Deutsch](./README.de.md) | [Русский](./README.ru.md)

---

[![License](https://img.shields.io/badge/license-MIT%20%2B%20Apache--2.0-blue.svg)](./docs/licensing.md)
[![Language](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org)
[![Arch](https://img.shields.io/badge/arch-x86__64%20|%20AArch64%20|%20RISC--V-brightgreen.svg)]()
[![Stage](https://img.shields.io/badge/stage-design%20&%20rewrite-yellow.svg)]()
[![Platform](https://img.shields.io/badge/platform-UEFI-lightgrey.svg)]()
[![Security](https://img.shields.io/badge/security-CHERI%20|%20IOMMU-red.svg)]()
[![PRs](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)]()

</div>

---

## 概要

Morion OS は **Rust** でゼロから構築されたモダンなオペレーティングシステムである。本プロジェクトは現在 **再設計・再実装フェーズ** にあり、以前の実装に存在したアーキテクチャ上の衝突を徹底的に見直し、**マイクロカーネル + exokernel のハイブリッドアーキテクチャ** を中核技術方針として再確立した。

レガシーコードはアーカイブされ（`legacy` ブランチ）、メインラインは再始動した。その後、純粋な設計段階から **実行可能なマイクロカーネルプロトタイプ** へと移行しており、QEMU (UEFI) 上で起動してユーザー空間シェルに到達し、「アプリケーション → libvfs → ファイルシステムサービス → ブロックデバイスサービス → NVMe ディスク」というファイル読み書きの全経路を駆動する。

### 基本原則

- **マイクロカーネルの信頼済みコンピューティングベース**: カーネルが公開する境界はシステムコールインターフェースの 1 つだけであり、ファイルシステム、ネットワーク、デバイスドライバ、グラフィックス、シェル、その他従来のカーネル機能はすべてユーザー空間サービスとして実装される。カーネル内に存在するのはごく少数の中核プリミティブ（IPC、アドレス空間マッピング、保護ドメイン、ケーパビリティ）のみであり、0–63 の全システムコール表の中でもこれらはごく一部にすぎない（[docs/dev-reference.md](./docs/dev-reference.md) §5 参照）。
- **exokernel の高性能パス**: 「パフォーマンスエンクレーブ」機構を通じて、IOMMU / CHERI のハードウェアケーパビリティにより高性能アプリケーション（ゲーム、AI）がハードウェアを直接操作できる — カーネルトラップゼロ、データコピーゼロ。
- **ケーパビリティベースのセキュリティモデル**: 従来の UID/GID 権限システムを廃止する。ケーパビリティを唯一のアクセス資格情報とし、「root は何でもできる」という脆弱性を根本から排除する。
- **Anykernel デュアルモードドライバ**: 同一のドライバソースが、ユーザー空間のサービスプロセス（共有シナリオ）または直接パスライブラリ（高性能シナリオ）のいずれにもコンパイルでき、コードの 90% 以上を共有する。

> 詳細なアーキテクチャ設計: [docs/architecture.md](./docs/architecture.md)。

---

## 現在の状態

本システムは設計ドキュメントから、**実ハードウェア / QEMU 上で起動・動作するマイクロカーネルプロトタイプ** へと移行した。以下の表は「動作中」と「未着手」を区別し、設計目標が既存の能力と誤解されないようにしている。

| 領域 | 状態 | 備考 |
|------|--------|-------|
| マイクロカーネルの中核 | ✅ 動作中 | 保護ドメイン、同期/非同期 IPC、プリエンプティブスケジューリング（**タイムアウト付きブロッキング** を含む）、アドレス空間とデマンドページング、割り込みルーティング（PIC + **LAPIC/MSI-X**、**ブロッキング割り込み待ち** と **マルチベクタ `wait_any`** を含む）、ケーパビリティシステム（**IPC 経由のケーパビリティ転送** を含む） |
| 実ハードウェアでの起動 / 電源 | ✅ 動作中 | UEFI ローダーは起動ボリュームの Device Path から起動ディスクを特定する（`BootInfo` v5）。**`SAFE` バリアントは起動ディスクチェーン上のブロックコントローラのみを公開する**（USB コントローラは決してフィルタされない）。シェルの `poweroff` は ACPI S5（FADT + DSDT `_S5`）を使用し、`reboot` は i8042 → `0xCF9` → triple fault の順にフォールバックする。PS/2 コントローラが存在しない場合は明示的に報告される |
| システムコールインターフェース | ✅ 56（ID 0–63） | カーネルの完全な ID 表: [docs/dev-reference.md](./docs/dev-reference.md) §5、アプリケーション向けサブセット: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §3 |
| ケーパビリティセキュリティモデル | ✅ 動作中 | ケーパビリティスロット + **ケーパビリティハンドル**（オープン時に発行され、すべての I/O の前に検査され、クローズ時に失効する）、デフォルトではケーパビリティゼロ。実行時に IPC 経由で **ハンドル転送**（move）と **ケーパビリティ委譲**（copy、権限の増幅なし）が可能 — 起動時にすべてを静的に付与する必要はない |
| ユーザー空間ドライバ | ✅ 一部実装 | キーボードドライバ（IRQ1 / PS/2 存在プローブ + USB HID ブートキーボード）、ブロックデバイスサービス（NVMe ドライバ、IDE PIO フォールバック付き）、**マルチコントローラ xHCI**（最大 4 コントローラ、USB ストレージ + USB HID キーボード）、ネットワークドライバ **`net_srv`（virtio-net、ドメイン 16）** — N2/N3 ドライバは動作する（モダン virtio の立ち上げ: virtio ケーパビリティの解析 / MAC の読み出し / RX・TX virtqueue の構築 / `DRIVER_OK` / **MSI-X 割り込み駆動の RX**、および自身が発行した ARP 要求にゲートウェイが応答する `NET1 … ARP reply OK`）。**汎用デバイスグラント（D1）**: カーネルの `device.rs` が `DeviceGrant`（BAR + 連続 DMA ブロック + MSI-X パラメータ、デバイス固有の意味論なし）を払い出す。カーネルは **NVMe 固有のコードをもはや含まない** — キューレイアウトとデバイスプロトコルはドライバドメイン内にある。ドライバは `SYS_DEVICE_CONFIG_READ` を使って自身のデバイスの PCI ケーパビリティを解析することもでき、カーネルは `BIR` に従って別の BAR から MSI-X テーブルをマップする |
| ユーザー空間ファイルシステム | ✅ 一部実装 | FAT32（VFAT ロングネームを含む）、tmpfs、独自 MorionFS v2（COW + スナップショット + フリービットマップ/空間再利用 + 大容量ファイル向け間接ブロック + 可変長ディレント/ロングネーム + ノードメタデータ + iノード番号間接化レイヤ/ハードリンク/シンボリックリンク + **ボリュームジオメトリごとのフォーマット** + **明示的な `mkfs.mfs`、マルチボリュームおよびプライマリボリュームの切り替え**）、ext2 **読み取り専用**、exFAT（読み取り + 書き込み、大容量かつ高クラスタ数ボリューム） |
| パーティション / ボリュームレイヤ | ✅ 動作中 | block_srv は各ディスクの **MBR/GPT** パーティションテーブルを解析してボリュームテーブルを構築し、ボリュームごとの先頭シグネチャから FS タイプを判別する。**パーティションテーブルの書き込み** も可能（`part.create/del/wipe/reload`: パーティションの作成/削除、ワイプ、再スキャン — GPT と MBR の両方）。`dev` は現在「ボリューム番号」であり、ブロックレイヤはマルチページ DMA（1 コマンドあたり ≤ 128 KiB）をサポートする。**マルチボリュームマウント**: 同一タイプの追加ボリュームは `/usb<vol>` に自動マウントされるため、1 つのコードベースで複数ディスクに対応できる — 実際の USB パーティション読み取りへの道を開く |
| シェル & 統合ディレクトリツリー | ✅ 動作中 | `help/echo/uname/version/pwd/ls/cat/run/wget/net/ping/ping6/dns/wifi/lspci/dmesg/cd/mkdir/touch/rm/mv/ln/ln -s/chmod/chown/truncate/stat/lstat/readlink/mkfs.mfs/mfs.primary/mfs.snap/mfs.snaps/mfs.rollback/mfs.gc/mfs.fsck/mfs.sync/df/part.create/part.del/part.wipe/part.reload/clear/poweroff/reboot`（`ls -l` ロング形式、シンボリックリンクは `l` として表示、`dmesg` は `-d/-a/-g/path` と `Ctrl+C` をサポート、`uname`/`version` は `SYS_UNAME` 経由でバージョン文字列を報告する）。複数のファイルシステムはマウントレイヤによって単一の `/` ルートに統合され、実行時マウントをサポートする |
| グラフィックス / GUI | 🚧 進行中 | **G1** フレームバッファをユーザー空間の `gfx_srv`（ドメイン 15）に排他的に引き渡す: `Capability::Fb` + `SYS_FB_INFO/MAP/TAKEOVER` を追加し、テイクオーバー後はカーネル端末が画面へ書き込まなくなる（出力は COM1 に残る）。**G2** 描画プリミティブ `fill/rect/blit` + 共有サーフェス + クライアントライブラリ `libmorion::gfx`。`blit` は応答前にフレームバッファを再読み取りして検証する（セルフテスト GS-1）。**G3a** テキスト描画の外部化: フォント（ASCII 8×16 + CJK 16×16 `cjk.bin` ≈276 KB）と端末状態をカーネルから `gfx/` へ移動し、新プロトコル `GFX_OP_TEXT/CLEAR/MOVE/QUERY`、ピクセル単位の書き込み後読み戻し検証（セルフテスト GT-1）。**G3b** シェル出力の画面表示: `SYS_CONSOLE_READY(47)`。`libmorion` の出力シンク `sink()` がプロセス単位で画面コンソールへ出力を **ミラー** できる（オプトインするのはシェルのみ）。**G3c** カーネルが CJK フォントを削除（−276 KB、カーネル ELF 347 KB → 69 KB）。カーネル端末は ASCII + tofu にフォールバックし、CJK 描画はユーザー空間でのみ行われる。**G4** 入力のカーネル外への移動: `SYS_KEY_PUSH(48)/SYS_KEY_READ(49)` によるキーバイトキュー（カーネルはバイトを中継するだけ）、行編集/エコーはクライアントライブラリ `morion::console::readline` 内で行う。**G6** サービスの自己修復: `ipc::call` が永久にハングしなくなり（タイムアウト + 対象に生存タスクがない場合の即時失敗）、フレームバッファをカーネルの予約領域として登録し、再起動時に古いメールボックス要求を破棄し、クライアントが共有セッションを再構築し、`gfx_srv` は init によって監視される（セルフテスト GS-2）。カーネルのテキストコンソールは起動時とパニック時の出力のみに残る |
| ネットワーキング / 仮想化 / エンクレーブ / パッケージマネージャ | 🚧 進行中 | **ネットワーキング**: `net_srv` の virtio-net ドライバは動作する。スタックは専用の `netstack_srv`（ドメイン 21: TCP/IP + ソケットサービス + ポートケーパビリティゲート）に分割され、第 2/第 3 の実 NIC として `e1000e_srv`(22) / `e1000_srv`(24)、無線 `wifi_srv`(25)、ゲスト内 `httpd_srv`(23) がある。**エンクレーブ**: IOMMU/VT-d によるハードウェア分離は完了している（E1c 以降、対象デバイスのウィンドウは制約され、ウィンドウ外の DMA は証拠付きで拒否される）。エンクレーブマネージャ E2/E3 は未完了。**仮想化 & パッケージ管理**: 設計は確定済み、実装は未着手 |
| システム AI ケーパビリティインターフェース | 📐 仕様定義済み | アプリケーションが機能をシステム AI に公開する方法: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9 |

> クイックスタート（ビルド & 実行コマンド）: [docs/commands.md](./docs/commands.md);
> カーネルとインターフェースのリファレンス: [docs/dev-reference.md](./docs/dev-reference.md);
> アプリケーション開発（AI 呼び出し可能なケーパビリティを含む）: [docs/app-dev-guide.md](./docs/app-dev-guide.md);
> ファイルシステムのロードマップ: [docs/roadmap-fs.md](./docs/roadmap-fs.md);
> ドライバ & エンクレーブのロードマップ: [docs/roadmap-driver.md](./docs/roadmap-driver.md)。

---

## バージョン

現在のバージョンは **`0.4.0`**、リリース済みのグラフィックスなしバリエーションは **`v0.4.0-nogui`** というタグが付いている。

- バージョン定数の **唯一の信頼できる情報源（single source of truth）** はカーネルの `kernel/src/version.rs`（`SYSTEM_NAME` / `VERSION` / `MACHINE` / `VARIANT` / `BUILD`）であり、`SYS_UNAME(51)` を介してユーザー空間に報告される — シェルの `uname` / `version` コマンドはこれを読み取る（ビルド番号は `Makefile` によって注入される git の短縮ハッシュである）。
- 2 つのビルドバリエーション:
  - **通常（グラフィックスあり）**: `make ...`。シェル出力はさらに `gfx_srv` の画面コンソールへ **ミラー** される。
  - **グラフィックスなし**: `make NOGUI=1 ...`。シェルは画面ミラーを開かず（入力/エコーはシリアルポートを経由する）、リリース文字列は `0.4.0-nogui` を報告し、成果物は `build/nogui/` に出力される。その他のサービスと完全な回帰ベースラインは変更されない。
- 変更履歴は [CHANGELOG.md](./CHANGELOG.md) を参照。

---

## アーキテクチャ概要（目標）

```
┌──────────────────────────────────────────────────────┐
│                  Application Layer                    │
│    POSIX Interface (libc)  |  High-Perf Direct API    │
├──────────────────────────────────────────────────────┤
│            User-Space System Services                 │
│  Filesystem │ Network Stack │ Device Svc │ Security   │
│  ext4/vfat  │    TCP/IP     │ Driver Svc │ Auth/Audit │
├──────────────────────────────────────────────────────┤
│       Performance Enclave — Optional Acceleration     │
│   GPU Direct  │  NPU Direct  │  Userspace NIC Driver  │
│   (IOMMU-enforced isolation, CHERI bounds protection) │
├──────────────────────────────────────────────────────┤
│                    Microkernel                        │
│  IPC │ Scheduling │ Address Space │ IRQ Route │ Caps  │
└──────────────────────────────────────────────────────┘
```

---

## 中核設計

### マイクロカーネルのプリミティブ

還元不可能な最小集合のみを含む:

| プリミティブ | 説明 |
|-----------|-------------|
| `send` / `receive` / `call` | ケーパビリティ転送を伴う同期/非同期 IPC |
| `map` / `unmap` | アドレス空間マッピングの管理 |
| `create_domain` / `destroy_domain` | 保護ドメイン（プロセス）のライフサイクル |
| `schedule` | CPU スケジューリング |
| `allocate_frame` / `free_frame` | 物理メモリフレームの管理 |
| `register_interrupt` / `ack_interrupt` | 割り込みの認可と確認応答 |
| `create_enclave` | ハードウェア分離エンクレーブの作成 |

### 外部ページャ

- カーネルはページフォールトを捕捉して転送するだけであり、内容と置換ポリシーはユーザー空間のページャサービスが決定する
- 各プロセスは専用のページャを指定できる — デマンドページング、圧縮メモリプール、ネットワークストレージなど

### Anykernel デュアルモードドライバ

| モード | 対象 | 特性 |
|------|--------|-----------------|
| **ドライバサービスプロセス** | 一般アプリケーション | デバイスの共有、安全な分離、IPC 経由の間接アクセス |
| **直接ドライバライブラリ（LibDevice）** | ゲーム / AI | 実行時リンク、MMIO/DMA 操作におけるカーネルトラップゼロ |

単一の Rust trait インターフェース、コンパイル時のフィーチャーフラグによるバックエンド選択、90% 以上のコード共有。

### パフォーマンスエンクレーブ

1. IOMMU がデバイスの MMIO と DMA ウィンドウをプロセスのアドレス空間にマップする
2. LibDevice 直接ドライバライブラリをリンクする
3. GPU/NPU コマンドをカーネルの介入なしに直接発行する

影響範囲はハードウェアによってエンクレーブのリソース境界に固定される。

### ケーパビリティベースのセキュリティモデル

- ケーパビリティを唯一のアクセス資格情報とし、UID/GID に依存しない
- 新規プロセスはケーパビリティゼロで開始し、親プロセスによって明示的に付与される
- POSIX 権限 API（`chmod`/`chown`）は libc によってケーパビリティ操作に変換される
- セキュリティポリシーはユーザー空間のポリシーエンジンによって解釈され、動的更新をサポートする

### ユーザー空間サービス

従来のカーネル機能はすべて独立したユーザー空間プロセスとして動作する:

| サービス | 責務 | 状態 |
|---------|---------------|--------|
| ファイルシステムサービス | FAT32（VFAT ロングネームを含む）、tmpfs、独自 MorionFS、ext2 読み取り専用、exFAT（読み取り + 書き込み）、libvfs による統合 | ✅ 実装済み（ext2 読み取り専用 / exFAT 読み書き） |
| デバイスサービス | ブロックデバイス（NVMe ドライバ）、キーボードドライバ、割り込みディスパッチ | ✅ 一部実装 |
| シェルサービス | コマンドラインインタープリタ + 統合ディレクトリツリー / 実行時マウント | ✅ 実装済み |
| ネットワークスタック | ユーザー空間 TCP/IP、ゼロコピー共有メモリ | ⏳ 計画中 |
| セキュリティ / 監査 | 認証、ポリシーエンジン、侵入検知 | ⏳ 計画中 |
| エンクレーブマネージャ | エンクレーブのライフサイクル、ログストリーム、マイグレーション & サスペンド | ⏳ 計画中 |
| パッケージマネージャ | Nix スタイルの宣言的ビルド、アトミックスイッチング、バージョンロールバック | ⏳ 計画中 |
| GUI サービス | アクリル半透明デスクトップ、高度にカスタマイズ可能（`gfx_srv` のフレームバッファ / テキストコンソール / 描画プリミティブは実装済み、デスクトップ環境は未着手） | 🚧 進行中 |
| オーディオ / IME / コンテナ / 時刻 / 電源 / ログ / 設定 | システム基盤サービス | ⏳ 計画中 |
| AI ケーパビリティ登録 / ゲートウェイサービス | アプリケーションケーパビリティの登録 & 検出、AI 呼び出しの認可と監査 | 📐 仕様定義済み（[app-dev-guide.md](./docs/app-dev-guide.md) §9 参照） |

### 仮想化

- マイクロカーネルはハイパーバイザ（Intel VT-x / AMD-V）としても機能する
- unikernel および未改変の Linux/Windows ゲストをサポートする
- PCIe デバイスパススルー（VT-d / IOMMU）、ネストしたエンクレーブ

### ブート

- UEFI ネイティブ、レガシーなリアルモード遷移なし
- GOP 高解像度ブートメニュー、アクリルテーマ
- Nix クロージャベースのブートエントリ、アトミックスイッチング & ロールバック
- TPM 2.0 による測定 + Secure Boot 検証
- kexec ウォームブート、マルチ OS 共存

---

## リポジトリ構造

### 現在の構造

本プロジェクトは Rust ワークスペース（ルートの `Cargo.toml`）であり、現在 `boot`、`kernel`、`user/srv`、`user/libmorion`、`user/libdevice`、`user/hello`、`kernel_test` のクレートを含む。カーネル外のシステムサービス（ブロックデバイス / ファイルシステム / マウント / シェル / キーボードドライバ、…）はすべて **独立したユーザー空間プログラム** である（`user/srv` 内のサービスごとに 1 つの `[[bin]]` → 1 つのスタンドアロン ELF）。**ブートローダーは起動時に ESP（`EFI/morion/services/`）からそれらを読み込み**、`BootInfo` のモジュール表を介してカーネルに渡し、その後カーネルは各々を固定ドメインにロードする（E2b: ドメイン ID でディスパッチする単一のフラットバイナリではなくなった。E3b: サービスはカーネルイメージに埋め込まれなくなった）。UI 資産は用途別に整理されている: 起動時リソースは `boot/loader/resources/` 配下、システム全体のリソースは `resources/system/` 配下。

```
.
├── .github/
│   ├── workflows/            # GitHub Actions（Gitee への自動同期）
│   ├── ISSUE_TEMPLATE/       # Issue テンプレート
│   └── PULL_REQUEST_TEMPLATE.md
├── boot/                     # UEFI ブートローダー（morion-boot）
│   ├── asm/
│   │   └── boot_stub.asm     #   ブートエントリのアセンブリスタブ
│   ├── loader/               # ブート時のリソースと設定
│   │   ├── entries/          #   ブートエントリ（.conf）
│   │   │   └── morion.conf
│   │   ├── resources/        #   ブートローダーのテーマ資産（BMP/PNG）
│   │   │   ├── animation/    #     ローディングアニメーションのフレーム
│   │   │   ├── background/   #     背景（dark/light/default/mask）
│   │   │   ├── icons/        #     カテゴリアイコン（dialog/power/security/system/ui）
│   │   │   ├── logo/         #     ロゴバリエーション（horizontal/monochrome/square/system）
│   │   │   ├── progress/     #     プログレスバー（bar_bg/bar_fill）
│   │   │   └── splash/       #     スプラッシュ画面（background/logo）
│   │   ├── kernel_placeholder.bin
│   │   ├── loader.conf       #   ブートローダー設定
│   │   └── theme.toml        #   アクリルテーマ設定
│   └── src/
│       ├── boot/             #   ブートフロー（loader/menu/kexec）
│       ├── config/           #   設定解析（entries/theme）
│       ├── gfx/              #   グラフィックス描画（framebuffer/font/renderer/animation）
│       ├── security/         #   セキュリティ（hash/secure_boot/tpm）
│       ├── lib.rs
│       └── main.rs
├── kernel/                   # マイクロカーネル（morion-kernel、最小 TCB）
│   └── src/
│       ├── arch/             #   x86_64（gdt/idt/pic/pit/keyboard/pci）
│       ├── memory/           #   メモリ管理（paging/frame_allocator）
│       ├── scheduler/        #   スケジューラ（コンテキストスイッチ）
│       ├── video/            #   カーネルテキストコンソール（framebuffer/font/logo/bg/unicode）— 起動時とパニック時の出力
│       ├── bootinfo.rs       #   ブート情報（メモリマップ + GOP フレームバッファ）
│       ├── cap.rs            #   ケーパビリティシステム（スロット + ハンドルテーブル）
│       ├── domain.rs         #   保護ドメイン（プロセス、ドメインごとのページテーブル）
│       ├── elf.rs            #   ELF64 の解析と検証（実行ファイル読み込みの信頼境界）
│       ├── exec.rs           #   実行時に ELF を読み込み → ドメイン作成 → マップ → タスク開始
│       ├── ipc.rs            #   プロセス間通信
│       ├── irq.rs            #   割り込みルーティング（割り込みを IPC として扱う）
│       ├── device.rs         #   汎用デバイスグラント（BAR / DMA / MSI-X → DeviceGrant）
│       ├── pager.rs          #   ユーザー空間ページャインターフェース
│       ├── syscall.rs        #   システムコールエントリと ID 表
│       ├── lib.rs
│       └── main.rs
├── user/                     # ユーザー空間: ランタイム + ドライバ共通ライブラリ + サービス
│   ├── libmorion/            #   ランタイム（クレート `morion`）: syscall / print / libvfs / libgfx / エントリの定型コード
│   ├── libdevice/            #   ドライバ共通ライブラリ（クレート `libdevice`、D2/D2b）: デバイスグラント / MMIO / MSI-X / virtio トランスポート + vring
│   ├── hello/                #   デモ: **スタンドアロンの ELF プログラム**（実行時に SYS_SPAWN_ELF で読み込まれる）
│   └── srv/                  #   システムサービス（クレート `morion-srv`）: サービスごとに 1 つの [[bin]] → 1 つのスタンドアロン ELF
│       └── src/
│           ├── common.rs     #     サービス間で共有されるワイヤプロトコル / ブロッククライアント / 名前ヘルパー
│           ├── block_srv.rs  #     ドメイン 5  ブロックデバイスドライバサービス
│           ├── fat32_srv.rs  #     ドメイン 6  FAT32 ファイルシステムサービス
│           ├── app.rs        #     ドメイン 7  セルフテストプログラム
│           ├── shell.rs      #     ドメイン 8  コマンドライン
│           ├── mount_srv.rs  #     ドメイン 9  マウントサービス
│           ├── tmpfs_srv.rs  #     ドメイン 10 インメモリファイルシステム
│           ├── mfs_srv.rs    #     ドメイン 11 MorionFS
│           ├── ext2_srv.rs   #     ドメイン 12 ext2 読み取り専用
│           ├── exfat_srv.rs  #     ドメイン 13 exFAT 読み書き
│           ├── init.rs       #     ドメイン 14 スーパーバイザ（サービスドメインを監視し、メモリイメージからその場で再起動）
│           ├── gfx_srv.rs    #     ドメイン 15 グラフィックスサービス（フレームバッファを保持し、ユーザー空間で描画: プリミティブ + テキスト端末）
│           ├── net_srv.rs    #     ドメイン 16 ネットワークドライバ（virtio-net; N0–N3: MSI-X 割り込み + ARP セルフテスト）
│           ├── virtio_blk_srv.rs  # ドメイン 17 virtio-blk ブロックドライバ（D3: 汎用グラント、読み取りシグネチャ/書き込み読み戻しセルフテスト）
│           ├── gfx/          #     グラフィックスサービス内部: フレームバッファビュー + フォント（font/glyphs/cjk.bin）+ 端末
│           ├── sender.rs / receiver.rs / pager.rs / echo.rs / kbd.rs  # ドメイン 0..4 デモ & キーボード
│           └── bin/          #     18 個のエントリポイント（各々が morion_main を自身のモジュールの run() に割り当てる）
├── kernel_test/              # 初期ブート統合テスト用カーネル（一時的に保持）
│   └── src/main.rs
├── resources/
│   └── system/               # システム全体のリソース
│       ├── device/           #   デバイスアイコン（.ico）
│       ├── file/             #   ファイルタイプアイコン（.ico）
│       ├── github/           #   GitHub カバー（.png）
│       ├── icons/            #   汎用 UI アイコン（.ico）
│       ├── logo/             #   システムロゴ（.ico/.svg/.png）
│       ├── service/          #   サービスアイコン（.ico）
│       └── terminal/         #   ターミナル背景（.raw）
├── docs/
│   ├── architecture.md       #   技術アーキテクチャ設計
│   ├── app-dev-guide.md      #   アプリケーション開発ガイド（AI 呼び出し可能なケーパビリティ仕様を含む）
│   ├── dev-reference.md      #   カーネル & インターフェースのクイックリファレンス
│   ├── commands.md           #   ビルド / 実行 / 検証コマンド
│   ├── shell-reference.md    #   シェル使用リファレンス
│   └── roadmap-fs.md         #   ファイルシステムロードマップ
├── Cargo.toml                # Rust ワークスペース（boot/kernel/user/kernel_test）
├── Cargo.lock
├── Makefile                  # ビルドシステム（make iso/run/run-nvme/debug/check/clippy）
├── flake.nix                 # Nix ビルド統合
├── rust-toolchain.toml       # Rust nightly ツールチェーン
├── linker.ld                 # カーネルリンカスクリプト
├── .gitattributes
├── .gitignore
├── CONTRIBUTING.md
├── LICENSE
├── README.md                 # 英語（メイン）
├── README.zh-CN.md           # 简体中文
├── README.ja.md              # 日本語
├── README.ko.md              # 한국어
├── README.es.md              # Español
├── README.fr.md              # Français
├── README.de.md              # Deutsch
└── README.ru.md              # Русский
```

### 目標とする開発構造

```
├── bootloader/       # ブートローダー（UEFI）
├── kernel/           # カーネルソース
│   ├── arch/         #   アーキテクチャ固有（x86_64 / AArch64 / RISC-V）
│   ├── core/         #   マイクロカーネルの中核（IPC、スケジューラ、アドレス空間、ケーパビリティ）
│   └── compat/       #   互換レイヤ（POSIX / Linux / RTOS）
├── services/         # ユーザー空間のシステムサービス
│   ├── fs/           #   ファイルシステムサービス
│   ├── net/          #   ネットワークスタック
│   ├── device/       #   デバイスサービス + ドライバ
│   ├── security/     #   セキュリティ / 認証 / 監査
│   ├── enclave/      #   エンクレーブマネージャ
│   ├── gui/          #   GUI サービス
│   ├── shell/        #   シェルサービス
│   ├── audio/        #   オーディオサービス
│   ├── ime/          #   IME サービス
│   └── ...           #   その他のサービス
├── userland/         # ユーザー空間
│   ├── libs/         #   libc、libvfs、libdevice など
│   └── bin/          #   基本コマンド（ls、cat、mkdir、rm）
├── resources/        # リソースファイル
├── docs/             # ドキュメント
└── pkg/              # パッケージ管理（Nix スタイル）
```

---

## ロードマップ

> 詳細なファイルシステムのロードマップ: [docs/roadmap-fs.md](./docs/roadmap-fs.md)、グラフィックスサブシステムのロードマップ: [docs/roadmap-gfx.md](./docs/roadmap-gfx.md)。チェック済みの項目は **QEMU 上での動作を検証済み** である。

### フェーズ 1 — マイクロカーネルの中核（ほぼ完了）

- [x] 保護ドメイン（プロセス）の作成とアドレス空間の分離
- [x] 同期/非同期 IPC（`send` / `recv` / `call` / `reply`、96 バイトのペイロード + 大量データ向け共有メモリ）
- [x] タスクスケジューリングとコンテキストスイッチ
- [x] アドレス空間の map/unmap + ユーザー空間ページャ（デマンドページング）
- [x] 割り込みルーティング（「割り込みを IPC として扱う」）+ MMIO / I/O ポートグラント
- [x] ケーパビリティシステム（スロット + ハンドル: 発行 / 検査 / 失効）
- [x] **IPC 経由のケーパビリティ転送**（ハンドル転送 `SYS_HANDLE_SEND`: オープン済みオブジェクトを **対象ドメインへ** 移動、move セマンティクス。ケーパビリティ委譲 `SYS_CAP_SEND`: 保持しているケーパビリティを **コピー**、権限増幅なし。いずれも `SendTo(to)` を必要とする）。ドメイン 0/1/3 が起動時に 8 つの肯定/否定セルフテストケースをカバーする

### フェーズ 2 — 基盤サービス（進行中）

- [x] PCI 列挙 + ユーザー空間 NVMe ドライバ（ブロックデバイスサービス、IDE PIO フォールバック付き）
- [x] MBR/GPT の **パーティション解析 + ボリュームレイヤ**（block_srv 内部、`dev` = ボリューム番号、FS タイプはボリュームごとの先頭シグネチャで判別）
- [x] **マルチボリュームマウント**: 要求タグの上位 32 ビットがボリュームコードを運び、1 回のオープンが 1 つのボリュームに束縛される。各 FS サービスは自身の追加ボリュームを自動報告する（`/usb<vol>` にマウント）。fat32 のクラスタバッファ 2 → 16 ページ（**64 KiB** のクラスタ上限）、ext2 のブロックグループ上限 16 → 4096
- [x] ファイルシステム: FAT32（VFAT ロングネームを含む）/ tmpfs / 独自 MorionFS（COW + スナップショット）
- [x] ext2 **読み取り専用** 互換（既存の Linux パーティションをマウント）
- [x] exFAT 互換（`exfat_srv` ドメイン 13、`/usb` にマウント）: **読み取り専用** + **読み書き**（`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`）+ **大容量ボリューム**（マルチページ DMA、1 コマンドあたり ≤ 128 KiB）
- [x] libvfs + マウントレイヤ: 複数のファイルシステムを単一の `/` ルートに統合、実行時マウントをサポート
- [x] シェル（ビルトイン + cwd 相対パス）とユーザー空間キーボードドライバ
- [x] **MorionFS v2 フォーマットの確定 + 空間再利用 / GC**（フリービットマップ + mark & sweep）
- [x] **MorionFS の大容量ファイル**（`MFS3`: 間接ブロック、旧来の ≈4 MiB 上限を打破）
- [x] **MorionFS のディレクトリ & ロングネーム**（`MFS4`: 可変長ディレント + 拡張ブロック、名前は ≤255 バイト）
- [x] **MorionFS のノードメタデータ**（`MFS5`: タイムスタンプ / 権限 / 所有者 / リンク数、`rename` / `truncate` / `chmod`）
- [x] **MorionFS の iノード番号間接化 + ハードリンク**（`MFS6`）
- [x] **MorionFS のシンボリックリンク**（`MFSL`: 追従を伴うパス解決 + ループ保護、`readlink` / `lstat` / `ln -s`）
- [x] **MorionFS のボリュームジオメトリごとのフォーマット**（**M7**）
- [x] **MorionFS の明示的フォーマット + マルチボリューム**（**M8 / S2**: `mkfs.mfs <vol>`、ガードは空白または MFS ボリュームのみを受け付ける）
- [x] **MorionFS の容量スケールアップ（ビットマップの外部化）**（**S3a**: 容量上限 ≈119 MiB → **≈127.25 GiB**）
- [x] **MorionFS の 4 GiB を超える単一ファイル**（**S3b**: VFS の `offset`/`size` をエンドツーエンドで u64 化 + 第三レベル間接ブロック `MFI3`）
- [x] **MorionFS のプライマリボリューム切り替え**（**S2**: スーパーブロックがプライマリボリュームのシリアルを保持し、最大のシリアルが優先される）
- [x] **マーカーのみによる MorionFS プライマリボリュームの再ポイント**（**S2**: データに触れずに `mfs.primary <vol>`）
- [x] **`df` による空間使用量**（MorionFS は合計 / 使用済み / 空きを報告する）
- [x] **ボリューム管理の総仕上げ: パーティションテーブルの書き込み**（**S2**: `part.create/del/wipe/reload`、GPT + MBR、ホストの `sgdisk -v` に対して検証済み）
- [x] **NVMe 割り込み（MSI/MSI-X）**（**S3**: LAPIC の最小サポート + PCI ケーパビリティウォーク + MSI ベクタセグメント `0x50..0x5F`）
- [x] **ブロッキング割り込み待ち（wait プリミティブ）**（**S4**: タイムアウト付きブロッキング + `SYS_IRQ_WAIT`）
- [x] **マルチベクタ + `wait_any`**（**S5**: ドメインごとの待機キー + ベクタマスク、管理キュー + 2 つの I/O キューが各々独自のベクタを持つ）
- [x] **ターミナルの CJK 描画（CJK / 全角 / 混在幅）**（カーネル側は後に G3c で削除され、フォントはユーザー空間の `gfx_srv` へ移された）
- [x] **実行ファイルの読み込み（ELF + 実行時の spawn）**（**E1**: ELF64 ローダー + `SYS_SPAWN_ELF`、デモ `user/hello` はスタンドアロン ELF）
- [x] **ユーザー空間ランタイム libmorion + `run` コマンド**（**E2a**）
- [x] **サービスをスタンドアロンプログラムに分割 + ドメイン破棄 / フレーム再利用**（**E2b**: 14 サービス、各々 1 ELF。`SYS_DOMAIN_DESTROY/COUNT/FRAME_FREE`）
- [x] **サービスライフサイクルの総仕上げ**（**E3a/E3b/E3c**: ユーザーページの W^X、サービスのカーネルイメージ外への移動、`init` スーパーバイザ + メモリイメージからのその場再起動、ディスク依存なし）
- [x] **グラフィックス G1（フレームバッファのユーザー空間への移動）**
- [x] **グラフィックス G2（描画プリミティブ + 共有サーフェス + `libmorion::gfx`）**
- [x] **グラフィックス G3a（サービス内端末 + テキスト描画の外部化）**
- [x] **グラフィックス G3b（シェル出力の画面表示）**
- [x] **グラフィックス G3c（カーネルの CJK フォント削除、−276 KB）**
- [x] **グラフィックス G4（入力のカーネル外への移動）**
- [x] **グラフィックス G6（gfx_srv の自己修復: 監視付き再起動 + クライアントセッションの再構築）**
- [ ] **さらなるファイルシステム互換性**（ext4 書き込み、UDF など）
- [ ] ネットワークスタック

### フェーズ 3 — パフォーマンスエンクレーブ（進行中）

- [x] **IOMMU パススルー**（**E1a** ACPI DMAR プローブ / **E1b** ルートテーブル + コンテキストテーブル + アイデンティティ第 2 レベルページテーブルと `GCMD.TE` / **E1c** 対象デバイスウィンドウを `[0, 3 GiB)` に狭め、ウィンドウ外のデバイス DMA を IOMMU が証拠付きで拒否 — すべて完了、[docs/roadmap-driver.md](./docs/roadmap-driver.md) 参照）
- [ ] **LibDevice パススルー形式 & エンクレーブマネージャ**（E2/E3 は保留中。共通ライブラリ `user/libdevice` はすでに存在する）

### フェーズ 4 — ネットワーキング & セキュリティ（進行中）

- [x] **NIC ドライバ（virtio-net）**（`net_srv` ドメイン 16 + 汎用デバイスグラント、[docs/roadmap-driver.md](./docs/roadmap-driver.md) N0–N3 参照: virtio-modern の立ち上げ + MSI-X 割り込み RX + ARP エンドツーエンドセルフテスト）
- [x] **2 つ目の実ドライバ（virtio-blk）**（`virtio_blk_srv` ドメイン 17、引き続き汎用デバイスグラント経由で、デバイス固有のカーネルロジックなし: D3 — 読み取りシグネチャ / 書き込み読み戻しセルフテスト）
- [x] **実行時デバイスグラント（D1b）**（`SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT` + `Mmio` ケーパビリティゲート。NVMe / `net_srv` / `virtio_blk_srv` は実行時システムコールで `DeviceGrant` を取得し、挙動は変わらない）
- [ ] TCP/IP スタック、ケーパビリティ監査、ポリシーエンジン

### フェーズ 5 — GUI & エコシステム（未着手）

- [ ] デスクトップ環境、パッケージマネージャ、仮想化

### 横断項目 — システム AI ケーパビリティインターフェース（仕様定義済み、実装未着手）

- [x] 設計仕様: アプリが関数を **AI 呼び出し可能なケーパビリティ** として注釈 / 記述する方法（[docs/app-dev-guide.md](./docs/app-dev-guide.md) §9 参照）
- [ ] ケーパビリティの登録 & 検出サービス
- [ ] AI 呼び出しゲートウェイ（ケーパビリティ検査 / 監査ログ / タイムアウト & クォータ）

---

## ライセンス

本プロジェクトは **モジュールごとのライセンス** を採用している:

- コア（`boot/`、`kernel/`、`user/`）: [MIT](./LICENSE)
- エンクレーブモジュール（`enclave/`）: [Apache-2.0](./LICENSES/Apache-2.0.txt)、明示的な特許許諾と特許訴訟の終了条項を含む
- サードパーティのドライバ / ファームウェア: 元のライセンスを保持する。[docs/licensing.md](./docs/licensing.md) を参照

正式な表現: **Morion OS のコアは MIT ライセンス、エンクレーブモジュールは Apache-2.0、サードパーティのドライバとファームウェアはそれぞれのライセンスを保持する。** 本リポジトリは MIT の単一ライセンスではない。

---

## 連絡先

- プロジェクトホームページ: [github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- メール: 3555679134@qq.com
