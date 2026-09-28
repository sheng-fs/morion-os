use crate::common::ECHO_QUIT_TAG;
use morion::syscall::*;

/// 域 3 — echo 服务: 同步 IPC 演示, 循环 `recv` → `reply` (回显 tag + 1)。
pub fn run() {
    loop {
        let tag = sys_recv();
        // 控制消息 (E3c): 让本服务退出 —— 内核走正常的「退出即回收」路径把这个任务终结,
        // 域本身留着 (引导期服务域是白名单), 由监督者 init 原地把它重启。
        // 故意**不回复**: 这是单向控制消息, 回复会让对端的邮箱里多出一条无人认领的消息。
        if tag == ECHO_QUIT_TAG {
            println("echo: quit requested, exiting");
            sys_exit();
        }
        // 回复当前调用者 (回复目标由内核在 recv 时记录)。
        if sys_reply(tag + 1) != 1 {
            println("echo: reply FAILED");
        }
    }
}
