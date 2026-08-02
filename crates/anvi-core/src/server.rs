//! 常駐 headless Neovim の起動と RPC 接続（DESIGN §3 / §4.6 / §5 / §6.3）。

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use nvim_rs::compat::tokio::Compat;
use nvim_rs::error::LoopError;
use nvim_rs::{Handler, Neovim, UiAttachOptions};
use rmpv::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader, WriteHalf};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::event::HostEvent;
use crate::port::pick_free_port;

/// nvim が listen するまでのラグを吸収する接続リトライ（→ DESIGN §4.6）。
const CONNECT_ATTEMPTS: u32 = 100;
const CONNECT_INTERVAL: Duration = Duration::from_millis(20);
/// ポートを他プロセスに奪われた場合（TOCTOU）に取り直す回数（→ DESIGN §4.6）。
const PORT_ATTEMPTS: u32 = 5;
/// 握手の期限。ポートの奪い主に繋がると応答が返らないため、無応答で固まらせない。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// host が nvim へ書き込む側の型。`new_tcp` が返す writer に合わせて固定される。
type HostWriter = Compat<WriteHalf<TcpStream>>;

/// 常駐 nvim の起動設定。
#[derive(Debug, Clone)]
pub struct NvimConfig {
    pub nvim_exe: PathBuf,
    pub runtime_dir: PathBuf,
    pub appname: String,
}

/// [`NvimServer::spawn`] が返す受信口一式。
///
/// host イベントと `redraw` は流量も寿命も違う（`redraw` は UI がアタッチして
/// いる間だけ、しかもキー 1 打ごとに来る）ので、同じチャンネルに混ぜない。
#[derive(Debug)]
pub struct NvimHandles {
    pub host: UnboundedReceiver<HostEvent>,
    /// `redraw` 通知の params を **未パースのまま** 運ぶ。解釈は
    /// [`crate::ui`] 側の仕事で、RPC の io タスクを重くしないため。
    pub redraw: UnboundedReceiver<Vec<Value>>,
}

/// 常駐 headless nvim と、その RPC 接続。
pub struct NvimServer {
    child: Child,
    port: u16,
    nvim: Neovim<HostWriter>,
    /// io loop の終了を監視して [`HostEvent::Disconnected`] を送るタスク。
    io_watch: JoinHandle<()>,
}

