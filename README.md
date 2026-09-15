# dualslint — UIスレッドとRenderスレッドを分離したslintのフォーク

このリポジトリは [slint-ui/slint](https://github.com/slint-ui/slint) のフォークです。フォークの変更は `i-slint-backend-winit`
（`internal/backends/winit`）のみです。

提供する API は上流の Slint に準拠しています。フォークは上流の公開 API を削除・変更せず、描画スレッドに関するAPIを追加しています。

## このフォークの目的
GUI は人間とコンピュータの間の情報伝達のための手段です。その中でも、アニメーションを適切に使えば、人間から見える情報は増えます。
そのようなとき、コンピュータがアニメーションを描き続ける処理（出力）とマウス・キーボード操作の処理（入力）を並列に処理したいことがあるはずです。
ゲームやWebではUIと描画のスレッド分離は一般的かもしれませんが、ボタンやテキストボックスを使ったツール開発ではスレッド分離は珍しいでしょう。
したがって、**軽量かつ2スレッドなGUIを構築する**、というのが、このフォークの目的となります。

## アーキテクチャ（2スレッド分離）

Rust製で軽量なGUIライブラリであるslintですが、標準の slint は、描画（シーングラフ・GL コンテキスト・プレゼンタ）を必ず UI スレッド（ウィンドウを作ったスレッド）に縛ります。
本フォークは 描画を UI スレッドから完全に切り離し、独立した描画スレッドに移動させます。

| 担当 | UI スレッド | 描画スレッド（Render スレッド） |
|---|---|---|
| ウィンドウ / イベントループ | ✅ winit のイベントループとネイティブウィンドウ | — |
| 入力（マウス・キーボード）・OS イベント | ✅ | — |
| シーングラフ（item tree）・プロパティ | ✅ | — |
| テキストレイアウト（sharedparley） | ✅（UI スレッドで字句を生成） | — |
| GL コンテキスト（glutin） | — | ✅ |
| 描画（FemtoVG canvas） | — | ✅ |
| スワップチェーン / プレゼント | — | ✅（`swap_buffers`） |

UI スレッドはシーングラフを **シリアライズ**（スナップショット化）して描画スレッドへ送り、描画スレッドはそれを
**リプレイ** するだけです。GL コンテキストは描画スレッド上で作成されるため、UI スレッドは GL を一切触りません。
`winit::window::Window` は `Send + Sync` なので、UI スレッドで作ったネイティブウィンドウのハンドルを描画スレッドへ渡せます。

```
 UI スレッド                              描画スレッド
 ┌───────────────────────────┐          ┌───────────────────────────┐
 │ winit event loop          │          │ glutin GL context         │
 │ window (native)           │          │ FemtoVG canvas            │
 │ scene graph (items)       │          │ swap chain / present      │
 │ SnapshotEncoder ──┐       │          │                           │
 └───────────────────┼───────┘          └─────────────▲─────────────┘
					 │ mpsc channel                    │
			   RenderMessage::                        （プレゼント後）
			   {Configure, Resize,                    RequestRedraw を
				RenderScene(SceneFrame),               UI スレッドへ返す
				User, Suspend, Quit}
```

描画のペーシングは、描画スレッドがフレームをプレゼントした後に `CustomEvent::RequestRedraw` を UI スレッドへ
送り返すことで行われます（UI スレッドからネイティブ `request_redraw()` が呼ばれ、次のフレームの `RedrawRequested`
がスケジュールされます）。

## 上流slintからの変更点

| # | ファイル | 変更 |
|---|---------|------|
| 1 | `internal/backends/winit/render_thread.rs` | **書き換え（本体）** … 描画スレッドの GL ドライバ。mpsc プロトコル（`RenderMessage`）・`SceneFrame`/`DrawCommand` のレトゥインドモード・門番の `RenderHost`/`RenderCore` |
| 2 | `internal/backends/winit/snapshot.rs` | **新規追加** … `SnapshotEncoder`。UI スレッド上で item tree を `SceneFrame`（`DrawCommand` 列）へ直列化。テキストは sharedparley で字句（glyph run）に分解し、フォントの実バイト列を同梱 |
| 3 | `internal/backends/winit/renderer/dual.rs` | **新規追加** … `DualThreadRenderer` / `DualCoreRenderer`。`WinitCompatibleRenderer` の実装。`resume()` でネイティブウィンドウを生成して raw handle を `Configure` で描画スレッドへ送出、`render()` で `SnapshotEncoder` によるエンコード＋`submit_scene()` を行う |
| 4 | `internal/backends/winit/lib.rs` | `ensure_render_thread()` とモジュール配線。デフォルトレンダラーを `renderer::dual::DualThreadRenderer` にルーティング |
| 5 | `internal/backends/winit/winitwindowadapter.rs` | `request_redraw()` をスロットル/合流せずに winit へ直接転送 |
| 6 | `internal/backends/winit/event_loop.rs` | `CustomEvent::RequestRedraw`（描画スレッド → UI スレッドのフレームペーシング）の処理 |
| 7 | `internal/backends/winit/Cargo.toml` | `femtovg`/`glutin`/`glutin-winit`/`rgb`/`imgref` を常時依存化。`i-slint-core` に `box-shadow-cache` feature を追加。`linebender_resource_handle` を廃止 |
| 8 | `README.md` | 本ドキュメント |

削除した `frame_throttle.rs` / `apple_display_link.rs` は以前のフォーク版の名残です（描画のペーシングは現在
描画スレッドが担当するため、UI スレッド側のリフレッシュレートスロットルは不要です）。

## 描画スレッドのプロトコル

UI スレッド → 描画スレッドは **mpsc チャネル** 1本です。`RenderHost`（送信側）と `RenderCore`（受信側）を
`render_thread::channel()` が生成し、`ensure_render_thread()` がスレッドを起動します。

```rust
pub enum RenderMessage {
	Configure { window: Arc<winit::window::Window>, width: u32, height: u32, scale_factor: f64 },
	Resize { width: u32, height: u32 },
	RenderScene { frame: SceneFrame },
	SetOverlay { overlay: OverlayFrame },
	User(Box<dyn FnOnce() + Send>),
	Suspend,
	Quit,
}
```

- `Configure` … UI スレッドで作ったネイティブウィンドウのハンドル。描画スレッドが glutin GL コンテキストと
  **FemtoVG canvas を自スレッド上で生成** します。
- `RenderScene` … `SceneFrame`（下記）をリプレイして `swap_buffers()` でプレゼント。
- `SetOverlay` … UI シーンの上に合成するオーバーレイ層を差し替え。**任意のスレッドから送信可能**。
- `Suspend` … GL コンテキストとウィンドウの `Arc` を解放（UI スレッド側からネイティブウィンドウ破棄可能にする）。
- `User` … 任意クロージャーを描画スレッドで実行（診断・補助用）。
- `Quit` … スレッド終了。

### UI スレッド非依存の描画（オーバーレイ）

描画スレッドは最後に受け取った `SceneFrame` を **保持** します。`SetOverlay`（`RenderHost::submit_overlay()`）
が届くと、UI スレッドに依頼せず、保持した UI シーン＋オーバーレイを再合成して `swap_buffers()` でプレゼントします。
ウィンドウの `Resize` 時も同様に再合成するため、**UI スレッドがビジーでも描画を続行**できます。

```rust
// 任意のワーカースレッドから（RenderHost は Clone + Send + Sync）
if let Some(host) = render_thread::host() {
	host.submit_overlay(OverlayFrame { fonts: vec![], commands: vec![
		DrawCommand::FillRoundedRect { /* 物理ピクセル座標で記述 */ .. },
	] });
}
```

`OverlayFrame` の座標空間・列挙型は `SceneFrame` と同じです（物理ピクセル、同一 `DrawCommand`）。
空の `commands` を送るとオーバーレイ解除になります。

### `SceneFrame` — シーン全体のスナップショット

```rust
pub struct SceneFrame {
	pub width: u32, pub height: u32, pub scale_factor: f32,
	pub background: Option<[u8; 4]>,        // ウィンドウ背景（単色）→ クリアカラー
	pub commands: Vec<DrawCommand>,
	pub controls: Vec<ControlRegion>,       // ヒットテスト / スクリーンリーダー用（予約）
}
```

`DrawCommand` は状態操作（`Save`/`Restore`/`Translate`/`Rotate`/`Scale`/`SetGlobalAlpha`/`CombineClip`）と
プリミティブ（`FillRect`/`FillRoundedRect`/`StrokePath`/`FillPath`/`DrawGlyphRun`/`Blit`/`RenderLayer`/
`DrawBoxShadow` など）に分かれ、**すべての座標・長さは物理ピクセル** です。描画スレッドはスケーリングなしで
そのまま femtovg に流し込みます（`Scale` は UI スレッド側でエンコード時に畳み込む）。

- ウィンドウ背景が**単色**なら `SceneFrame.background` のクリアカラーとして送ります（オーバードローなし）。
- **グラデーションやパス**なら全面を覆う `draw_rectangle` コマンドとして直列化します。

### テキストの扱い

テキストは UI スレッド上で sharedparley によりグリフ配置まで済ませ、`DrawGlyphRun` としてフォントの実バイト列
（`Vec<u8>`）＋グリフ座標を送ります。描画スレッドは受信したバイト列を `get_or_create_font` でアトラスに登録し、
キャッシュ済みなら再利用してグリフを描画します。

## ワークフロー（1フレーム）

1. アプリが状態を更新 → `Window::request_redraw()`。
2. winit が `RedrawRequested` を発火 → `WinitWindowAdapter::draw()` が `DualThreadRenderer::render()` を呼ぶ。
3. `SnapshotEncoder` が item tree を巡回し、`SceneFrame` に直列化 → `RenderHost::submit_scene()`。
4. 描画スレッドが `SceneFrame` をリプレイ（クリア → `DrawCommand` 列 → `overlay` → `flush_to_output` → `swap_buffers`）。
5. 描画スレッドが `CustomEvent::RequestRedraw` を UI スレッドへ送り返し、次のフレームをスケジュール。

上記は UI スレッド発のペーシングです。これとは別に、**任意スレッドからの `submit_overlay()`** でも
描画スレッドは保持済み UI シーン＋オーバーレイをその場で合成・プレゼントします（ステップ 1〜3 を
経由しない、UI スレッド非依存の描画パス）。

## 追加された API

`i-slint-backend-winit` に `render_thread` モジュール（`pub mod render_thread;`）が公開されます。
シグネチャはフォーク開発の経緯で進化していますが、現時点（`main`）の主な公開項目は以下の通りです。

| 項目 | シグネチャ | 説明 |
|---|---|---|
| `channel()` | `pub(crate) fn channel(proxy) -> (RenderHost, RenderCore, FrameQueue)` | 描画スレッドとのチャネルを生成 |
| `ensure_render_thread()` | `pub(crate) fn ensure_render_thread(proxy)` | 初回呼び出しで描画スレッドを起動（冪等） |
| `host()` | `pub fn host() -> Option<RenderHost>` | グローバルな送信側ハンドルを取得 |
| `RenderHost` | `#[derive(Clone)]` `Send + Sync` | 送信側。`submit_scene()` / `submit_configure()` / `submit_resize()` / `submit_suspend()` / `send_user()` / `send_quit()` / `send_redraw()` を持ち、任意スレッドから描画スレッドへ要求を送れる |
| `set_image_sink()` | `pub fn set_image_sink<F>(sink: F)` | レトゥインドモードのフレームを `Image` として外部へ渡すコールバックを登録（CPU ラスタフォールバック用に残置） |
| `submit_overlay()` | `pub fn submit_overlay(&self, overlay: OverlayFrame)` | UI 非依存の描画パス。任意スレッドから UI シーン上に合成されるオーバーレイ層を差し替え、保持済み UI シーン＋オーバーレイを即座に再合成・プレゼント（`Resize` 時も自動再合成） |
| `OverlayFrame` | `pub struct { fonts: Vec<SceneFont>, commands: Vec<DrawCommand> }` | オーバーレイ層。座標空間・`DrawCommand` は `SceneFrame` と同一（物理ピクセル） |
| `hwnd()` | `pub fn hwnd() -> Option<isize>` | 起動中のネイティブウィンドウの HWND（Windows）を取得 |
| 型別名 | `PhysicalLength` / `PhysicalPoint` / `PhysicalRect` | `DrawCommand` の座標空間（物理ピクセル）を表す `euclid` エイリアス |

従来の CPU ラスタ API（`RenderThreadPlatform` 等）は設計の変遷で整理され、`render_thread::render_thread_legacy`
に互換スタブ（`PixelTarget` など）として残っています。

## 上流 API との関係

- **削除・変更された公開 API はありません。** 既存の `WinitWindowAdapter`・`Platform`・`Renderer`・
  `with_window_event_handler()` 等はシグネチャを保ったまま動作します。
- 動作差は以下のとおりです（いずれもシグネチャ不変）:
  - `WinitWindowAdapter::request_redraw()` … スロットル/合流をやめ、毎回 winit へ直接転送
  - デフォルトレンダラがフォーク版 `DualThreadRenderer` になる（`renderer-femtovg` 時）
- `renderer-software` / `renderer-skia` のフォールバックは、上流のままの通常スレッドモデルで動作します。
  2スレッド分離は `renderer-femtovg` の GL パスで有効です。

## アプリとの統合方法

`dualslint` は crates.io に公開しておらず、git リポジトリから直接依存して使います。`slint` を
依存に追加するときと同じ要領で、`slint` の依存指定をバージョンの代わりに git 参照へ差し替えるだけです。

```toml
[dependencies]
slint = { git = "https://github.com/akenejie/dualslint", branch = "master", features = ["renderer-femtovg"] }
```

`dualslint` は slint の公開 API（`.slint` のコンパイル、`slint` 相当の各モジュール、`Image` 等の型）を
そのまま提供し、内部の winit バックエンドだけを本フォーク版（`render_thread` モジュール入り）に
差し替えています。**`slint` を使う既存コードは、Cargo.toml の依存指定を git 参照へ差し替えるだけでそのまま動きます。**
アプリ側のコード変更は不要です。

```rust
slint::slint! { /* 既存の .slint コードはそのまま */ }

fn main() -> Result<(), slint::PlatformError> {
	let ui = MainWindow::new()?;
	ui.run()   // 内部で ensure_render_thread() が描画スレッドを起動する
}
```

> **注意** — git 依存にすると、フォークの workspace 全体（`i-slint-backend-winit` / `i-slint-core` /
> `i-slint-renderer-femtovg` など）が同じ git ツリーから一括で解決されます。crates.io 版と git 版の
> `i_slint_core` が別インスタンスになると型不一致のコンパイルエラーになるため、既存プロジェクトでは
> `slint` を含む全 slint 系依存を git 参照へ差し替えてください。フォークは上流 `master` を固定コミット
> （下表）で追従しています。挙動を固定したい場合は `branch = "master"` の代わりに `rev` にコミット
> ハッシュを指定してください。

---

## ベース情報

| 項目 | 値 |
|---|---|
| 上流リポジトリ | https://github.com/slint-ui/slint |
| ベース | `master`（commit `8dce1c4265d7ade881d8b2d5ec6c8bc3c228868c`、2026-09-12、version `1.18.0`） |
| 公開先 | https://github.com/akenejie/dualslint |
| フォークブランチ | `master` |
| 変更対象 | `internal/backends/winit`（`i-slint-backend-winit`） |

モノレポ内パスとクレート名の対応:

| モノレポ内パス | クレート名 |
|---|---|
| `internal/backends/winit` | `i-slint-backend-winit` |
| `internal/renderers/femtovg` | `i-slint-renderer-femtovg` |
| `internal/core` | `i-slint-core` |

---

## ライセンス

本リポジトリは上流 [slint-ui/slint](https://github.com/slint-ui/slint) をもとにしています。

- **上流のコード**（`render_thread.rs` / `snapshot.rs` / `renderer/dual.rs` 以外）は上流 slint のライセンス
  （`GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`）に従います。
  各ファイルの SPDX ヘッダーと各クレート内の `LICENSES/` ディレクトリを参照してください。
- **本フォークで変更・追加した部分**（新規 `render_thread.rs` / `snapshot.rs` / `renderer/dual.rs`、
  `lib.rs` / `winitwindowadapter.rs` / `event_loop.rs` / `Cargo.toml` の変更箇所）は
  **GNU Affero General Public License v3.0 (AGPL-3.0)** です。