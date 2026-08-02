//! ウィンドウの生成と表示制御（DESIGN v2 §4.5）。
//!
//! 編集ウィンドウは host 自身のものなので、探索も生存監視も要らない。ここに残る
//! Win32 は **前面化だけ** で、それも [`crate::focus`] の作法をそのまま使う。
//!
//! winit の `Window::focus_window` は使わない。あれは `SetForegroundWindow` を素で
//! 呼ぶだけで、実機では呼び出し制限に引っかかって無言で失敗する。前面スレッドへ
//! `AttachThreadInput` してから叩く作法が要る（→ [`crate::focus`]）。

use anyhow::{Context as _, bail};
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::platform::windows::WindowAttributesExtWindows as _;
use winit::window::Window;

use crate::focus;

const TITLE: &str = "anvi";

/// セル寸法が分かる前に使う暫定サイズ（論理ピクセル）。
///
/// 本当のセル寸法は `Renderer` が DirectWrite に訊くまで分からないが、ウィンドウは
/// レンダーターゲットより先に無いと作れない。既定フォント（MS Gothic 12pt = 16px、
/// 等幅なので半角の送り幅は 8px）から見積もった値を初期サイズに使い、`Renderer` が
/// できた直後に [`resize_to_grid`] で実測値へ合わせ直す。
const PROVISIONAL_CELL: (f64, f64) = (8.0, 16.0);

/// 編集ウィンドウを作る。**作った時点では見えないし前面にも来ない。**
///
/// ホットキーが押されるまで画面に出てはいけないので `with_visible(false)`、
/// 起動時にユーザーの作業を奪わないので `with_active(false)`、常駐アプリなので
/// タスクバーにも出さない。
///
/// タイトルバーは出さない（`with_decorations(false)`）。編集中の 1 行に
/// システムの枠を被せる意味が無く、ウィンドウは中央に出して `ZZ` / `ZQ` で閉じる
/// ものなので、ボタン類も要らない。
///
/// **`with_no_redirection_bitmap(true)` は透過の前提条件**（`WS_EX_NOREDIRECTIONBITMAP`）。
/// リダイレクションサーフェスがあると、そこが不透明に塗られて背後が透けない。
/// 実際の合成は [`crate::gui::render`] の DirectComposition が行う。
pub fn create(event_loop: &ActiveEventLoop, grid: (u16, u16)) -> anyhow::Result<Window> {
    let (cols, rows) = grid;
    let attributes = Window::default_attributes()
        .with_title(TITLE)
        .with_visible(false)
        .with_active(false)
        .with_resizable(true)
        .with_decorations(false)
        .with_transparent(true)
        .with_inner_size(LogicalSize::new(
            f64::from(cols) * PROVISIONAL_CELL.0,
            f64::from(rows) * PROVISIONAL_CELL.1,
        ))
        .with_skip_taskbar(true)
        .with_no_redirection_bitmap(true);
    event_loop
        .create_window(attributes)
        .context("failed to create the editor window")
}

/// `focus` と `Renderer` に渡す生の HWND。
///
/// HWND は `!Send` なので、スレッドを跨ぐ経路と同じく `isize` で持ち回す
/// （[`crate::focus::as_hwnd`] が Win32 の型へ戻す）。
pub fn hwnd_of(window: &Window) -> anyhow::Result<isize> {
    let handle = window
        .window_handle()
        .context("the editor window has no raw handle")?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Ok(win32.hwnd.get()),
        other => bail!("unexpected raw window handle: {other:?}"),
    }
}

/// グリッドがちょうど収まる内側サイズへ合わせる。
///
/// `cell` は物理ピクセルのセル寸法（幅・高さ）、`pad` はグリッドの周囲に空ける
/// 余白（物理ピクセル。上下左右それぞれ）。要求したサイズがそのまま通るとは
/// 限らない（DWM の最小サイズなど）ので、確定は後続の `Resized` イベントで行う。
pub fn resize_to_grid(window: &Window, cell: (f32, f32), grid: (u16, u16), pad: f32) {
    let margin = f64::from(pad) * 2.0;
    let width = to_px(f64::from(grid.0) * f64::from(cell.0) + margin);
    let height = to_px(f64::from(grid.1) * f64::from(cell.1) + margin);
    let size = PhysicalSize::new(width, height);
    if window.request_inner_size(size).is_none() {
        tracing::debug!(
            width,
            height,
            "the resize request is deferred to a Resized event"
        );
    }
}

/// 内側サイズの、余白を除いた部分に収まるグリッドの行列数。端数は切り捨て、最低 1x1。
///
/// 0 行 0 列を nvim へ送ると `nvim_ui_try_resize` がエラーを返すので、ここで潰す。
#[must_use]
pub fn grid_for(size: PhysicalSize<u32>, cell: (f32, f32), pad: f32) -> (u16, u16) {
    let margin = (pad * 2.0).ceil().max(0.0) as u32;
    (
        count(size.width.saturating_sub(margin), cell.0),
        count(size.height.saturating_sub(margin), cell.1),
    )
}

/// 画面に出して前面へ持ってくる（DESIGN 6.2 手順 5）。
///
/// 位置決めに失敗しても表示はする。変な位置に出るのは困るが、編集できないよりましで、
/// 原因はログに残る。
pub fn show(event_loop: &ActiveEventLoop, window: &Window, hwnd: isize) -> anyhow::Result<()> {
    match centered_position(event_loop, window) {
        Ok(position) => window.set_outer_position(position),
        Err(err) => tracing::error!(%err, "cannot center the editor window"),
    }
    window.set_visible(true);
    focus::set_foreground(hwnd)
}

/// 画面から消す（DESIGN 6.2 手順 8）。前面は呼び出し側が元のアプリへ戻す。
pub fn hide(window: &Window) {
    window.set_visible(false);
}

/// プライマリモニタの中央。ウィンドウがモニタより大きい場合は左上に寄せる。
fn centered_position(
    event_loop: &ActiveEventLoop,
    window: &Window,
) -> anyhow::Result<PhysicalPosition<i32>> {
    let monitor = match event_loop.primary_monitor() {
        Some(monitor) => monitor,
        None => {
            // Windows はディスプレイが 1 枚でもあればプライマリを返す。ここへ来るのは
            // マルチシートやリモートセッションの切り替わり中くらい。
            tracing::debug!("no primary monitor; using the first available one");
            event_loop
                .available_monitors()
                .next()
                .context("no monitor is available")?
        }
    };
    let origin = monitor.position();
    let area = monitor.size();
    let size = window.outer_size();
    Ok(PhysicalPosition::new(
        origin.x + offset(area.width, size.width),
        origin.y + offset(area.height, size.height),
    ))
}

/// モニタの辺 `area` の中に辺 `window` を置くときの左（上）余白。
fn offset(area: u32, window: u32) -> i32 {
    let margin = area.saturating_sub(window) / 2;
    i32::try_from(margin).unwrap_or(i32::MAX)
}

/// 画素数へ丸める。負にも巨大にもならないよう挟む。
fn to_px(value: f64) -> u32 {
    value.ceil().clamp(1.0, f64::from(u32::MAX)) as u32
}

/// 辺 `span`（画素）に幅 `unit` のセルが何個入るか。
fn count(span: u32, unit: f32) -> u16 {
    if unit <= 0.0 {
        // セル寸法が 0 以下になるのは `Renderer` の実装バグでしかない。
        tracing::error!(unit, "non-positive cell metric; clamping the grid to 1");
        return 1;
    }
    let cells = (f64::from(span) / f64::from(unit)).floor();
    cells.clamp(1.0, f64::from(u16::MAX)) as u16
}