// `Neovim<W>` は Debug を実装しないため手で書く。
impl std::fmt::Debug for NvimServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvimServer")
            .field("port", &self.port)
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl NvimServer {
    /// `nvim --headless --listen 127.0.0.1:PORT -u <runtime_dir>/init.lua --noplugin`
    /// を `NVIM_APPNAME` 付きで起動し、TCP で接続して host のチャンネルを登録する。
    ///
    /// ポートを奪われて nvim が bind に失敗した場合（TOCTOU、→ DESIGN §4.6）は、
    /// ポートを取り直して再試行する。
    pub async fn spawn(cfg: &NvimConfig) -> anyhow::Result<(Self, NvimHandles)> {
        let init_lua = cfg.runtime_dir.join("init.lua");
        if !init_lua.is_file() {
            bail!(
                "bundled init.lua not found at {} (runtime_dir must contain init.lua and lua/anvi/)",
                init_lua.display()
            );
        }

        let (host_tx, host_rx) = mpsc::unbounded_channel();
        let (redraw_tx, redraw_rx) = mpsc::unbounded_channel();
        let handler = EventHandler {
            host: host_tx,
            redraw: redraw_tx,
        };
        let mut failures = Vec::new();

        for attempt in 1..=PORT_ATTEMPTS {
            let port = pick_free_port().context("failed to pick a free TCP port for nvim")?;
            // spawn 自体の失敗（exe が無い等）は再試行で直らないので即座に返す。
            // `?` で抜けた場合も kill_on_drop により子は始末される。
            let mut child = spawn_child(cfg, &init_lua, port)?;
            drain_stderr(&mut child, port)?;

            match attach(port, &mut child, &handler).await {
                Ok((nvim, io)) => {
                    let io_watch = watch_io(io, handler.host.clone());
                    return Ok((
                        Self {
                            child,
                            port,
                            nvim,
                            io_watch,
                        },
                        NvimHandles {
                            host: host_rx,
                            redraw: redraw_rx,
                        },
                    ));
                }
                Err(e) => {
                    warn!(port, attempt, "nvim did not come up: {e:#}");
                    failures.push(format!("port {port}: {e:#}"));
                    // child はこのイテレーションの終わりに drop され、kill_on_drop で殺される。
                }
            }
        }

        Err(anyhow!(
            "nvim ({}) did not come up on {PORT_ATTEMPTS} different ports [{}]; \
             the nvim.stderr log records carry nvim's own message",
            cfg.nvim_exe.display(),
            failures.join("; ")
        ))
    }

    /// セッションを開始する。lines をバッファへ流し込むのは nvim 側の責務。
    pub async fn start_session(
        &self,
        lines: &[String],
        filetype: Option<&str>,
    ) -> anyhow::Result<()> {
        let lines = Value::Array(lines.iter().map(|l| Value::from(l.as_str())).collect());
        let filetype = filetype.map_or(Value::Nil, Value::from);
        self.nvim
            .exec_lua("require('anvi').start_session(...)", vec![lines, filetype])
            .await
            .context("require('anvi').start_session failed")?;
        Ok(())
    }

    /// この RPC チャンネルを UI クライアントとして登録する。以後 `redraw` 通知が
    /// [`NvimHandles::redraw`] へ流れてくる。
    ///
    /// 立てるのは `rgb` と `ext_linegrid` だけ。cmdline / メッセージ / 補完メニューは
    /// nvim にグリッドへ描かせる（外部化すると自前で全部組み直すことになる）。
    pub async fn attach_ui(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let mut opts = UiAttachOptions::new();
        opts.set_rgb(true).set_linegrid_external(true);
        self.nvim
            .ui_attach(i64::from(cols), i64::from(rows), &opts)
            .await
            .with_context(|| format!("nvim_ui_attach({cols}, {rows}) failed"))?;
        debug!(cols, rows, "attached as a ui client");
        Ok(())
    }

    /// ウィンドウのリサイズを nvim へ伝える。
    pub async fn try_resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.nvim
            .ui_try_resize(i64::from(cols), i64::from(rows))
            .await
            .with_context(|| format!("nvim_ui_try_resize({cols}, {rows}) failed"))?;
        Ok(())
    }

    /// key-notation（→ [`crate::ui::input`]）をそのまま nvim へ流し込む。
    pub async fn input(&self, keys: &str) -> anyhow::Result<()> {
        let written = self
            .nvim
            .input(keys)
            .await
            .with_context(|| format!("nvim_input({keys:?}) failed"))?;
        // nvim は「今回受け取れたバイト数」を返す。入力バッファが埋まっていると
        // 要求より短くなるが、GUI から来るのは 1 打分（数バイト）なので実際には
        // 起こらない。仮に起きても再送はしない ── 取りこぼしを黙って補うより、
        // ログに残して原因を見えるようにする。
        if usize::try_from(written).ok().is_none_or(|n| n < keys.len()) {
            debug!(
                written,
                requested = keys.len(),
                "nvim_input did not take every byte"
            );
        }
        Ok(())
    }

    /// `AnviQuit` を実行する（ウィンドウの × 用）。破棄の意味論は `ZQ` と同じ。
    pub async fn quit_session(&self) -> anyhow::Result<()> {
        self.nvim
            .command("AnviQuit")
            .await
            .context("AnviQuit failed")?;
        Ok(())
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// nvim を殺す（意図的なシャットダウン / ペア再起動）。
    ///
    /// 切断監視も止める。意図的な終了で安全網（→ DESIGN §6.3）を誤発火させないため。
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.io_watch.abort();
        self.child
            .kill()
            .await
            .context("failed to kill the nvim child process")?;
        Ok(())
    }
}

