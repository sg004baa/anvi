//! Neovide ウィンドウの制御（DESIGN 7）。
//!
//! Neovide には「隠れろ」というコマンドが無いので、HWND を掴んで Win32 で直に殴る。
//! nvim を起動するのは host の責務であり Neovide ではない（DESIGN 3.2）ため、ここは
//! `--server` でアタッチさせるだけ。

use std::cell::Cell;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, DispatchMessageW, EVENT_OBJECT_SHOW, EnumWindows, GA_ROOT, GWL_EXSTYLE,
    GetAncestor, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    HWND_TOP, IsWindow, IsWindowVisible, MSG, MsgWaitForMultipleObjects, OBJID_WINDOW, PM_REMOVE,
    PeekMessageW, PostQuitMessage, QS_ALLINPUT, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, ShowWindow, TranslateMessage,
    WINEVENT_OUTOFCONTEXT, WM_QUIT, WS_EX_TOOLWINDOW,
};

use crate::controller::Cmd;
use crate::focus;

/// 画面外の待機座標（DESIGN 7.2）。
const PARKING: (i32, i32) = (-32_000, -32_000);
/// ウィンドウが現れるまでの上限。セキュリティソフトのスキャンが乗ると数秒かかる
/// ことがあるため長めに取る（DESIGN 5.3 / 10.6）。
const WINDOW_TIMEOUT: Duration = Duration::from_secs(30);
/// フックの取りこぼしに備えた再走査の間隔。フックが飛べばこれより早く起きる。
const WINDOW_POLL: Duration = Duration::from_millis(100);
/// Neovide プロセスの生存確認の間隔（DESIGN 6.3 の watch）。
const CHILD_POLL: Duration = Duration::from_millis(250);

/// 常駐する Neovide ウィンドウ。
pub struct Editor {
    child: Arc<Mutex<Child>>,
    hwnd: isize,
    /// `kill()` 済みの印。意図的に殺したペアの死をリカバリ経路へ流さない
    /// （DESIGN 6.3 誤発火の抑止）。
    abandoned: Arc<AtomicBool>,
}

impl Editor {
    /// Neovide を spawn し、ウィンドウを特定して画面外へ隠す（DESIGN 7.1 / 7.2）。
    ///
    /// Neovide の死（プロセス終了／ウィンドウ消滅）は専用スレッドで watch し、
    /// `Cmd::EditorLost` を送る。
    pub fn spawn(
        neovide_exe: &Path,
        port: u16,
        tx: std::sync::mpsc::Sender<Cmd>,
    ) -> anyhow::Result<Self> {
        let child = Command::new(neovide_exe)
            .arg(format!("--server=127.0.0.1:{port}"))
            // Neovide も env_logger を見るため、host のログ設定を継がせると
            // Neovide 自身の redraw ログでこちらのログが埋まる。
            .env_remove("RUST_LOG")
            .spawn()
            .with_context(|| format!("failed to spawn {}", neovide_exe.display()))?;
        let pid = child.id();
        tracing::info!(pid, port, "neovide spawned");

        let child = Arc::new(Mutex::new(child));
        match Self::attach(pid, &child, tx) {
            Ok((hwnd, abandoned)) => Ok(Self {
                child,
                hwnd,
                abandoned,
            }),
            Err(err) => {
                // 画面が出ないなら使い物にならない。孤児を残さず落とす。
                if let Err(cleanup) = kill_child(&child) {
                    tracing::warn!(%cleanup, pid, "could not clean up the neovide child");
                }
                Err(err)
            }
        }
    }

    /// spawn 済みの子から、ウィンドウの特定・待避・watch までを済ませる。
    fn attach(
        pid: u32,
        child: &Arc<Mutex<Child>>,
        tx: std::sync::mpsc::Sender<Cmd>,
    ) -> anyhow::Result<(isize, Arc<AtomicBool>)> {
        let hwnd = find_window(pid, child)?;
        tracing::info!(
            pid,
            hwnd = format_args!("{hwnd:#x}"),
            "neovide window found"
        );
        park(hwnd)?;
        let abandoned = Arc::new(AtomicBool::new(false));
        watch(hwnd, Arc::clone(child), Arc::clone(&abandoned), tx)?;
        Ok((hwnd, abandoned))
    }

