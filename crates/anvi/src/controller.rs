//! セッション状態機械を持つスレッド（DESIGN 6、11.2、付録 B）。
//!
//! `Session` と nvim の RPC はこのスレッドだけが触る。GUI（winit のループが回る
//! main スレッド）とは `Cmd` で、UIA の MTA スレッドとは `Uia` で、それぞれ
//! チャンネル越しにしか繋がらない。逆向き（コントローラ → GUI）は
//! [`EventLoopProxy`] に載せた [`UserEvent`]。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use anvi_core::{Applied, HostEvent, NvimConfig, NvimServer, Phase, Session};
use anyhow::{Context as _, anyhow};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;
use winit::event_loop::EventLoopProxy;

use crate::bundle::{APPNAME, Bundle};
use crate::focus;
use crate::gui::{DEFAULT_GRID, UserEvent};
use crate::uia::Uia;

/// コントローラへの指令。GUI スレッドと RPC 転送タスクから届く。
#[derive(Debug)]
pub enum Cmd {
    Hotkey,
    Exit,
    Host(HostEvent),
    /// GUI からのキー入力（既に nvim 記法）。
    Input(String),
    /// ウィンドウのサイズが変わった。
    Resize {
        cols: u16,
        rows: u16,
    },
    /// ウィンドウの × が押された。`AnviQuit` と同じ扱い。
    CloseRequested,
}

/// nvim との接続一式。RPC は 1 本で、host イベントと UI の `redraw` が相乗りする。
pub struct Pair {
    pub nvim: NvimServer,
    pub host_rx: UnboundedReceiver<HostEvent>,
    pub redraw_rx: UnboundedReceiver<Vec<rmpv::Value>>,
}

/// コントローラスレッドに渡す一切。
pub struct Boot {
    pub bundle: Bundle,
    pub rt: Handle,
    pub tx: Sender<Cmd>,
    pub rx: Receiver<Cmd>,
    pub uia: Uia,
    pub pair: Pair,
    /// GUI へ「出せ」「隠せ」「前面へ」を伝える経路。
    pub proxy: EventLoopProxy<UserEvent>,
    /// 意図的シャットダウン中の印。リカバリの誤発火を抑止する（DESIGN 6.3）。
    pub shutting_down: Arc<AtomicBool>,
}

/// nvim を起こして UI としても繋ぐ。
///
/// `ui_attach` までここでやる。attach していない nvim は 1 バイトも描かないので、
/// 「起動はしたが画面が真っ白」を作らないためにペアの生成と不可分にしておく。
pub fn spawn_pair(bundle: &Bundle, rt: &Handle) -> anyhow::Result<Pair> {
    let cfg = NvimConfig {
        nvim_exe: bundle.nvim_exe.clone(),
        runtime_dir: bundle.runtime_dir.clone(),
        appname: APPNAME.to_owned(),
        clipboard: Arc::new(crate::clipboard::WinClipboard),
    };
    let (nvim, handles) = rt
        .block_on(NvimServer::spawn(&cfg))
        .context("failed to start the nvim server")?;
    let (cols, rows) = DEFAULT_GRID;
    rt.block_on(nvim.attach_ui(cols, rows))
        .context("failed to attach the ui")?;
    // ポートは実機での調査に要る（`nvim --server 127.0.0.1:PORT --remote-send` で
    // 中の nvim を直接叩ける）。info で出しておく。
    tracing::info!(port = nvim.port(), cols, rows, "nvim is up and attached");
    Ok(Pair {
        nvim,
        host_rx: handles.host,
        redraw_rx: handles.redraw,
    })
}

/// コントローラスレッドを起こす。
pub fn start(boot: Boot) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let Boot {
        bundle,
        rt,
        tx,
        rx,
        uia,
        pair,
        proxy,
        shutting_down,
    } = boot;
    let Pair {
        nvim,
        host_rx,
        redraw_rx,
    } = pair;

    let generation = Arc::new(AtomicU64::new(0));
    forward(&rt, host_rx, tx.clone(), 0, Arc::clone(&generation));
    pump_redraw(&rt, redraw_rx, proxy.clone(), 0, Arc::clone(&generation));

    let mut controller = Controller {
        bundle,
        rt,
        tx,
        uia,
        proxy,
        session: Session::default(),
        nvim,
        target_hwnd: 0,
        shutting_down,
        generation,
    };
    std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || controller.run(rx))
        .context("failed to start the controller thread")
}

struct Controller {
    bundle: Bundle,
    rt: Handle,
    tx: Sender<Cmd>,
    uia: Uia,
    proxy: EventLoopProxy<UserEvent>,
    session: Session,
    nvim: NvimServer,
    /// 書き戻し先ウィンドウ（DESIGN 6.2 手順 2 / 11）。
    target_hwnd: isize,
    shutting_down: Arc<AtomicBool>,
    /// 現行ペアの世代。旧ペアのイベントを捨てるために使う。
    generation: Arc<AtomicU64>,
}

