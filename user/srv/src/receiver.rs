use crate::common::*;
use morion::syscall::*;

/// 域 1 — 接收者: 经 IPC 收到通知后, 直接从共享页读取数据, 再解除映射;
/// 最后核验 sender 交过来的句柄与能力。
pub fn run() {
    // 反面对照 (关键): receiver 启动时**持有零个能力**, 此刻调用 echo 必须失败。
    // 后面同样的调用在拿到委派来的 SendTo(3) 之后必须成功 —— 一负一正,
    // 才能证明「能力确实是靠这次传递获得的」而不是本来就有的。
    if sys_call(3, 0x1234) != u64::MAX {
        println("receiver: call echo WITHOUT capability unexpectedly OK FAILED");
    }

    // 等 sender 通知共享页就绪。
    let tag = sys_recv();
    if tag != 777 {
        println("receiver: unexpected first tag FAILED");
    }

    // 直接读共享页 (零拷贝, 数据未经 IPC 传递)。
    let page = SHARED_PAGE;
    let _ = unsafe { core::slice::from_raw_parts(page as *const u8, 12) };

    // 解除映射: 引用计数 1 -> 0, 真正释放帧。
    if sys_unmap(page) != 1 {
        println("receiver: unmap FAILED");
    }

    // ---- 能力随 IPC 传递 (M-C1): 核验收到的句柄与能力 ----
    let tag = sys_recv();
    if tag & !0xFF != CAP_HANDLE_TAG {
        println("receiver: capability transfer notification FAILED");
        return;
    }
    let h = tag & 0xFF;
    // 收到的句柄必须能查出 sender 当初放进去的那个不透明对象。
    if sys_cap_lookup(h) != CAP_TOKEN {
        println("receiver: transferred handle lookup FAILED");
    }
    // 用委派来的 SendTo(3) 直接调 echo —— 这正是本函数开头做不到的那件事。
    if sys_call(3, 0xCAFE) != 0xCAFF {
        println("receiver: call echo via delegated capability FAILED");
    }
    // 负例: receiver 依然没有 SendTo(2), 不能往 pager 塞能力。
    if sys_cap_send(2, CAP_KIND_SEND_TO, 3) != 0 {
        println("receiver: cap_send to unreachable domain FAILED");
    }

    // 成功标记: 本段其余步骤都「静默通过」, 但「静默」无法区分「跑过并通过」与
    // 「根本没跑到」(例如阻塞在上面的 recv)。故这一行**必须打印** —— 它是本原语
    // 被真正执行过的唯一正面证据。上面 8 条负例/正例中任意一条失败都会另打 FAILED。
    println("receiver: capability passing OK (handle moved + SendTo(3) delegated)");
}
