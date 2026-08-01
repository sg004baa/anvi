//! anywhere-nvim — どこでも Neovim でテキスト編集するための常駐 host。
//!
//! スレッドモデル（DESIGN 11.2）。COM のアパートメント地雷を踏まないよう最初から分ける。
//!
//! - main スレッド: winit のイベントループ。編集ウィンドウの描画・キーボード・IME に
//!   加え、トレイとグローバルホットキーの隠しウィンドウのメッセージもここで汲まれる
//!   （どちらも「登録したスレッドでループが回っていること」を要求するので、
//!   [`gui::run`] を呼ぶ直前に main スレッドで作る）
//! - `uia` の MTA スレッド: UI Automation の一切
//! - `controller` スレッド: `Session` の所有と RPC 呼び出し
//! - tokio ランタイム: nvim との msgpack-rpc

#[cfg(not(windows))]
compile_error!("anywhere-nvim targets Windows 11 only");

mod bundle;
mod clipboard;
mod controller;
mod focus;
mod gui;
mod hotkey;
mod keys;
mod tray;
mod uia;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, anyhow};
use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoop;

use controller::Cmd;
use gui::UserEvent;

/// `RUST_LOG` が無いときのログレベル。設定の既定値ではなくコード上のリテラル。
const DEFAULT_LOG: &str = "info";

fn main() -> anyhow::Result<()> {
    init_tracing()?;

    let bundle = bundle::resolve()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the tokio runtime")?;

    // イベントループはウィンドウより先に要る。proxy はここでしか作れないので、
    // トレイ・ホットキー・コントローラのどれよりも先に用意する。
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("failed to create the winit event loop")?;
    let proxy = event_loop.create_proxy();

    let (tx, rx) = std::sync::mpsc::channel::<Cmd>();

    // nvim はアプリ起動時に立ち上がり、終了まで生き続ける（DESIGN 3.1）。
    let pair = controller::spawn_pair(&bundle, rt.handle())?;
    let uia = uia::Uia::start().context("failed to start the UI Automation thread")?;

    let shutting_down = Arc::new(AtomicBool::new(false));
    let controller = controller::start(controller::Boot {
        bundle,
        rt: rt.handle().clone(),
        tx: tx.clone(),
        rx,
        uia,
        pair,
        proxy: proxy.clone(),
        shutting_down: Arc::clone(&shutting_down),
    })?;

    let tray = tray::Tray::new(gui::ProxyHandle::new(proxy.clone()))?;
    let hotkeys = hotkey::Hotkeys::register(gui::ProxyHandle::new(proxy))?;
    tracing::info!("anywhere-nvim is resident");

    // ここから先は main スレッドを winit が占有する。戻るのは終了時だけ。
    let boot = gui::GuiBoot {
        tx: tx.clone(),
        tray,
        hotkeys,
    };
    let result = gui::run(event_loop, boot);

    // ループが止まったら理由に関わらず終了する。フラグはコントローラがペアを
    // 畳むより先に立てる。これより後に起きる RPC 切断でリカバリが誤発火しては
    // ならない（DESIGN 6.3）。
    shutting_down.store(true, Ordering::SeqCst);
    if tx.send(Cmd::Exit).is_err() {
        tracing::error!("controller is already gone");
    }
    // 転送タスクが持つ複製とは別に、こちらの送信端は手放しておく。
    drop(tx);
    controller
        .join()
        .map_err(|_| anyhow!("the controller thread panicked"))?;
    tracing::info!("anywhere-nvim stopped");
    result
}

fn init_tracing() -> anyhow::Result<()> {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        // `RUST_LOG` が在るのに解釈できないのは書き間違い。黙って既定値へ落とさない。
        Err(err) if std::env::var_os("RUST_LOG").is_some() => {
            return Err(anyhow!("invalid RUST_LOG: {err}"));
        }
        Err(_) => EnvFilter::new(DEFAULT_LOG),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|err| anyhow!("failed to install the tracing subscriber: {err}"))
}
