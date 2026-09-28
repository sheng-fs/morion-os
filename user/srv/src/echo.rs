use morion::syscall::*;

/// 域 3 — echo 服务: 同步 IPC 演示, 循环 `recv` → `reply` (回显 tag + 1)。
pub fn run() {
    loop {
        let tag = sys_recv();
        // 回复当前调用者 (回复目标由内核在 recv 时记录)。
        if sys_reply(tag + 1) != 1 {
            println("echo: reply FAILED");
        }
    }
}