impl Controller {
    fn run(&mut self, rx: Receiver<Cmd>) {
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Hotkey => self.on_hotkey(),
                Cmd::Host(event) => self.on_host(event),
                Cmd::Input(keys) => self.on_input(&keys),
                Cmd::Resize { cols, rows } => self.on_resize(cols, rows),
                Cmd::CloseRequested => self.on_close_requested(),
                Cmd::Exit => {
                    self.teardown();
                    return;
                }
            }
        }
        // `self.tx` を握っているので到達しないが、`recv()` の Result を潰さない。
        tracing::error!("command channel closed; tearing down the pair");
        self.shutting_down.store(true, Ordering::SeqCst);
        self.teardown();
    }

    /// DESIGN 付録 B ホットキーハンドラ。
    fn on_hotkey(&mut self) {
        match self.session.phase() {
            // 既存セッションへ戻すだけ（DESIGN 6.1）。
            Phase::Editing => self.notify_gui(UserEvent::Focus),
            Phase::Idle => {
                if let Err(err) = self.begin_session() {
                    tracing::error!(%err, "cannot start a session");
                }
            }
            // Capturing / Applying は一瞬で通過する遷移状態。取りこぼして構わない。
            phase => tracing::debug!(?phase, "hotkey ignored"),
        }
    }

    fn begin_session(&mut self) -> anyhow::Result<()> {
        let foreground = focus::foreground_window();
        if !self.session.begin_capture() {
            tracing::debug!("capture already in flight");
            return Ok(());
        }

        let captured = match self.uia.capture() {
            Ok(Some(captured)) => captured,
            // 編集対象が無ければ何もせず Idle へ戻る。通知も不要（DESIGN 8.3）。
            Ok(None) => {
                tracing::debug!(
                    foreground = format_args!("{foreground:#x}"),
                    "no editable target"
                );
                self.session.abort_capture();
                return Ok(());
            }
            Err(err) => {
                self.session.abort_capture();
                return Err(err.context("capture failed"));
            }
        };
        tracing::debug!(
            foreground = format_args!("{foreground:#x}"),
            target = format_args!("{:#x}", captured.hwnd),
            lines = captured.lines.len(),
            "captured"
        );

        // filetype は指定しない。見た目とオプションはローカル設定の領分（DESIGN 5.4）。
        if let Err(err) = self
            .rt
            .block_on(self.nvim.start_session(&captured.lines, None))
        {
            self.session.abort_capture();
            return Err(err.context("start_session failed"));
        }

        self.target_hwnd = captured.hwnd;
        self.session.begin_edit(captured.lines);

        if let Err(err) = self.proxy.send_event(UserEvent::Show) {
            // 画面が出ないなら編集できない。掴んだセッションを畳んで Idle へ戻す。
            self.session.reset();
            return Err(anyhow!("the gui event loop is gone: {err}"));
        }
        Ok(())
    }

    /// DESIGN 付録 B 通知ハンドラ。
    fn on_host(&mut self, event: HostEvent) {
        match event {
            // 保持するだけ。書き戻しはセッション終了時（DESIGN 4.4）。
            HostEvent::SessionWrite(lines) => self.session.on_write(lines),
            HostEvent::SessionEnd => self.finish_session(),
            // 「設定が効かない」はここを見れば終わる（DESIGN 5.4）。
            HostEvent::ConfigResolved { dir, loaded } => {
                tracing::info!(dir, loaded, "local config");
            }
            // ローカル設定の読み込み失敗。起動は続行済み（DESIGN 5.4）。
            HostEvent::InitError { kind, message } => {
                tracing::warn!(kind, message, "user config error");
            }
            // 早期ヒント。正はこの下の Disconnected（DESIGN 6.3）。
            HostEvent::NvimDying => self.recover("nvim reported VimLeavePre"),
            HostEvent::Disconnected => self.recover("nvim rpc disconnected"),
        }
    }

    /// GUI から届いたキー入力をそのまま nvim へ。
    fn on_input(&mut self, keys: &str) {
        if let Err(err) = self.rt.block_on(self.nvim.input(keys)) {
            tracing::error!(%err, keys, "nvim_input failed");
        }
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        if let Err(err) = self.rt.block_on(self.nvim.try_resize(cols, rows)) {
            tracing::error!(%err, cols, rows, "nvim_ui_try_resize failed");
        }
    }

    /// ウィンドウの ×。破棄の意味論は `ZQ` と同じで、書き戻しは起きない。
    fn on_close_requested(&mut self) {
        let phase = self.session.phase();
        if phase != Phase::Editing {
            tracing::debug!(?phase, "close requested outside of a session");
            return;
        }
        if let Err(err) = self.rt.block_on(self.nvim.quit_session()) {
            tracing::error!(%err, "AnviQuit failed");
        }
    }

    fn finish_session(&mut self) {
        let phase = self.session.phase();
        if phase != Phase::Editing {
            tracing::warn!(?phase, "session_end outside of a session");
            return;
        }

        self.notify_gui(UserEvent::Hide);
        let restored = focus::set_foreground(self.target_hwnd);
        let applied = self.session.on_end();
        if let Err(err) = restored {
            // フォーカスが戻らなくても書き戻しは試す。UIA `SetValue` 経路は
            // フォーカスに依存せず（DESIGN 9.1）、諦めると編集内容が消えるだけ。
            tracing::error!(%err, "focus restore failed; attempting write-back anyway");
        }

        match applied {
            Applied::WriteBack(lines) => {
                if let Err(err) = self.uia.write_back(&lines) {
                    tracing::error!(%err, "write-back failed");
                }
            }
            // 相手アプリの undo 履歴を無駄に汚さない（DESIGN 9.4）。
            Applied::Unchanged => tracing::debug!("content unchanged; write-back skipped"),
            Applied::Discarded => tracing::debug!("never written; discarded"),
        }
    }

    /// 安全網（DESIGN 6.3）。ペアを再起動して Idle へ戻す。
    fn recover(&mut self, why: &str) {
        if self.shutting_down.load(Ordering::SeqCst) {
            tracing::debug!(why, "recovery suppressed while shutting down");
            return;
        }
        tracing::warn!(why, "restarting nvim");
        if let Err(err) = self.restart_pair() {
            tracing::error!(%err, "pair restart failed; the host can no longer edit");
        }
    }

    fn restart_pair(&mut self) -> anyhow::Result<()> {
        // 世代を先に進める。`NvimServer::shutdown()` は切断監視を止めるが、io ループが
        // それより先に畳まれていれば `Disconnected` は既に積まれている。旧ペアの転送
        // タスクをここで黙らせないと、再起動が無限に連鎖する。
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        if let Err(err) = self.rt.block_on(self.nvim.shutdown()) {
            tracing::warn!(%err, "could not shut down the old nvim");
        }

        // `spawn_pair` が `ui_attach` までやり直す。GUI は新しいグリッドを
        // 最初の `grid_resize` で受け取る。
        let pair = spawn_pair(&self.bundle, &self.rt)?;
        forward(
            &self.rt,
            pair.host_rx,
            self.tx.clone(),
            generation,
            Arc::clone(&self.generation),
        );
        pump_redraw(
            &self.rt,
            pair.redraw_rx,
            self.proxy.clone(),
            generation,
            Arc::clone(&self.generation),
        );
        self.nvim = pair.nvim;
        self.target_hwnd = 0;
        self.session.reset();
        // 編集中に落ちた場合はウィンドウが出たままなので引っ込める。
        self.notify_gui(UserEvent::Hide);
        tracing::info!(generation, "pair restarted");
        Ok(())
    }

    /// 意図的シャットダウン。`shutting_down` は GUI 側が `Cmd::Exit` より先に
    /// 立てているため、ここで生じる切断はリカバリを誘発しない（DESIGN 6.3 誤発火）。
    fn teardown(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Err(err) = self.rt.block_on(self.nvim.shutdown()) {
            tracing::warn!(%err, "could not shut down nvim during shutdown");
        }
        tracing::info!("pair torn down");
    }

    fn notify_gui(&self, event: UserEvent) {
        if let Err(err) = self.proxy.send_event(event) {
            tracing::error!(%err, "the gui event loop is gone; event dropped");
        }
    }
}