fn spawn_child(cfg: &NvimConfig, init_lua: &Path, port: u16) -> anyhow::Result<Child> {
    let mut command = Command::new(&cfg.nvim_exe);
    // host は GUI サブシステムでコンソールを持たない。何も指定しないと nvim
    // （コンソールアプリ）が自分でコンソールウィンドウを開いてしまう。
    #[cfg(windows)]
    {
        // `creation_flags` は tokio の Command が Windows で直接提供している。
        /// `CREATE_NO_WINDOW`。`windows` クレートを core に持ち込まないため直値。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .arg("--headless")
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("-u")
        .arg(init_lua)
        .arg("--noplugin")
        // 既存環境からの隔離（→ DESIGN §5.2）。VIMRUNTIME / VIM が環境に居ると
        // 同梱 nvim が他人の runtime を掴む。exe 相対の解決（= 同梱物）に固定する。
        .env("NVIM_APPNAME", &cfg.appname)
        .env_remove("VIMRUNTIME")
        .env_remove("VIM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn nvim at {} (NVIM_APPNAME={})",
                cfg.nvim_exe.display(),
                cfg.appname
            )
        })
}

/// nvim の stderr を読み捨てずにログへ流す。bind 失敗の理由はここにしか出ない。
fn drain_stderr(child: &mut Child, port: u16) -> anyhow::Result<()> {
    let stderr = child
        .stderr
        .take()
        .context("nvim was spawned without a piped stderr")?;
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => warn!(target: "nvim.stderr", port, "{line}"),
                Ok(None) => break,
                Err(e) => {
                    warn!(target: "nvim.stderr", port, "failed to read nvim stderr: {e}");
                    break;
                }
            }
        }
    });
    Ok(())
}

type Attached = (Neovim<HostWriter>, JoinHandle<Result<(), Box<LoopError>>>);

/// 接続して host のチャンネルを登録するまで。失敗はすべてポート再試行の理由になる。
async fn attach(port: u16, child: &mut Child, handler: &EventHandler) -> anyhow::Result<Attached> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let (nvim, io) = connect(addr, child, handler).await?;

    // ポートを奪われていると nvim ではない相手に繋がり、応答が返ってこない。
    // 期限を切らないと host の起動がここで永久に止まる（→ DESIGN §4.6）。
    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, register_host(&nvim)).await;
    match handshake {
        Ok(Ok(chan)) => {
            debug!(port, chan, "registered the host channel with nvim");
            Ok((nvim, io))
        }
        Ok(Err(e)) => {
            io.abort();
            Err(e)
        }
        Err(_) => {
            io.abort();
            Err(anyhow!(
                "the server at {addr} did not answer nvim_get_api_info within \
                 {HANDSHAKE_TIMEOUT:?}; something other than nvim may hold the port"
            ))
        }
    }
}

async fn connect(
    addr: SocketAddr,
    child: &mut Child,
    handler: &EventHandler,
) -> anyhow::Result<Attached> {
    let mut last_err = None;

    for _ in 0..CONNECT_ATTEMPTS {
        if let Some(status) = child
            .try_wait()
            .context("failed to poll the nvim child process")?
        {
            // bind に失敗して即死した（→ DESIGN §4.6）。ポートを取り直せば直りうる。
            bail!("nvim exited before accepting a connection ({status})");
        }
        match nvim_rs::create::tokio::new_tcp(addr, handler.clone()).await {
            Ok(attached) => return Ok(attached),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(CONNECT_INTERVAL).await;
            }
        }
    }

    let last_err = last_err.expect("CONNECT_ATTEMPTS > 0 guarantees at least one attempt");
    Err(anyhow!(
        "could not connect to the nvim server at {addr} within {:?} ({CONNECT_ATTEMPTS} attempts); \
         last error: {last_err}",
        CONNECT_INTERVAL * CONNECT_ATTEMPTS
    ))
}