    /// 表示してフォーカスを移す（DESIGN 7.2 表示時）。
    ///
    /// 表示できないのは致命的だが、フォーカスが移らないのはそうではない。窓が出て
    /// いればユーザーがクリックすれば編集できるので、フォーカスの失敗は記録して
    /// `Ok` を返す（DESIGN 6.1 の Editing は「Neovide 表示中」である）。
    pub fn show_and_focus(&self) -> anyhow::Result<()> {
        let hwnd = focus::as_hwnd(self.hwnd);
        let (x, y) = centered_position(hwnd)?;
        // SAFETY: hwnd は spawn 時に特定したトップレベルウィンドウ。
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            )
        }
        .context("failed to move the neovide window on-screen")?;
        // SAFETY: 同上。戻り値は「元が可視だったか」であってエラーではない。
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
        if let Err(err) = focus::set_foreground(self.hwnd) {
            tracing::error!(%err, "neovide is visible but did not take focus");
        }
        Ok(())
    }

    /// 既存セッションへ戻すだけ（DESIGN 6.1 Editing 中のホットキー）。
    pub fn focus(&self) -> anyhow::Result<()> {
        focus::set_foreground(self.hwnd)
    }

    /// ウィンドウがまだ生きているか（× で閉じられていないか）。
    pub fn window_is_alive(&self) -> bool {
        window_is_alive(self.hwnd)
    }

    /// 隠す（DESIGN 7.2 終了時）。座標は動かさない。
    pub fn hide(&self) -> anyhow::Result<()> {
        // SAFETY: hwnd は spawn 時に特定したトップレベルウィンドウ。
        let _ = unsafe { ShowWindow(focus::as_hwnd(self.hwnd), SW_HIDE) };
        Ok(())
    }

    /// 意図的に落とす。ペア再起動と host 終了の両方で使う。
    pub fn kill(&self) -> anyhow::Result<()> {
        // watch スレッドより先に印を立てる。順序が逆だとリカバリが誤発火する。
        self.abandoned.store(true, Ordering::SeqCst);
        kill_child(&self.child)
    }
}

fn kill_child(child: &Arc<Mutex<Child>>) -> anyhow::Result<()> {
    let mut child = lock(child);
    child.kill().context("failed to kill neovide")?;
    child.wait().context("failed to reap neovide")?;
    Ok(())
}

/// 中身は毒されない（`Child` の操作は panic しない）ので、毒された鍵は素通しでよい。
fn lock(child: &Arc<Mutex<Child>>) -> std::sync::MutexGuard<'_, Child> {
    child.lock().unwrap_or_else(|err| err.into_inner())
}

/// 起動直後の待避（DESIGN 7.2 起動時）。画面外へ飛ばしてから隠すので実質見えない。
fn park(hwnd: isize) -> anyhow::Result<()> {
    let hwnd = focus::as_hwnd(hwnd);
    // SAFETY: hwnd は直前に特定したトップレベルウィンドウ。
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            PARKING.0,
            PARKING.1,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }
    .context("failed to park the neovide window off-screen")?;
    // SAFETY: 同上。
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    Ok(())
}

/// プライマリモニタの中央に置くための左上座標。サイズは変えない。
fn centered_position(hwnd: HWND) -> anyhow::Result<(i32, i32)> {
    let mut rect = RECT::default();
    // SAFETY: hwnd は有効なウィンドウ、rect は有効なローカル変数。
    unsafe { GetWindowRect(hwnd, &mut rect) }.context("GetWindowRect failed")?;
    // SAFETY: 引数は定数の問い合わせ ID。
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    Ok((((sw - w) / 2).max(0), ((sh - h) / 2).max(0)))
}

thread_local! {
    /// `EVENT_OBJECT_SHOW` が飛んできた印。out-of-context フックのコールバックは
    /// フックを張ったスレッド上で走るので thread_local で受けられる。
    static SHOWN: Cell<bool> = const { Cell::new(false) };
}

// SAFETY: `SetWinEventHook` の WINEVENTPROC として呼ばれる規約通りの署名。
unsafe extern "system" fn on_object_show(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if event == EVENT_OBJECT_SHOW && id_object == OBJID_WINDOW.0 && id_child == CHILDID_SELF as i32
    {
        SHOWN.set(true);
    }
}

/// PID のトップレベルウィンドウを待つ（DESIGN 7.1）。
///
/// フックは「ウィンドウが出た」という早期通知として使い、実際の特定は `EnumWindows` +
/// `GetWindowThreadProcessId` で行う。フックを張る前に窓が出てしまった場合でも
/// `WINDOW_POLL` 毎の再走査で拾えるようにしてある。
fn find_window(pid: u32, child: &Arc<Mutex<Child>>) -> anyhow::Result<isize> {
    // SAFETY: コールバックは 'static な関数ポインタ、idProcess で対象プロセスに絞る。
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_SHOW,
            None,
            Some(on_object_show),
            pid,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.0.is_null() {
        bail!("SetWinEventHook failed for pid {pid}");
    }
    SHOWN.set(false);

    let result = wait_for_window(pid, child);

    // SAFETY: 直前に張った有効なフックの解除。
    unsafe { UnhookWinEvent(hook) }
        .ok()
        .context("UnhookWinEvent failed")?;
    result
}

