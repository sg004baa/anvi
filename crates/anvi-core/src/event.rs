//! nvim → host の通知契約（DESIGN §5.5 / §6.3 / 付録 A-2）。

/// nvim（同梱 init.lua）から host に届くイベント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// `:w` 系。この時点では保持するだけで書き戻さない（→ DESIGN §4.4）。
    SessionWrite(Vec<String>),
    /// セッション終了。反映するかは host が「保存を受信したか」だけで決めるため、
    /// この通知は反映可否の情報を運ばない。
    SessionEnd,
    /// ローカル設定の解決結果。パスを決めるのは nvim（`stdpath('config')`）なので、
    /// host は聞くだけ。「設定が効かない」の一次情報がこれ。
    ConfigResolved { dir: String, loaded: bool },
    /// ローカル設定の読み込み失敗。起動は続行済みなのでログに残すだけ。
    InitError { kind: String, message: String },
    /// `VimLeavePre` の早期ヒント。終了処理中の rpcnotify はフラッシュされない
    /// 可能性があるため、これ以上の役割を与えない（→ DESIGN §6.3）。
    NvimDying,
    /// RPC io loop ended: nvim is gone (authoritative safety-net trigger, §6.3)
    Disconnected,
}
