# dualslint — UIスレッドとRenderスレッドを分離したslintのフォーク

このリポジトリは [slint-ui/slint](https://github.com/slint-ui/slint) のフォークです。フォークの変更は `i-slint-backend-winit`
（`internal/backends/winit`）のみです。

提供する API は上流の Slint に準拠しています。フォークは上流の公開 API を削除・変更せず、描画スレッドに関するAPIを追加しています。

## このフォークの目的
GUI は人間とコンピュータの間の情報伝達のための手段です。その中でも、アニメーションを適切に使えば、人間から見える情報は増えます。
そのようなとき、コンピュータがアニメーションを描き続ける処理（出力）とマウス・キーボード操作の処理（入力）を並列に処理したいことがあるはずです。
ゲームやWebではUIと描画のスレッド分離は一般的かもしれませんが、ボタンやテキストボックスを使ったツール開発ではスレッド分離は珍しいでしょう。
したがって、**軽量かつ2スレッドなGUIを構築する**、というのが、このフォークの目的となります。

## 上流slintからの変更点
Rust製で軽量なGUIライブラリであるslintですが、標準の slint は、描画（シーングラフ・GL コンテキスト・プレゼンタ）を必ず UI スレッド（ウィンドウを
作ったスレッド）に縛ります。本フォークは、UI スレッドのイベントループが入力処理を担当したまま、別の描画スレッドが画面全体の描画を担当します。変更ファイルは以下の通りです。

| # | ファイル | 変更 |
|---|---------|------|
| 1 | `internal/backends/winit/render_thread.rs` | **新規追加（本体）** … フル・オフ UI スレッド描画スタック |
| 2 | `internal/backends/winit/winitwindowadapter.rs` | `request_redraw()` をフレームスロットル経由にせず winit へ直接転送 |
| 3 | `internal/backends/winit/lib.rs` | `render_thread` モジュールを公開、イベントフィルタ登録をフォークの内部アダプタにも対応 |
| 4 | `internal/backends/winit/Cargo.toml` | `glow` 依存追加、`windows` クレート機能に `Win32_Graphics_OpenGL` / `Win32_Foundation` 追加 |
| 5 | `internal/backends/winit/frame_throttle.rs`（`apple_display_link.rs` 含む） | **削除** … UI スレッド側のリフレッシュレートスロットル（ui=render 前提）。描画ペーシングはレンダースレッドが担当するため不要 |

### 1. フル・オフ UI スレッド描画（`render_thread.rs`）

UI スレッドが winit のイベントループとネイティブウィンドウ（HWND）を所有し、レンダースレッドが
Slint のシーングラフ全体・WGL コンテキスト・プレゼンタを所有します。

- UI スレッド → レンダースレッドへは **mpsc チャネル** で「winit イベント」「ユーザーコールバック」
  「再描画要求」「ピクセル描画クロージャー」を送る
- レンダースレッドは受け取った **HWND 上に直接 WGL コンテキストを張り**、`FemtoVGRenderer` を構築
- アイドル中はチャネル上でブロック（CPU ほぼ 0）。アニメーション中のみ 16ms 周期で起床
- アプリは `RenderHost::paint(w, h, |target| …)` のクロージャー内で CPU バッファへ RGBA を描き、
  `mark_dirty`/`present` を呼ぶ。**描画中は `&mut PixelTarget` が排他で貸し出される**ため、
  同時ペインターによる競合が型レベルで不可能

### 2. `request_redraw()` の直接転送

標準の `WinitWindowAdapter::request_redraw()` は `pending_redraw` の合流とリフレッシュレートの
スロットル（`frame_throttle` モジュール）を行い、スロットリング中は要求を黙って捨てます。これは
**UI スレッドが描画も担当する前提**の設計で、本フォークでは描画のペーシングをレンダースレッドが
行うため、このスロットル機構（`frame_throttle.rs` と `apple_display_link.rs`）は丸ごと削除しました
（変更表 #5）。フォークでは毎回 `window.request_redraw()` を直接呼ぶため、**すべての
`request_redraw()` が同じイベントサイクル内で `RedrawRequested → draw` になります**（合成を
ネイティブ面に委譲）。

### 3. `with_window_event_handler()` の拡張

`with_window_event_handler()` が、標準の `WinitWindowAdapter` に加えてフォークの内部アダプタ
（`pub(crate)` の `HwndWindowAdapter`）でもイベントフィルタを登録できるようにしました。
シグネチャは不変で、既存の `WinitWindowAdapter` 向けの動作もそのままです。

---

## 追加された API

`i-slint-backend-winit` に `render_thread` モジュール（`pub mod render_thread;`）が新規公開されます。
このモジュールの公開 API は以下の **5 項目**です。

### ① `channel()` — チャネル生成

```rust
pub fn channel() -> (RenderHost, mpsc::Receiver<RenderMessage>)
```

UI スレッドが `RenderHost`（送信側）、レンダースレッドが `RenderMessage` の受信側を持ちます。

### ② `RenderHost` — 送信側ハンドル（`#[derive(Clone)]`）

```rust
pub struct RenderHost {
    sender: mpsc::Sender<RenderMessage>,   // フィールドは非公開
}
```

`Clone`・`Send + Sync`。UI スレッドや任意のワーカースレッドが持って、レンダースレッドへ要求を送ります。

| メソッド | シグネチャ | 説明 |
|---|---|---|
| `send_winit` | `pub fn send_winit(&self, event: winit::event::WindowEvent)` | winit ウィンドウイベントをシーングラフへ転送 |
| `send_user` | `pub fn send_user(&self, f: impl FnOnce() + Send + 'static)` | 任意クロージャーをレンダースレッドで実行 |
| `send_quit` | `pub fn send_quit(&self)` | イベントループを終了 |
| `send_redraw` | `pub fn send_redraw(&self)` | 再描画要求 |
| `paint` | `pub fn paint<F>(&self, width: u32, height: u32, f: F)`<br/>`where F: FnOnce(&mut PixelTarget) + Send + 'static` | ピクセル描画を予約。先にダブルバッファを `width`×`height` で（再）生成し、レンダースレッド上で**排他 `&mut PixelTarget`** を貸して `f` を実行 |

### ③ `RenderMessage` — チャネルのメッセージ型

```rust
pub enum RenderMessage {
    Winit(winit::event::WindowEvent),
    User(Box<dyn FnOnce() + Send>),
    Redraw,
    Paint { width: u32, height: u32, f: Box<dyn FnOnce(&mut PixelTarget) + Send> },
    Quit,
}
```

アプリが自分で構築する必要はありません。`channel()` の受信側を `RenderThreadPlatform::new`
へそのまま渡します（`derive` なし）。

### ④ `PixelTarget` — 描画先（レンダースレッド専用・`!Send`）

```rust
pub struct PixelTarget { /* … 非公開フィールド */ }
```

`RenderHost::paint` のクロージャーが受け取る描画先です。**変更系メソッドはすべて `&mut self`**のため、
同時に 2 箇所から書き込むことは型レベルで不可能です。バッファは RGBA8・row-major・`width * height * 4`
バイト（row 0 = 上）。`Send` ではないのでレンダースレッド専用です。

| メソッド | シグネチャ | 説明 |
|---|---|---|
| `width` | `pub fn width(&self) -> u32` | バッファ/テクスチャの幅 |
| `height` | `pub fn height(&self) -> u32` | バッファ/テクスチャの高さ |
| `bytes_mut` | `pub fn bytes_mut(&mut self) -> &mut [u8]` | ピクセルバッファへ直接書き込み |
| `mark_dirty` | `pub fn mark_dirty(&mut self, x: u32, y: u32, w: u32, h: u32)` | 更新領域を登録（`glTexSubImage2D` で差分アップロード） |
| `mark_whole_dirty` | `pub fn mark_whole_dirty(&mut self)` | 全面を更新領域に |
| `present` | `pub fn present(&mut self)` | フレームを確定。sink 経由で `Image` に差し替え、次フレーム描画直前にダブルバッファへアップロード |

### ⑤ `RenderThreadPlatform` — レンダースレッド用プラットフォーム（`#[derive(Clone)]`）

```rust
pub struct RenderThreadPlatform { /* … 非公開フィールド */ }
```

`i_slint_core::platform::Platform` を実装します。レンダースレッド上で
`SlintContext::new(Box::new(platform))` に渡して使います。

| メソッド | シグネチャ | 説明 |
|---|---|---|
| `new` | `pub fn new(hwnd: isize, size: PhysicalSize, host: RenderHost, rx: mpsc::Receiver<RenderMessage>) -> Self` | HWND・初期サイズ・`channel()` の 2 要素から構築（`PhysicalSize` は `i_slint_core::api::PhysicalSize`） |
| `set_image_sink` | `pub fn set_image_sink<F>(&self, sink: F)`<br/>`where F: Fn(Image) + Send + 'static` | 確定されたフレームをシーングラフに届けるクロージャーを登録（例: `ui.set_xxx_image(image)` を弱参照で呼ぶ） |
| `host` | `pub fn host(&self) -> RenderHost` | 任意スレッドから `paint` するための `RenderHost` を取り出す |

`Platform` 実装として `create_window_adapter()` と `run_event_loop()`（実体はチャネルポンプ）を提供し、
`new_event_loop_proxy()` は `send_user` へ接続されます。

---

## 上流 API との関係

本フォークの差分は上記に尽きます。確認の基準:

- 上流 master（commit `8dce1c42`）との差分は `internal/backends/winit` 内の **6 ファイル** のみ
  （新規 `render_thread.rs`、変更 `lib.rs` / `winitwindowadapter.rs` / `Cargo.toml`、削除
  `frame_throttle.rs` / `apple_display_link.rs`）。`Cargo.lock` の追記（`glow`）と本 README 以外に
  上流から変更したものはありません。
- **削除された public API はありません。** `render_thread` モジュールと上記 5 項目が追加されただけで、
  既存の `WinitWindowAdapter`・`Platform`・`with_window_event_handler()` 等はシグネチャを保ったまま動作します。
- 動作差は以下の 2 点のみ（いずれもシグネチャ不変）:
  - `WinitWindowAdapter::request_redraw()` … スロットル/合流をやめ、毎回 winit へ直接転送
  - `WinitWindowAccessor::with_window_event_handler()` … フォークの内部アダプタでもフィルタが効くよう拡張

---

## 追加された依存関係（`internal/backends/winit/Cargo.toml`）

- `glow = { workspace = true }`（version 0.18）
- `windows` クレートの機能に `Win32_Graphics_OpenGL` と `Win32_Foundation` を追加

---

## アプリとの統合方法

`dualslint` は公開時に **1 つのクレート** として crates.io に公開される想定です。アプリは `slint` を
依存に追加するのと同じ要領で、`slint` の代わりに `dualslint` を追加するだけです。

```toml
[dependencies]
dualslint = { version = "1.18.0", features = ["renderer-femtovg"] }
```

`dualslint` は slint の公開 API（`.slint` のコンパイル、`slint` 相当の各モジュール、`Image` 等の型）を
そのまま提供し、内部の winit バックエンドだけを本フォーク版（`render_thread` モジュール入り）に
差し替えています。ここまでに解説した `render_thread` の各 API は `dualslint::render_thread` から
使えます。

利用コード（抜粋）:

```rust
use dualslint::render_thread::{channel, RenderHost, RenderThreadPlatform};

let (host, rx) = channel();                                  // チャネル生成
let platform = RenderThreadPlatform::new(hwnd, size, host.clone(), rx);
platform.set_image_sink(move |image: dualslint::Image| {      // フレームを Image プロパティへ
    ui.set_xxx_image(image);
});
let ctx = i_slint_core::SlintContext::new(Box::new(platform));
let ui = MainWindow::new_with_context(ctx.clone())?;          // build.rs で
                                                              // SLINT_ENABLE_EXPERIMENTAL_FEATURES=1
// … 任意のスレッドから …
host.paint(w, h, |target| {
    target.bytes_mut().copy_from_slice(&rgba);
    target.mark_whole_dirty();
    target.present();
});
ctx.run_event_loop()
```

> **公開前（現在）の利用方法** — 未公開の間は、フォークの `i-slint-backend-winit` を
> `[patch.crates-io]` で差し替えて使います。ただしフォークの `i-slint-backend-winit` は
> `i-slint-core` / `i-slint-renderer-femtovg` を workspace path 依存で参照するため、同じ git ツリー
> から **3 クレートをまとめて** 差し替えてください（crates.io 版と git 版の `i_slint_core` が別
> インスタンスになると型不一致のコンパイルエラーになります）。詳細はブランチの状況で変わりますので、
> 利用時のコミット・ブランチに合わせてください。

---

## ベース情報

| 項目 | 値 |
|---|---|
| 上流リポジトリ | https://github.com/slint-ui/slint |
| ベース | `master`（commit `8dce1c4265d7ade881d8b2d5ec6c8bc3c228868c`、2026-09-12、version `1.18.0`） |
| 公開先 | https://github.com/akenejie/dualslint |
| フォークブランチ | `main` |
| 変更対象 | `internal/backends/winit`（`i-slint-backend-winit`） |

モノレポ内パスと crates.io パッケージ名の対応:

| モノレポ内パス | crates.io パッケージ |
|---|---|
| `internal/backends/winit` | `i-slint-backend-winit` |
| `internal/renderers/femtovg` | `i-slint-renderer-femtovg` |
| `internal/core` | `i-slint-core` |

---

## ライセンス

本リポジトリは上流 [slint-ui/slint](https://github.com/slint-ui/slint) をもとにしています。

- **上流のコード**（`render_thread.rs` 以外）は上流 slint のライセンス
  （`GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`）に従います。
  各ファイルの SPDX ヘッダーと各クレート内の `LICENSES/` ディレクトリを参照してください。
- **本フォークで変更・追加した部分**（新規 `render_thread.rs`、`lib.rs` /
  `winitwindowadapter.rs` / `Cargo.toml` の変更箇所、削除した `frame_throttle*` の扱い）は
  **GNU Affero General Public License v3.0 (AGPL-3.0)** です。