/// 自分のチャンネル ID を取得して init.lua に登録させる（→ DESIGN §5.5）。
async fn register_host(nvim: &Neovim<HostWriter>) -> anyhow::Result<i64> {
    let info = nvim
        .get_api_info()
        .await
        .context("nvim_get_api_info failed")?;
    let first = info
        .first()
        .context("nvim_get_api_info returned an empty array")?;
    let chan = first.as_i64().ok_or_else(|| {
        anyhow!("nvim_get_api_info's first element is not a channel id: {first:?}")
    })?;
    nvim.exec_lua("require('anvi').set_host(...)", vec![Value::from(chan)])
        .await
        .context(
            "require('anvi').set_host failed; the bundled init.lua did not establish the \
             contract. Check that runtime/init.lua and runtime/lua/anvi/init.lua are the \
             real files — an empty or truncated copy fails exactly like this, and nvim skips \
             an unreadable -u file without a word",
        )?;
    Ok(chan)
}

/// io loop の終了 = nvim の消滅。安全網の「正」の検知系（→ DESIGN §6.3）。
fn watch_io(
    io: JoinHandle<Result<(), Box<LoopError>>>,
    tx: UnboundedSender<HostEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        match io.await {
            Ok(Ok(())) => info!("the nvim rpc io loop ended"),
            Ok(Err(e)) => warn!("the nvim rpc io loop failed: {e}"),
            Err(e) => warn!("the nvim rpc io task did not finish normally: {e}"),
        }
        if tx.send(HostEvent::Disconnected).is_err() {
            debug!("nobody is listening for host events anymore; disconnect not reported");
        }
    })
}

/// nvim からの通知の受け口。接続のたびに clone される（`new_tcp` が所有するため）。
#[derive(Clone)]
struct EventHandler {
    host: UnboundedSender<HostEvent>,
    redraw: UnboundedSender<Vec<Value>>,
}

#[async_trait::async_trait]
impl Handler for EventHandler {
    type Writer = HostWriter;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Neovim<HostWriter>) {
        // `redraw` は解釈せずそのまま渡す。UI プロトコルの意味論は core の `ui` が
        // 持っており、ここで触ると RPC の io タスクが描画の都合で重くなる。
        if name == "redraw" {
            if self.redraw.send(args).is_err() {
                // UI が居ない（まだアタッチしていない / もう畳んだ）だけで異常ではない。
                debug!("nobody is listening for redraw batches anymore; dropped");
            }
            return;
        }

        match parse_notification(&name, &args) {
            Ok(Some(event)) => {
                if self.host.send(event).is_err() {
                    warn!(
                        notification = name,
                        "nobody is listening for host events anymore"
                    );
                }
            }
            Ok(None) => warn!(
                notification = name,
                "unknown notification from nvim; ignored"
            ),
            Err(e) => error!(notification = name, "malformed notification from nvim: {e}"),
        }
    }
}

