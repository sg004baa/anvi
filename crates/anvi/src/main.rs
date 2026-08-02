// コンソールを持たない GUI アプリとして起動する。常駐ツールなので、起動のたびに
// コンソールウィンドウが開いては話にならない。ログは stderr ではなくファイルへ出す
// （[`init_tracing`]）。
#![windows_subsystem = "windows"]

//! anvi — どこでも Neovim でテキスト編集するための常駐 host。
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
compile_error!("anvi targets Windows 11 only");

mod autostart;
mod bundle;
mod clipboard;
mod controller;
mod focus;
mod gui;
mod hotkey;
mod keys;
mod tray;
mod uia;

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, anyhow};
use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoop;

use controller::Cmd;
use gui::UserEvent;

/// `RUST_LOG` が無いときのログレベル。設定の既定値ではなくコード上のリテラル。
const DEFAULT_LOG: &str = "info";

/// ログの置き場所。`%LOCALAPPDATA%\anvi-data` は nvim の data ディレクトリ
/// （`NVIM_APPNAME=anvi`）と同じ名前空間で、消してよいものだけが入る。
const LOG_DIR: &str = "anvi-data\\log";
const LOG_FILE: &str = "anvi.log";

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
    tracing::info!("anvi is resident");

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
    tracing::info!("anvi stopped");
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
    let path = log_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create the log directory: {}", dir.display()))?;
    }
    // 常駐は 1 インスタンスなので、起動のたびに切り詰める。前回分を残しても
    // 増え続けるだけで、調べたいのは常に「今動いているもの」である。
    let file = File::create(&path)
        .with_context(|| format!("failed to open the log file: {}", path.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Arc::new(file))
        .try_init()
        .map_err(|err| anyhow!("failed to install the tracing subscriber: {err}"))
}

/// ログファイルの絶対パス。`%LOCALAPPDATA%` が無い環境は想定しない（既定値へ
/// 落とさず落ちる）。
fn log_path() -> anyhow::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local).join(LOG_DIR).join(LOG_FILE))
}
