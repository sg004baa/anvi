//! Foreground requests without sharing another application's input queue.
//!
//! AttachThreadInput makes activation synchronous with the other thread and can
//! freeze both applications when that thread is busy. Never use it to bypass
//! foreground restrictions. A cross-thread foreground request may complete
//! asynchronously; confirmation belongs on the controller, not the GUI thread.

use std::time::{Duration, Instant};

use anyhow::bail;
use anvi_core::SpawnPolicy;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, IsWindow, SetForegroundWindow,
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

/// Request activation once, without attaching input queues or synchronously
/// bringing a foreign window to the top. Acceptance is not completion.
pub fn set_foreground(raw: isize) -> anyhow::Result<()> {
    let hwnd = as_hwnd(raw);
    // SAFETY: IsWindow accepts arbitrary handles and returns false for stale ones.
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        bail!("target window {raw:#x} no longer exists");
    }
    let root = root_of(hwnd);
    if root_of(as_hwnd(foreground_window())) == root {
        return Ok(());
    }
    // SAFETY: root belongs to a valid window. Do not attach its input queue:
    // cross-thread activation must remain asynchronous.
    if unsafe { SetForegroundWindow(root) }.as_bool()
        || root_of(as_hwnd(foreground_window())) == root
    {
        return Ok(());
    }
    bail!("SetForegroundWindow was refused for {raw:#x}");
}

/// Run only on the controller thread after the GUI has posted the request.
/// Observes completion without repeating activation or blocking message pumping.
pub fn wait_for_foreground(raw: isize, policy: &SpawnPolicy) -> anyhow::Result<()> {
    let root = root_of(as_hwnd(raw));
    wait_for_activation(
        || root_of(as_hwnd(foreground_window())) == root,
        || policy.is_exiting(),
        Duration::from_millis(750),
    )
}

fn wait_for_activation(
    mut activated: impl FnMut() -> bool,
    mut exiting: impl FnMut() -> bool,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if exiting() {
            bail!("application is exiting");
        }
        if activated() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for foreground activation");
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

/// トップレベルの祖先。子コントロールの HWND を渡されても前面化や位置決めが
/// できるようにする。
pub fn root_of(hwnd: HWND) -> HWND {
    // SAFETY: 問い合わせのみ。無効な HWND でも NULL を返すだけ。
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.is_invalid() { hwnd } else { root }
}

#[cfg(test)]
mod tests {
    use super::wait_for_activation;
    use std::time::Duration;

    #[test]
    fn asynchronous_activation_is_observed_without_reissuing_the_request() {
        let mut polls = 0;
        wait_for_activation(
            || {
                polls += 1;
                polls == 3
            },
            || false,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(polls, 3);
    }

    #[test]
    fn refused_activation_is_bounded_and_exit_interrupts_the_wait() {
        assert!(wait_for_activation(|| false, || false, Duration::ZERO).is_err());
        assert!(wait_for_activation(|| true, || false, Duration::ZERO).is_ok());
        assert!(
            wait_for_activation(
                || panic!("exit must be checked first"),
                || true,
                Duration::from_secs(30),
            )
            .is_err()
        );
    }
}
