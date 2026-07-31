//! anywhere-nvim — どこでも Neovim でテキスト編集するための常駐 host。
//!
//! スレッドモデル（DESIGN 11.2）。COM のアパートメント地雷を踏まないよう最初から分ける。
//!
//! - main スレッド: Win32 メッセージポンプ。トレイ / グローバルホットキー / `SetWinEventHook`
//! - `uia` の MTA スレッド: UI Automation の一切
//! - `controller` スレッド: `Session` の所有と RPC 呼び出し
//! - tokio ランタイム: nvim との msgpack-rpc

#[cfg(not(windows))]
compile_error!("anywhere-nvim targets Windows 11 only");

mod bundle;
mod clipboard;
mod controller;
mod editor;
mod focus;
mod hotkey;
mod keys;
mod tray;
mod uia;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use anyhow::{Context as _, anyhow};
use tracing_subscriber::EnvFilter;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage,
};

use controller::Cmd;

/// `RUST_LOG` が無いときのログレベル。設定の既定値ではなくコード上のリテラル。
const DEFAULT_LOG: &str = "info";

fn main() -> anyhow::Result<()> {
    init_tracing()?;

    let bundle = bundle::resolve()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the tokio runtime")?;

    let (tx, rx) = std::sync::mpsc::channel::<Cmd>();

    // 3 プロセスすべてがアプリ起動時に立ち上がり、終了まで生き続ける（DESIGN 3.1）。
    let pair = controller::spawn_pair(&bundle, rt.handle(), &tx)?;
    let uia = uia::Uia::start().context("failed to start the UI Automation thread")?;

    let shutting_down = Arc::new(AtomicBool::new(false));
    let controller = controller::start(controller::Boot {
        bundle,
        rt: rt.handle().clone(),
        tx: tx.clone(),
        rx,
        uia,
        pair,
        shutting_down: Arc::clone(&shutting_down),
    })?;

    // ここから先は main スレッドをメッセージポンプ専用にする。トレイとホットキーは
    // 登録したスレッドでループが回っていることを要求するため、この順序が必須。
    let tray = tray::Tray::new()?;
    let hotkeys = hotkey::Hotkeys::register()?;
    tracing::info!("anywhere-nvim is resident");

    pump(&tx, &tray, &hotkeys);

    // ポンプが止まったら理由に関わらず終了する。フラグはコントローラがペアを
    // 畳むより先に立てる。これより後に起きる RPC 切断と Neovide の終了で
    // リカバリが誤発火してはならない（DESIGN 6.3）。
    shutting_down.store(true, Ordering::SeqCst);
    if tx.send(Cmd::Exit).is_err() {
        tracing::error!("controller is already gone");
    }
    // 転送タスクとウォッチャが持つ複製とは別に、こちらの送信端は手放しておく。
    drop(tx);
    controller
        .join()
        .map_err(|_| anyhow!("the controller thread panicked"))?;
    tracing::info!("anywhere-nvim stopped");
    Ok(())
}

/// Win32 メッセージポンプ。トレイと `global-hotkey` の隠しウィンドウはこのループで動く。
///
/// イベントは `DispatchMessageW` の中で各クレートのチャンネルへ積まれるので、
/// ディスパッチ直後に吸い出す。
fn pump(tx: &Sender<Cmd>, tray: &tray::Tray, hotkeys: &hotkey::Hotkeys) {
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg は有効なローカル変数。フィルタ無しでこのスレッドのキューから取る。
        match unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 {
            0 => {
                tracing::debug!("WM_QUIT received");
                return;
            }
            -1 => {
                tracing::error!(err = %windows::core::Error::from_thread(), "GetMessageW failed");
                return;
            }
            _ => {}
        }
        // SAFETY: msg は直前に取得した有効なメッセージ。
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        hotkeys.drain(tx);

        if tray.exit_requested() {
            tracing::info!("exit requested from the tray");
            return;
        }
    }
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
