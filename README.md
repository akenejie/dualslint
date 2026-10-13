# dualslint — UIスレッドとRenderスレッドを分離したslintのフォーク

このリポジトリは [slint-ui/slint](https://github.com/slint-ui/slint) のフォークです。フォークの変更は
主に winit バックエンド（`i-slint-backend-winit`）と、それを支える core / コンパイラ / 言語バインディングに
及びます。詳細は[「上流slintからの変更点」](#上流slintからの変更点)を参照してください。

提供する API は上流の Slint に準拠することを方針としていますが、描画権威の移動に伴い
`Window::request_redraw()` を削除しています（置き換えは `slint::render_thread::request_redraw()`）。
詳細は[「上流 API との関係」](#上流-api-との関係)を参照してください。

## このフォークの目的

GUI は人間とコンピュータの間の情報伝達のための手段です。その中でも、アニメーションを適切に使えば、人間から見える情報は増えます。
そのようなとき、コンピュータがアニメーションを描き続ける処理（出力）とマウス・キーボード操作の処理（入力）を並列に処理したいことがあるはずです。
ゲームやWebではUIと描画のスレッド分離は一般的かもしれませんが、ボタンやテキストボックスを使ったツール開発ではスレッド分離は珍しいでしょう。
したがって、**軽量かつ2スレッドなGUIを構築する**、というのが、このフォークの目的となります。

## 思想（render 主権）

フォークは次の 5 点を設計の土台としています。

1. **2 スレッド化。** 上流は 1 スレッドで OS 入力も描画も行いますが、本フォークは UI スレッドと
   render（描画）スレッドに分離します。
2. **UI は OS との通信（入力）に専念する。** ウィンドウの作成・イベントループ・マウス／キーボード入力の解決は
   UI スレッドの仕事です。
3. **render は画面への出力に専念する。** GL コンテキスト、描画、プレゼントは render スレッドのみが行います。
4. **render 主権。** 画面に描かれている item tree（プロパティを含む）は render が所有します。UI は tree や
   裏画面を render に送りません。render は「再描画して」と言われただけで再描画できるだけの情報
   （= その component をもう一度生成できる factory）を持ちます。UI スレッドも worker も render に対等な API で
   接します。
5. **UI は一切描画を行わない。**

「UI スレッドが描画した図をシリアライズして render へ送る」方式は、所有権を UI が握るために生まれた
中間表現であり、**思想違反として廃止済み**です。描画は上流のコード（`FemtoVGRenderer::render()`）を
render スレッド上でそのまま駆動します。

## アーキテクチャ（2スレッド分離）

上流 slint は、描画（シーングラフ・GL コンテキスト・プレゼンタ）を必ず UI スレッド（ウィンドウを作った
スレッド）に縛ります。本フォークは 描画を UI スレッドから完全に切り離し、独立した描画スレッドへ移します。
画面に表示される tree は render スレッドが自分のスレッド上で生成した **mirror** です。

| 担当 | UI スレッド | 描画スレッド（Render スレッド） |
|---|---|---|
| ウィンドウ / イベントループ | ✅ winit のイベントループとネイティブウィンドウ | — |
| 入力（マウス・キーボード）・OS イベント | ✅ 座標を論理ピクセルへ直し、発行済み geometry で hit-test | — |
| 画面に描かれる item tree・プロパティ | — | ✅ factory から作った mirror tree（bindings/timers 含む） |
| テキストレイアウト（sharedparley） | — | ✅ 上流の描画経路内で実施 |
| GL コンテキスト（glutin） | — | ✅ |
| 描画（上流 `FemtoVGRenderer` / `SoftwareRenderer`） | — | ✅ |
| スワップチェーン / プレゼント | — | ✅（`swap_buffers` / `present_with_damage`） |

```
UI スレッド                              Render スレッド
┌────────────────────────────┐          ┌────────────────────────────┐
│ winit event loop           │          │ own item tree (mirror)     │
│ window (native)            │          │ property graph / bindings  │
│ app 自身の component の handle │          │ glutin GL context          │
│ 入力イベントの解決          │          │ 上流 renderer::render()    │
│  └─ hit-test / 動詞化 ─────┼────┐     │ swap / present             │
└────────────────────────────┘    │     └─────────────▲──────────────┘
                                  │ mpsc             │
                     {Configure, Resize,  ───────────┘
                      AttachComponent,                  control geometry /
                     ApplyControlState,                 状態を共有表へ publish
                     ActivateControl,
                     KeyToRenderControl,
                     SetControlProperty,
                     GetControlProperty,
                     RunOnScreenTree, …}
```

- **画面の扱い方**は上流と同じ 1 本の tree に対するものだけです。UI スレッドから見ると、その tree は
  スレッドを隔てた別インスタンスなので、`render_thread` の API（下表）を介して操作します。
- アプリの handle が握る component（`AppWindow` 等）は UI スレッド側にも残りますが、**それは描画の
  対象ではありません**。表示されているのは render スレッドが factory から作った mirror です。
- 入力は生のまま render へ渡しません。UI スレッドが publish 済み geometry で hit-test し、コントロールを
  名指す「動詞」（hover / press / activate / scroll / pinch / rotate）だけを渡します。コントロールが
  何をするか（`clicked` / `toggled` / `accepted` の実行）は、tree を所有する render スレッドの責務です。

## ハンドオーバー（描画権の引き渡し）

生成コード（Rust / C++ ジェネレータと interpreter）は、window の root component を生成した時点で
**render factory**（その component を render スレッド上でもう 1 個生成できる `Fn() -> Box<dyn Any>`）を
`WindowInner::render_factory` に残します。アプリのコードは何も書く必要がありません。

1. UI スレッドが初回 `draw()` に達すると、`take_over_drawing_of()` が factory の有無・render スレッドの
   有無・描画可能な renderer の有無を確認し、`AttachComponent` を送ります。
2. render スレッドは factory を自分のスレッド上で実行し（headless window adapter + own platform）、
   mirror tree を生成します。ここで上流の描画経路（テキスト整形含む）が render スレッド上で動き始めます。
3. UI スレッドはウィンドウの graphics（GL context）を render へ渡し（`hand_over_graphics` →
   `GraphicsReleased`）、以降 render スレッドが全フレームを描いてプレゼントします。
4. 引き渡しができない window（factory がない / render スレッドが無い / renderer が描けない / 2 つ目以降の
   window）は、**blank のまま理由を一度だけ報告**して停止します。UI スレッドが静かに描くことはありません。

ハンドオーバー前後で、アプリが show 前に設定した public プロパティの値は mirror へ引き継がれます
（`run_render_handover`）。show 後の public プロパティの setter も、型がスレッドを越えられるものなら
描画中の tree（screen tree）へ転送されます。

## 描画スレッドのプロトコル

UI スレッド → 描画スレッドは **mpsc チャネル** です。送信側 `RenderHost`（`Clone` + `Send`）は任意スレッドから
使えます。`RenderMessage` の主な variant:

```rust
pub enum RenderMessage {
    Configure { window, width, height, scale_factor },
    Resize { window_id, width, height },
    User(Box<dyn FnOnce() + Send>),              // 任意クロージャを render スレッドで実行
    AttachComponent { factory, property_access }, // mirror tree を render スレッド上で生成
    ApplyControlState { id, hovered, pressed },
    SetControlProperty { id, property, value, response },
    GetControlProperty { id, property, response },
    ActivateControl { id, response },
    KeyToRenderControl { event, response },
    PressControl { id, button, position },
    MoveControl { position, finger_id },
    ReleaseControl { id, button, position },
    ScrollControl { id, position, delta_x, delta_y },
    PinchControl { id, position, delta },
    RotateControl { id, position, delta },
    RunOnScreenTree { task, done },
    RequestRedraw,
    SetAccent { color },
    HitTest { x, y, response },
    AwaitGraphics { window_id, response },
    GraphicsReleased { window_id },
    SetSurfaceOwner { window_id },
    Suspend { window_id },
    Quit,
}
```

- スレッドをまたぐ **フレーム（`SceneFrame` / `DrawCommand`）は送りません**。render スレッドは自分の
  mirror tree を保持しており、`RequestRedraw`（あるいは tree 自身の timer / animation / property 変更）が
  あるたびに自スレッド上で描画・プレゼントします。
- render スレッドは**自分専用の frame clock** を持ちます。UI スレッドからのメッセージ、mirror tree の
  `request_redraw()`、timer の期限、animation のフレーム間隔のいずれかで起床し、プレゼントします。
- 描画が終わるたびに UI スレッドへ再描画要求を返すのではなく、render スレッドが自分の clock で次の
  frame を決めて回します。

## コントロール借用 API（render への対等なアクセス）

rendered tree のコントロールは、共有座標表（`coordinate_map()` が返す `Arc<Mutex<PublishedControls>>`）に
矩形と状態として publish されます。UI スレッドも worker も同じ API で読めます。

| API | 意味 |
|---|---|
| `hit_test(x, y)` / `control_at(x, y)` | publish 済み geometry で最も特定度の高いコントロール id を得る |
| `apply_control_state(id, hovered, pressed)` | hover / press 状態を要求（bindings が上流のとおり再計算される） |
| `activate_control(id)` | クリック = press/release の組を tree に届ける。`clicked` の発火は tree の責務 |
| `press_control` / `move_control` / `release_control` / `scroll_control` / `pinch_control` / `rotate_control` | drag・wheel・ピンチ・回転の動詞 |
| `send_key_to_control(event)` | `InternalKeyEvent` をそのまま tree へ。解釈（カーソル移動・選択・IME・`accepted`）は item の仕事 |
| `get_control_property(id, name)` / `set_control_property(id, name, value)` | 名前ベースのプロパティ読み書き（`text` / `color` / `checked` 等） |
| `ComponentPropertyAccess::new(read, write)` | built-in item にない複合 component のプロパティは、app が貸す 2 つのクロージャで解決 |
| `run_on_screen_tree(local_tree, task)` | window-rooted な callback / function 呼び出しを描画中の tree で実行し、戻り値を回収 |

publish される対象は `TouchArea` / `FocusScope` / `TextInput` です。hover / press の状態と、`checked` のような
トグルの値を tree が持つため、UI スレッドは自分の記憶で推測しません。

## 上流slintからの変更点

| 領域 | 変更 |
|---|---|
| `internal/backends/winit/render_thread.rs` | **新規（本体）**。render スレッドの GL/software ドライバ。`RenderMessage` プロトコル・`RenderHost`（送信側）・mirror tree 生成（`AttachComponent`）・control 借用 API・`RunOnScreenTree` |
| `internal/backends/winit/winitwindowadapter.rs` | `draw()` は描画しない（handover を開始するだけ）。入力イベントを publish geometry で解決して動詞化。`request_redraw()` のフィルタ |
| `internal/backends/winit/lib.rs` | `pub mod render_thread`。デフォルトレンダラ選択を「render スレッドが描けるもの」（femtovg / software）に変更 |
| `internal/backends/winit/renderer/sw.rs` | ソフトウェア描画の UI スレッド駆動を撤去し、render スレッドの present 経路へ |
| `internal/backends/scene/` | **新規クレート**。control プロトコル型（`PublishedControls` / `ControlPropertyValue` / C ABI）と geometry のみのコントロール走査（`encode_window_controls`） |
| `internal/backends/android-activity/` | 同思想の render スレッド（`SkiaRenderer` を `ANativeWindow` に束縛、CPU ラスタ含む） |
| `internal/core/` | `WindowInner::render_factory`（生成コードが factory を残す場所）、`WindowAdapter::run_on_screen_tree()`、`render_handover`、`ComponentPropertyAccess` の土台 |
| `internal/compiler/` | Rust/C++/LLR: window-rooted な `invoke_*` の screen tree へのルーティング、複合 component の public プロパティが unused-remove されないよう保持、no_std でも通る render factory 登録 |
| `internal/interpreter/` | `CompilationUnit` を `Send + Sync` 化（`RwLock` / `AtomicUsize`）して render スレッドでも instance を生成可能に。window-rooted 呼び出しの screen tree ルーティング |
| `api/rs/slint` / `api/cpp` / `api/node` / `api/python` | `slint::render_thread`（`request_redraw`・control 借用・`attach_component*`）の公開とバインディング |

## 追加された API（`slint::render_thread`）

`winit` / `android-activity` バックエンドで `slint::render_thread` が公開されます。主な項目:

| 項目 | 説明 |
|---|---|
| `request_redraw()` | 任意スレッドから再描画を要求。`Window::request_redraw()` の置き換え |
| `RenderHost::attach_component(_to)` / `attach_component_with(_to)` | mirror tree を明示的に生成（自動ハンドオーバーを使わない場合） |
| `RenderHost::hit_test` / `control_at` / `apply_control_state` / `activate_control` / `press_control` / `move_control` / `release_control` / `scroll_control` / `pinch_control` / `rotate_control` / `send_key_to_control` | control 借用 API |
| `RenderHost::get_control_property` / `set_control_property` | 名前ベースのプロパティ読み書き |
| `RenderHost::has_attached_component` / `run_on_screen_tree` / `send_user` / `send_quit` | render スレッドの状態確認と同期呼び出し |
| `coordinate_map()` / `active_host()` / `hosts()` / `hwnd()` | 共有座標表とホスト列挙（`hwnd` は Windows） |

## 上流 API との関係

- **削除された公開 API:** `Window::request_redraw()`（Rust / C++ / Node）と FFI `slint_windowrc_request_redraw`。
  描画の権威は render スレッド（mirror tree を所有する側）にあります。UI スレッドへ再描画要求しても
  silent no-op になるため削除し、`slint::render_thread::request_redraw()` へ置き換えました。
- その他の公開 API（`WinitWindowAdapter`・`Platform`・`Renderer`・`with_window_event_handler()` 等）は
  シグネチャを保ったまま動作します。
- **描画できる renderer は femtovg（GL）と software です。** render スレッドが駆動できない
  `renderer-skia` / `renderer-vello` / `renderer-femtovg-wgpu` を選択したビルドでは、UI スレッドは
  描かないため window は blank になり、その旨を一度だけ報告します（Android は例外で、Skia を
  render スレッド経由で使用）。
- **複数 window は 1 つの render スレッドにつき 1 window** です。2 つ目以降の window は blank + 報告
  になります。drag の一部（`MoveControl` は実装済み）など、未対応の動詞は「残る gap」として
  レンダラの実装コメントに記録されています。

## アプリとの統合方法

`dualslint` は crates.io に公開しておらず、git リポジトリから直接依存して使います。`slint` を
依存に追加するときと同じ要領で、`slint` の依存指定をバージョンの代わりに git 参照へ差し替えるだけです。

```toml
[dependencies]
slint = { git = "https://github.com/akenejie/dualslint", branch = "master", features = ["renderer-femtovg"] }
```

`dualslint` は slint の公開 API（`.slint` のコンパイル、`slint` 相当の各モジュール、`Image` 等の型）を
そのまま提供します。**`slint` を使う既存コードは、Cargo.toml の依存指定を git 参照へ差し替えるだけで
そのまま動きます（`Window::request_redraw()` を使用している場合のみ置き換えが必要です）。**
アプリ側のコード追加は不要です（ハンドオーバーは自動で行われます）。

```rust
slint::slint! { /* 既存の .slint コードはそのまま */ }

fn main() -> Result<(), slint::PlatformError> {
	let ui = MainWindow::new()?;
	ui.run()   // 初回描画時に render スレッドが自動起動し、mirror を引き継ぐ
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
| 主な変更対象 | `internal/backends/winit`・`internal/backends/scene`・`internal/backends/android-activity`・`internal/core`・`internal/compiler`・`internal/interpreter`・`api/{rs,cpp,node,python}` |

### バージョン管理

このリリース（dualslint）のバージョンは **0.1.0** です。正式な所在は `Cargo.toml` の
`[workspace.package]` → `version`（`0.1.0`）で、リリース時にはここを更新します。

dualslint は上流 slint `v1.18.0`（commit `8dce1c4265d7ade881d8b2d5ec6c8bc3c228868c`）と
**API 互換**です。dualslint の修正はこの互換基盤の上に積まれ、基盤を変えずに複数の dualslint リリースが
生まれることがあります。そのため dualslint のバージョン（`0.1.0`、次は `0.2.0` …）は互換基盤の
バージョンとは別に管理されます。

---

## ライセンス

本リポジトリは上流 [slint-ui/slint](https://github.com/slint-ui/slint) をもとにしています。

- **上流からそのまま来ているファイル**は上流 slint のライセンス
  （`GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`）に従います。
  各ファイルの SPDX ヘッダーと各クレート内の `LICENSES/` ディレクトリを参照してください。
- **本フォークで新規追加したファイル**（`internal/backends/winit/render_thread.rs`、
  `internal/backends/scene/` 等）は **GNU Affero General Public License v3.0 (AGPL-3.0)** です。
  各ファイルの SPDX ヘッダー（`Copyright © akenejie`）を参照してください。