//! 域 23 — httpd_srv：独立的用户态 HTTP 服务（N8.2 内建 HTTP 的拆分产物）。
//!
//! N8.2 时期 HTTP 响应**内建在 netstack_srv**；本次把它拆成一个独立服务域：httpd 只做
//! 「监听 `10.0.2.15:80` → 收请求 → 回固定响应 → 关连接」，TCP/IP/ARP 仍由 netstack_srv
//! 提供。协议栈因此不再内含任何应用层服务。
//!
//! 端口能力：内核授 `Net(80,80)`，`tcp_listen(80)` 先经 `SYS_NET_BIND` 登记再监听 ——
//! 协议栈侧 `tcp_listen_internal` 会用 `SYS_NET_OWNER` 核对，端口归属**不可伪造**。
//!
//! 数据路径：app/shell 的 `wget` 对 `10.0.2.15:80` 发起回环 TCP；本服务 accept 后回一个
//! 固定响应（`hello from morion-guest-httpd`），握手/收发/FIN 全在客户机内完成。

use morion::net;
use morion::syscall::{println, sys_sleep};

/// HTTP 监听端口（客户机内建服务，配合回环 TCP 供 app/shell 的 `wget` 取用）。
const HTTP_PORT: u16 = 80;

/// 固定响应（`Content-Length` 与 body 一致：`hello from morion-guest-httpd\n` = 30 字节）。
const HTTP_RESP: &[u8] = b"HTTP/1.0 200 OK\r\nContent-Length: 30\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nhello from morion-guest-httpd\n";

fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

/// 域 23 — httpd_srv 入口。
pub fn run() {
    // 建监听者（内核端口门禁在内登记归属）。失败即无 `Net(80)` 能力或无网卡。
    let listener = net::tcp_listen(HTTP_PORT);
    if listener == 0 {
        println("httpd: listen :80 FAILED (no Net(80) cap / no NIC), idle");
        idle();
    }
    println("httpd: listening on :80 (loopback TCP)");

    loop {
        let conn = net::tcp_accept(listener);
        if conn == 0 {
            sys_sleep(10);
            continue;
        }
        // 收请求（内容丢弃 —— 只回固定响应），随后发送并关闭连接。
        let mut buf = [0u8; 512];
        let mut i = 0;
        while i < 50 {
            if net::tcp_recv(conn, &mut buf) > 0 {
                break;
            }
            sys_sleep(20);
            i += 1;
        }
        let _ = net::tcp_send(conn, HTTP_RESP);
        let _ = net::tcp_close(conn);
    }
}
