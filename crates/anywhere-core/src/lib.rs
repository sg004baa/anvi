//! Portable core of anywhere-nvim: the resident headless Neovim server, the
//! host/nvim event contract, the session state machine and text normalization.
//!
//! Everything OS-specific (window control, UI Automation, clipboard, hotkey,
//! tray) lives in the `anywhere-nvim` binary crate.

pub mod event;
pub mod port;
pub mod server;
pub mod session;
pub mod text;
pub mod ui;

pub use event::HostEvent;
pub use server::{NvimConfig, NvimHandles, NvimServer};
pub use session::{Applied, Phase, Session};