/// `Ok(Some)` = 既知の通知、`Ok(None)` = 未知の名前、`Err` = payload が契約と合わない。
fn parse_notification(name: &str, args: &[Value]) -> Result<Option<HostEvent>, String> {
    match name {
        "session_write" => {
            let [Value::Array(lines)] = args else {
                return Err(format!(
                    "session_write expects one array of lines, got {args:?}"
                ));
            };
            let mut out = Vec::with_capacity(lines.len());
            for line in lines {
                let line = line
                    .as_str()
                    .ok_or_else(|| format!("session_write line is not a string: {line:?}"))?;
                out.push(line.to_owned());
            }
            Ok(Some(HostEvent::SessionWrite(out)))
        }
        "session_end" => {
            warn_unexpected_payload(name, args);
            Ok(Some(HostEvent::SessionEnd))
        }
        "nvim_dying" => {
            warn_unexpected_payload(name, args);
            Ok(Some(HostEvent::NvimDying))
        }
        "init_error" => {
            let [Value::Map(fields)] = args else {
                return Err(format!(
                    "init_error expects one map with kind/message, got {args:?}"
                ));
            };
            Ok(Some(HostEvent::InitError {
                kind: string_field(fields, "init_error", "kind")?,
                message: string_field(fields, "init_error", "message")?,
            }))
        }
        "config_resolved" => {
            let [Value::Map(fields)] = args else {
                return Err(format!(
                    "config_resolved expects one map with dir/loaded, got {args:?}"
                ));
            };
            let loaded = field(fields, "config_resolved", "loaded")?;
            let loaded = loaded
                .as_bool()
                .ok_or_else(|| format!("config_resolved `loaded` is not a bool: {loaded:?}"))?;
            Ok(Some(HostEvent::ConfigResolved {
                dir: string_field(fields, "config_resolved", "dir")?,
                loaded,
            }))
        }
        _ => Ok(None),
    }
}

/// payload を持たない通知の args を検査する。
///
/// 想定外の args でもイベント自体は落とさない。運ぶ情報がない通知であり、特に
/// `session_end` を落とすと host が Editing のまま取り残されるため。
fn warn_unexpected_payload(name: &str, args: &[Value]) {
    let expected = matches!(args, [] | [Value::Nil]);
    if !expected {
        warn!(
            notification = name,
            "expected a nil payload, got {args:?}; ignoring the payload"
        );
    }
}

fn field<'a>(fields: &'a [(Value, Value)], event: &str, key: &str) -> Result<&'a Value, String> {
    fields
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .ok_or_else(|| format!("{event} payload has no `{key}` field: {fields:?}"))
}

