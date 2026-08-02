//! 空きポートの取得（DESIGN §4.6）。

use std::net::{Ipv4Addr, SocketAddr, TcpListener};

/// ループバックの空き TCP ポートを 1 つ選ぶ。
///
/// OS に `127.0.0.1:0` を割り当てさせ、番号だけ受け取って listener を閉じる。
/// 閉じてから nvim が bind するまでの隙間で他プロセスに奪われうる（TOCTOU）。
/// 呼び出し側は bind 失敗時に取り直して再試行すること（→ [`crate::NvimServer::spawn`]）。
pub fn pick_free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::pick_free_port;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    #[test]
    fn returns_a_bindable_port() {
        let port = pick_free_port().expect("failed to pick a free port");
        assert_ne!(port, 0, "port 0 means the OS assignment was not read back");
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .expect("the picked port should be bindable right after being picked");
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    #[test]
    fn releases_the_listener_so_the_port_is_reusable_twice() {
        let port = pick_free_port().expect("failed to pick a free port");
        for _ in 0..2 {
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                .expect("picked port must not stay occupied by pick_free_port itself");
            drop(listener);
        }
    }
}