fn wait_for_window(pid: u32, child: &Arc<Mutex<Child>>) -> anyhow::Result<isize> {
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    loop {
        if let Some(hwnd) = top_level_window_of(pid)? {
            return Ok(hwnd);
        }
        if let Some(status) = lock(child)
            .try_wait()
            .context("try_wait on neovide failed")?
        {
            bail!("neovide exited before showing a window: {status}");
        }
        if Instant::now() >= deadline {
            bail!("timed out after {WINDOW_TIMEOUT:?} waiting for the neovide window (pid {pid})");
        }
        // フックの通知（またはタイムアウト）で起きる。ポーリング間隔は保険。
        // SAFETY: ハンドル配列なしのメッセージ待ち。
        unsafe {
            MsgWaitForMultipleObjects(None, false, WINDOW_POLL.as_millis() as u32, QS_ALLINPUT)
        };
        pump();
        if SHOWN.take() {
            tracing::debug!(pid, "EVENT_OBJECT_SHOW hint received");
        }
    }
}

/// このスレッドのキューを空にする。フックのコールバックはこの間に走る。
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: msg は有効なローカル変数。PM_REMOVE で取り出して捨てるだけ。
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        if msg.message == WM_QUIT {
            // このスレッドの本来のメッセージループへ返す（起動時は誰も投げないが、
            // ペア再起動時にコントローラスレッドで回る可能性を潰しておく）。
            // SAFETY: 引数は終了コードのみ。
            unsafe { PostQuitMessage(msg.wParam.0 as i32) };
            return;
        }
        // SAFETY: msg は直前に取得した有効なメッセージ。
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

struct Search {
    pid: u32,
    found: isize,
}

// SAFETY: `EnumWindows` の WNDENUMPROC として呼ばれる規約通りの署名。
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: lparam には直下の EnumWindows 呼び出しが渡した &mut Search しか入らない。
    let search = unsafe { &mut *(lparam.0 as *mut Search) };
    let mut pid = 0u32;
    // SAFETY: hwnd は列挙で渡された有効なウィンドウ。pid は有効な参照。
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if search.found == 0 && pid == search.pid && is_app_window(hwnd) {
        search.found = hwnd.0 as isize;
    }
    // 早期打ち切りで FALSE を返すと EnumWindows 自体が失敗扱いになるため、
    // 最後まで回して最初の一致を採る。
    windows::core::BOOL(1)
}

/// 画面に出る本物のウィンドウか。
///
/// winit は同一プロセスに `Winit Thread Event Target`（14x14 のツールウィンドウ）と
/// IME 用の隠しウィンドウも作る。**実機ではこのツールウィンドウが先に列挙され、そちらを
/// 掴むと「本物の窓が出たまま隠れない・フォーカスも移らない」という壊れ方をした。**
/// ツールウィンドウは定義上アプリのウィンドウではないので除外する。
fn is_app_window(hwnd: HWND) -> bool {
    // SAFETY: hwnd は列挙で渡された有効なウィンドウ。以下はすべて問い合わせのみ。
    unsafe {
        if GetAncestor(hwnd, GA_ROOT).0 != hwnd.0 || !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        ex_style & WS_EX_TOOLWINDOW.0 == 0
    }
}

fn top_level_window_of(pid: u32) -> anyhow::Result<Option<isize>> {
    let mut search = Search { pid, found: 0 };
    // SAFETY: コールバックは 'static、lparam はこの呼び出しの間だけ生きるローカル。
    unsafe { EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut Search as isize)) }
        .context("EnumWindows failed")?;
    Ok((search.found != 0).then_some(search.found))
}

/// Neovide の死を watch する（DESIGN 6.3 の三本立てのうちの一本）。
///
/// 監視対象はプロセスの終了だけではない。**Neovide のウィンドウを × で閉じると、実機では
/// プロセスが生き残って HWND だけが消える**（Windows 11 で確認）。この場合 RPC も切れない
/// ため、ウィンドウの消滅も同じリカバリ経路へ流す。
fn watch(
    hwnd: isize,
    child: Arc<Mutex<Child>>,
    abandoned: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<Cmd>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("neovide-watch".into())
        .spawn(move || {
            loop {
                if abandoned.load(Ordering::SeqCst) {
                    return;
                }
                match lock(&child).try_wait() {
                    Ok(Some(status)) => {
                        if abandoned.load(Ordering::SeqCst) {
                            return;
                        }
                        tracing::warn!(%status, "neovide exited on its own");
                        let _ = tx.send(Cmd::EditorLost("neovide exited"));
                        return;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::error!(%err, "cannot watch the neovide process");
                        return;
                    }
                }
                if !window_is_alive(hwnd) {
                    if abandoned.load(Ordering::SeqCst) {
                        return;
                    }
                    tracing::warn!(
                        hwnd = format_args!("{hwnd:#x}"),
                        "the neovide window was destroyed while the process lives"
                    );
                    let _ = tx.send(Cmd::EditorLost("neovide window destroyed"));
                    return;
                }
                std::thread::sleep(CHILD_POLL);
            }
        })
        .context("failed to start the neovide watch thread")?;
    Ok(())
}

/// HWND がまだ生きているか。× で閉じられた Neovide の検知に使う。
fn window_is_alive(hwnd: isize) -> bool {
    // SAFETY: 生の HWND 値の問い合わせ。無効値でも false を返すだけ。
    unsafe { IsWindow(Some(focus::as_hwnd(hwnd))) }.as_bool()
}