/// `NvimServer` のイベント列を `Cmd::Host` へ流す（tokio ランタイム上のタスク）。
///
/// 世代が進んでいたら旧ペアのイベントなので捨てて畳む。
fn forward(
    rt: &Handle,
    mut host_rx: UnboundedReceiver<HostEvent>,
    tx: Sender<Cmd>,
    generation: u64,
    current: Arc<AtomicU64>,
) {
    rt.spawn(async move {
        while let Some(event) = host_rx.recv().await {
            if current.load(Ordering::SeqCst) != generation {
                tracing::debug!(?event, generation, "event from a superseded pair dropped");
                return;
            }
            if tx.send(Cmd::Host(event)).is_err() {
                return;
            }
        }
    });
}

/// `redraw` バッチを GUI スレッドへ流す（tokio ランタイム上のタスク）。
///
/// パースはしない。UI 状態への適用は winit のループの中でやる（`redraw` は毎打鍵
/// 飛んでくるので、RPC の io タスクを重くしない）。世代の扱いは [`forward`] と同じ。
fn pump_redraw(
    rt: &Handle,
    mut redraw_rx: UnboundedReceiver<Vec<rmpv::Value>>,
    proxy: EventLoopProxy<UserEvent>,
    generation: u64,
    current: Arc<AtomicU64>,
) {
    rt.spawn(async move {
        while let Some(batch) = redraw_rx.recv().await {
            if current.load(Ordering::SeqCst) != generation {
                tracing::debug!(generation, "redraw from a superseded pair dropped");
                return;
            }
            if proxy.send_event(UserEvent::Redraw(batch)).is_err() {
                tracing::debug!("the gui event loop is gone; redraw pump stopped");
                return;
            }
        }
    });
}
