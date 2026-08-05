//! フォーカス復帰（DESIGN 7.3）。
//!
//! `SetForegroundWindow` には呼び出し制限があり、素で呼ぶと無言で失敗する。前面へ
//! 出す権利は「前面プロセスの入力キューに繋がっているスレッド」に与えられるため、
//! **対象ではなく現在の前面ウィンドウのスレッド**へ `AttachThreadInput` する。
//! 対象側にも `AllowSetForegroundWindow` で協調的に権利を渡す。
//!
//! 実機（Windows 11 / WSLg 越しの起動）で確認した事実:
//! 対象スレッドへアタッチする版は `SetForegroundWindow` が拒否される。前面スレッドへ
//! アタッチする版は通る。最後に `GetForegroundWindow` で結果を検証する。

use anyhow::bail;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, GA_ROOT, GetAncestor, GetForegroundWindow,
    GetWindowThreadProcessId, IsWindow, SetForegroundWindow,
};

/// `isize` として持ち回している HWND を Win32 の型へ戻す。
///
/// HWND は `!Send` なので、スレッドを跨ぐ経路（コントローラ / ウォッチャ）では
/// 生ポインタ値そのままを運ぶ。
pub fn as_hwnd(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

/// ホットキー押下時点の前面ウィンドウ（DESIGN 6.2 手順 2）。
pub fn foreground_window() -> isize {
    // SAFETY: 引数を取らず、失敗時は NULL を返すだけの純粋な問い合わせ。
    unsafe { GetForegroundWindow() }.0 as isize
}

/// 対象ウィンドウを前面へ出す。
///
/// UIA の `CurrentNativeWindowHandle` は子コントロール（メモ帳の編集領域など）を返す。
/// 前面化はトップレベルウィンドウの話なので、必ず `GA_ROOT` を解決してから扱う。
/// これを怠ると `SetForegroundWindow` が true を返しつつ前面は親になり、結果の検証が
/// 空振りする（実機で確認済み）。
///
/// 入力キューのアタッチは成功・失敗どちらの経路でも必ず解除する。付けっぱなしにすると
/// 対象アプリのキー入力が host のキューへ吸われ続ける。
pub fn set_foreground(raw: isize) -> anyhow::Result<()> {
    let hwnd = as_hwnd(raw);
    // SAFETY: 生の HWND 値の妥当性検査。無効値でも false を返すだけ。
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        bail!("target window {raw:#x} no longer exists");
    }
    let root = root_of(hwnd);
    if root_of(as_hwnd(foreground_window())) == root {
        return Ok(());
    }

    let mut target_pid = 0u32;
    // SAFETY: root は IsWindow 済みウィンドウの祖先。pid は有効なローカル変数。
    let target_tid = unsafe { GetWindowThreadProcessId(root, Some(&mut target_pid)) };
    if target_tid == 0 {
        bail!("GetWindowThreadProcessId failed for {raw:#x}");
    }

    // 対象プロセス自身が前面へ出る権利を与える協調的な作法。host に前面権が
    // 無い場合は失敗するが、それ自体は致命的ではないので記録して続行する。
    // SAFETY: pid は直前に取得した実在プロセスの ID。
    if let Err(err) = unsafe { AllowSetForegroundWindow(target_pid) } {
        tracing::debug!(%err, target_pid, "AllowSetForegroundWindow denied");
    }

    let attachment = Attachment::to_foreground_thread();

    // SAFETY: root は実在するトップレベルウィンドウ。可否は下で実測する。
    let brought = unsafe { SetForegroundWindow(root) }.as_bool();
    // SAFETY: 同上。Z オーダーだけを動かす。
    if let Err(err) = unsafe { BringWindowToTop(root) } {
        tracing::debug!(%err, "BringWindowToTop failed");
    }

    drop(attachment);

    // 戻り値が true でも実際に前面になっていない場合があるため実測で判定する。
    if root_of(as_hwnd(foreground_window())) == root {
        return Ok(());
    }
    bail!("SetForegroundWindow was refused for {raw:#x} (returned {brought})");
}

/// トップレベルの祖先。子コントロールの HWND を渡されても前面化や位置決めが
/// できるようにする。
pub fn root_of(hwnd: HWND) -> HWND {
    // SAFETY: 問い合わせのみ。無効な HWND でも NULL を返すだけ。
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.is_invalid() { hwnd } else { root }
}

/// 現在の前面ウィンドウのスレッドへ入力キューを繋いでいる間だけ生きるガード。
struct Attachment {
    this_tid: u32,
    fg_tid: u32,
}

impl Attachment {
    fn to_foreground_thread() -> Option<Self> {
        let fg = as_hwnd(foreground_window());
        // SAFETY: fg は GetForegroundWindow の戻り値。NULL でも 0 を返すだけ。
        let fg_tid = unsafe { GetWindowThreadProcessId(fg, None) };
        // SAFETY: 引数なしの問い合わせ。
        let this_tid = unsafe { GetCurrentThreadId() };
        if fg_tid == 0 || fg_tid == this_tid {
            return None;
        }
        // SAFETY: 双方とも実在するスレッド ID。失敗しても副作用は無い。
        if !unsafe { AttachThreadInput(this_tid, fg_tid, true) }.as_bool() {
            tracing::debug!(
                this_tid,
                fg_tid,
                "AttachThreadInput to the foreground failed"
            );
            return None;
        }
        Some(Self { this_tid, fg_tid })
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // SAFETY: 直前に成功したアタッチの対称な解除。解除漏れは対象アプリの入力を
        // host のキューへ吸い続けるため、失敗しても記録だけして進む。
        if let Err(err) = unsafe { AttachThreadInput(self.this_tid, self.fg_tid, false) }.ok() {
            tracing::warn!(%err, "failed to detach the input queue");
        }
    }
}
