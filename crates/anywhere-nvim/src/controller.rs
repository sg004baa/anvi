//! セッション状態機械を持つスレッド（DESIGN 6、11.2、付録 B）。
//!
//! `Session` はこのスレッドだけが触る。メッセージポンプ（トレイ / ホットキー）と
//! UIA の MTA スレッドとは `Cmd` / `Uia` のチャンネル越しにしか繋がらない。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use anyhow::Context as _;
use anywhere_core::{Applied, HostEvent, NvimConfig, NvimServer, Phase, Session};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::bundle::{APPNAME, Bundle};
use crate::editor::Editor;
use crate::focus;
use crate::uia::Uia;

/// コントローラへの指令。ポンプスレッド / Neovide ウォッチャ / RPC 転送タスクから届く。
#[derive(Debug)]
pub enum Cmd {
    Hotkey,
    Exit,
    /// Neovide が使えなくなった（プロセス終了・ウィンドウ消滅）。理由をそのまま記録する。
    EditorLost(&'static str),
    Host(HostEvent),
}

/// nvim と Neovide の組（DESIGN 付録 C「ペア」）。
pub struct Pair {
    pub nvim: NvimServer,
    pub host_rx: UnboundedReceiver<HostEvent>,
    pub editor: Editor,
}

/// コントローラスレッドに渡す一切。
pub struct Boot {
    pub bundle: Bundle,
    pub rt: Handle,
    pub tx: Sender<Cmd>,
    pub rx: Receiver<Cmd>,
    pub uia: Uia,
    pub pair: Pair,
    /// 意図的シャットダウン中の印。リカバリの誤発火を抑止する（DESIGN 6.3）。
    pub shutting_down: Arc<AtomicBool>,
}

/// ペアを起こす。nvim を起動するのは host の責務であり Neovide ではない（DESIGN 3.2）。
pub fn spawn_pair(bundle: &Bundle, rt: &Handle, tx: &Sender<Cmd>) -> anyhow::Result<Pair> {
    let cfg = NvimConfig {
        nvim_exe: bundle.nvim_exe.clone(),
        runtime_dir: bundle.runtime_dir.clone(),
        appname: APPNAME.to_owned(),
    };
    let (nvim, host_rx) = rt
        .block_on(NvimServer::spawn(&cfg))
        .context("failed to start the nvim server")?;
    let editor = Editor::spawn(&bundle.neovide_exe, nvim.port(), tx.clone())?;
    Ok(Pair {
        nvim,
        host_rx,
        editor,
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
        shutting_down,
    } = boot;
    let Pair {
        nvim,
        host_rx,
        editor,
    } = pair;

    let generation = Arc::new(AtomicU64::new(0));
    forward(&rt, host_rx, tx.clone(), 0, Arc::clone(&generation));

    let mut controller = Controller {
        bundle,
        rt,
        tx,
        uia,
        session: Session::default(),
        nvim,
        editor,
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
    session: Session,
    nvim: NvimServer,
    editor: Editor,
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
                Cmd::EditorLost(why) => self.recover(why),
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
            Phase::Editing => {
                if let Err(err) = self.editor.focus() {
                    tracing::error!(%err, "cannot focus the neovide window");
                }
            }
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
        // Neovide のウィンドウを × で閉じると、実機では **プロセスは生き残り HWND だけが
        // 消える**。通常はウォッチャが先に気付いて作り直すが、押下と同時だと間に合わない。
        // ここでペアを作り直したうえで **この押下は捨てる**。作り直しの直後は新しい
        // Neovide がフォーカスを持ち去るため、そのまま capture すると編集対象を取り違える
        // （DESIGN 4.3 で却下した案と同じ事故）。
        if !self.editor.window_is_alive() {
            tracing::warn!(
                "the neovide window vanished; restarting the pair and dropping this press"
            );
            self.recover("neovide window vanished");
            return Ok(());
        }

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

        // filetype は v1 では指定しない。見た目とオプションはローカル設定の領分
        // （DESIGN 5.4）。
        if let Err(err) = self
            .rt
            .block_on(self.nvim.start_session(&captured.lines, None))
        {
            self.session.abort_capture();
            return Err(err.context("start_session failed"));
        }

        self.target_hwnd = captured.hwnd;
        self.session.begin_edit(captured.lines);

        if let Err(err) = self.editor.show_and_focus() {
            // 画面が出ないなら編集できない。掴んだセッションを畳んで Idle へ戻す。
            self.session.reset();
            return Err(err.context("cannot show the neovide window"));
        }
        Ok(())
    }

    /// DESIGN 付録 B 通知ハンドラ。
    fn on_host(&mut self, event: HostEvent) {
        match event {
            // 保持するだけ。書き戻しはセッション終了時（DESIGN 4.4）。
            HostEvent::SessionWrite(lines) => self.session.on_write(lines),
            HostEvent::SessionEnd => self.finish_session(),
            // ローカル設定の読み込み失敗。起動は続行済み（DESIGN 5.4）。
            HostEvent::InitError { kind, message } => {
                tracing::warn!(kind, message, "user config error");
            }
            // 早期ヒント。正はこの下の Disconnected（DESIGN 6.3）。
            HostEvent::NvimDying => self.recover("nvim reported VimLeavePre"),
            HostEvent::Disconnected => self.recover("nvim rpc disconnected"),
        }
    }

    fn finish_session(&mut self) {
        let phase = self.session.phase();
        if phase != Phase::Editing {
            tracing::warn!(?phase, "session_end outside of a session");
            return;
        }

        if let Err(err) = self.editor.hide() {
            tracing::error!(%err, "cannot hide the neovide window");
        }
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
        tracing::warn!(why, "restarting the nvim + neovide pair");
        if let Err(err) = self.restart_pair() {
            tracing::error!(%err, "pair restart failed; the host can no longer edit");
        }
    }

    fn restart_pair(&mut self) -> anyhow::Result<()> {
        // 世代を先に進める。`NvimServer::shutdown()` は切断監視を止めるが、io ループが
        // それより先に畳まれていれば `Disconnected` は既に積まれている。旧ペアの転送
        // タスクをここで黙らせないと、再起動が無限に連鎖する。
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        if let Err(err) = self.editor.kill() {
            tracing::warn!(%err, "could not kill the old neovide");
        }
        if let Err(err) = self.rt.block_on(self.nvim.shutdown()) {
            tracing::warn!(%err, "could not shut down the old nvim");
        }

        let pair = spawn_pair(&self.bundle, &self.rt, &self.tx)?;
        forward(
            &self.rt,
            pair.host_rx,
            self.tx.clone(),
            generation,
            Arc::clone(&self.generation),
        );
        self.nvim = pair.nvim;
        // `Editor::spawn` が起動時の待避（画面外 + SW_HIDE）まで済ませている。
        self.editor = pair.editor;
        self.target_hwnd = 0;
        self.session.reset();
        tracing::info!(generation, "pair restarted");
        Ok(())
    }

    /// 意図的シャットダウン。`shutting_down` はポンプ側が `Cmd::Exit` より先に
    /// 立てているため、ここで生じる切断・プロセス終了はリカバリを誘発しない
    /// （DESIGN 6.3 誤発火）。
    fn teardown(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Err(err) = self.editor.kill() {
            tracing::warn!(%err, "could not kill neovide during shutdown");
        }
        if let Err(err) = self.rt.block_on(self.nvim.shutdown()) {
            tracing::warn!(%err, "could not shut down nvim during shutdown");
        }
        tracing::info!("pair torn down");
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
