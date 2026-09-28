use crate::common::*;
use morion::syscall::*;

/// 域 0 — 发送者: 持有 SendTo(1) + MapInto(1) + SendTo(3) 能力, **不持有** SendTo(2)。
/// 成功路径保持静默 (避免刷屏), 仅失败时打印。
pub fn run() {
    // 共享内存演示: 申请一页 → 写入 → 共享给域 1 → IPC 通知。
    let page = SHARED_PAGE;
    if sys_alloc_page(page) != 1 {
        println("sender: alloc_page FAILED");
        return;
    }
    let msg: &str = "HELLO SHARED";
    unsafe {
        core::ptr::copy_nonoverlapping(msg.as_ptr(), page as *mut u8, msg.len());
    }
    if sys_share_page(page, 1) != 1 {
        println("sender: share_page DENIED");
    }
    // IPC 通知 receiver 读取 (复用现有 SendTo 能力)。
    sys_send(1, 777);

    // 解除本域映射: 引用计数 2 -> 1, 帧不释放 (receiver 仍持有)。
    if sys_unmap(page) != 1 {
        println("sender: unmap FAILED");
    }

    // 按需分页演示: 访问一个从未映射的用户空间地址触发缺页 (由 pager 域补零帧)。
    // 必须是 canonical 用户空间地址 (P4[1])。
    let fault_addr = 0x81_0000_0000u64;
    let _ = unsafe { core::ptr::read_volatile(fault_addr as *const u64) };

    // ---- 能力随 IPC 传递 (M-C1) ----
    // 负例一: 委派自己**没有**的能力必须被拒 —— sender 持有 SendTo(1)/SendTo(3),
    // 但从不持有 SendTo(2)。没有的能力给不出去 (无放大), 这是能力模型的根。
    if sys_cap_send(1, CAP_KIND_SEND_TO, 2) != 0 {
        println("sender: delegate unheld SendTo(2) NOT denied FAILED");
    }
    // 负例二: 连「往不可达域塞能力」都不允许 —— 没有 SendTo(2), 目标域校验先失败。
    if sys_cap_send(2, CAP_KIND_SEND_TO, 3) != 0 {
        println("sender: cap_send to unreachable domain FAILED");
    }
    // 负例三: 参数非法的能力 (Irq 的 arg 超出 u8) 必须被解码层拒绝。
    if sys_cap_send(1, CAP_KIND_IRQ, 0x1234) != 0 {
        println("sender: bad cap arg NOT rejected FAILED");
    }

    // 句柄移交: 为一个不透明对象签发句柄, 再把它**移入** receiver 域。
    let h = sys_cap_issue(CAP_TOKEN);
    if h == u64::MAX {
        println("sender: cap_issue FAILED");
        return;
    }
    let moved = sys_handle_send(1, h);
    if moved == u64::MAX {
        println("sender: handle_send FAILED");
        return;
    }
    // 移动语义: 移走之后**本域的句柄必须立即失效** (能力同一时刻只属于一个域)。
    if sys_cap_lookup(h) != u64::MAX {
        println("sender: handle still valid after move FAILED");
    }

    // 能力委派: 把 SendTo(3) 复制给 receiver —— 它因此获得与 echo 通信的能力,
    // 而启动期没有任何静态授权让它能这么做 (receiver 的能力槽是空的)。
    if sys_cap_send(1, CAP_KIND_SEND_TO, 3) != 1 {
        println("sender: delegate SendTo(3) FAILED");
    }
    // 同一项能力重复委派必须幂等 (不再占新槽): 连做两次都应成功。
    if sys_cap_send(1, CAP_KIND_MAP_INTO, 1) != 1 || sys_cap_send(1, CAP_KIND_MAP_INTO, 1) != 1 {
        println("sender: duplicate delegation NOT idempotent FAILED");
    }

    // 通知 receiver 去核验 (句柄索引编进 tag 低 8 位)。
    // 必须检查返回值: 这条通知若没送出去, receiver 会**永久阻塞在第二次 recv**
    // 而不报任何错 —— 是本次自测里唯一的静默挂死路径。
    if sys_send(1, CAP_HANDLE_TAG | moved) != 1 {
        println("sender: capability transfer notification FAILED");
    }

    // 同步 IPC call/reply 演示: 调用 echo 服务 (域 3)。
    let _ = sys_call(3, 0xABCD);
}