fn string_field(fields: &[(Value, Value)], event: &str, key: &str) -> Result<String, String> {
    let value = field(fields, event, key)?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{event} `{key}` is not a string: {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::{NvimConfig, NvimServer, parse_notification};
    use crate::event::HostEvent;
    use rmpv::Value;
    use std::path::PathBuf;

    fn s(text: &str) -> Value {
        Value::from(text)
    }

    #[test]
    fn session_write_carries_the_lines() {
        let args = vec![Value::Array(vec![s("a"), s("日本語")])];
        assert_eq!(
            parse_notification("session_write", &args),
            Ok(Some(HostEvent::SessionWrite(vec![
                "a".to_string(),
                "日本語".to_string()
            ])))
        );
    }

    #[test]
    fn session_write_accepts_an_empty_buffer() {
        let args = vec![Value::Array(vec![s("")])];
        assert_eq!(
            parse_notification("session_write", &args),
            Ok(Some(HostEvent::SessionWrite(vec![String::new()])))
        );
    }

    #[test]
    fn session_write_with_a_non_array_payload_is_malformed() {
        assert!(parse_notification("session_write", &[s("oops")]).is_err());
        assert!(parse_notification("session_write", &[]).is_err());
        assert!(
            parse_notification(
                "session_write",
                &[Value::Array(vec![]), Value::Array(vec![])]
            )
            .is_err()
        );
    }

    #[test]
    fn session_write_with_a_non_string_line_is_malformed() {
        let args = vec![Value::Array(vec![s("a"), Value::from(7)])];
        assert!(parse_notification("session_write", &args).is_err());
    }

    #[test]
    fn nil_payload_notifications_are_accepted() {
        // 実機の `vim.rpcnotify(chan, "session_end", nil)` は args = [Nil] を送る。
        assert_eq!(
            parse_notification("session_end", &[Value::Nil]),
            Ok(Some(HostEvent::SessionEnd))
        );
        assert_eq!(
            parse_notification("session_end", &[]),
            Ok(Some(HostEvent::SessionEnd))
        );
        assert_eq!(
            parse_notification("nvim_dying", &[Value::Nil]),
            Ok(Some(HostEvent::NvimDying))
        );
    }

    #[test]
    fn session_end_survives_an_unexpected_payload() {
        // 落とすと host が Editing のまま取り残される。
        assert_eq!(
            parse_notification("session_end", &[s("junk")]),
            Ok(Some(HostEvent::SessionEnd))
        );
    }

    #[test]
    fn init_error_carries_kind_and_message() {
        let args = vec![Value::Map(vec![
            (s("message"), s("boom")),
            (s("kind"), s("user_config_error")),
        ])];
        assert_eq!(
            parse_notification("init_error", &args),
            Ok(Some(HostEvent::InitError {
                kind: "user_config_error".to_string(),
                message: "boom".to_string(),
            }))
        );
    }

    #[test]
    fn init_error_without_the_contract_fields_is_malformed() {
        let missing = vec![Value::Map(vec![(s("kind"), s("user_config_error"))])];
        assert!(parse_notification("init_error", &missing).is_err());
        let wrong_type = vec![Value::Map(vec![
            (s("kind"), s("user_config_error")),
            (s("message"), Value::from(1)),
        ])];
        assert!(parse_notification("init_error", &wrong_type).is_err());
        assert!(parse_notification("init_error", &[Value::Nil]).is_err());
    }

    #[test]
    fn config_resolved_carries_the_dir_and_whether_it_loaded() {
        let args = vec![Value::Map(vec![
            (s("dir"), s(r"C:\Users\me\.config\anvi")),
            (s("loaded"), Value::from(false)),
        ])];
        assert_eq!(
            parse_notification("config_resolved", &args),
            Ok(Some(HostEvent::ConfigResolved {
                dir: r"C:\Users\me\.config\anvi".to_string(),
                loaded: false,
            }))
        );
    }

    #[test]
    fn config_resolved_without_the_contract_fields_is_malformed() {
        let missing = vec![Value::Map(vec![(s("dir"), s("x"))])];
        assert!(parse_notification("config_resolved", &missing).is_err());
        let wrong_type = vec![Value::Map(vec![
            (s("dir"), s("x")),
            (s("loaded"), s("true")),
        ])];
        assert!(parse_notification("config_resolved", &wrong_type).is_err());
    }

    #[test]
    fn unknown_notifications_are_ignored_not_errors() {
        assert_eq!(parse_notification("whatever", &[Value::Nil]), Ok(None));
    }

    #[tokio::test]
    async fn spawn_fails_fast_when_the_bundled_init_lua_is_missing() {
        let cfg = NvimConfig {
            nvim_exe: PathBuf::from("/nonexistent/nvim"),
            runtime_dir: PathBuf::from("/nonexistent/runtime"),
            appname: "anvi-test".to_string(),
        };
        let err = NvimServer::spawn(&cfg)
            .await
            .expect_err("a missing init.lua must not be tolerated")
            .to_string();
        assert!(err.contains("init.lua"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn spawn_reports_a_missing_nvim_executable() {
        let dir = std::env::temp_dir().join(format!("anvi-core-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("failed to create the fake runtime dir");
        std::fs::write(dir.join("init.lua"), "-- fake\n").expect("failed to write init.lua");

        let cfg = NvimConfig {
            nvim_exe: dir.join("nvim-does-not-exist"),
            runtime_dir: dir.clone(),
            appname: "anvi-test".to_string(),
        };
        let err = NvimServer::spawn(&cfg)
            .await
            .expect_err("spawning a nonexistent executable must fail")
            .to_string();

        std::fs::remove_dir_all(&dir).expect("failed to clean up the fake runtime dir");
        assert!(
            err.contains("failed to spawn nvim"),
            "unexpected error: {err}"
        );
    }
}
