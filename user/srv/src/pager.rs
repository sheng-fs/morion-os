use crate::common::*;
use morion::syscall::*;

/// 域 2 — 分页器: 经通用 IPC 阻塞接收缺页消息, 映射匿名零帧并回复。
pub fn run() {
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        // 从 payload 前 24 字节解出缺页信息。
        let info: PageFaultInfo =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const PageFaultInfo) };

        if sys_map_anon(info.fault_domain, info.fault_addr) != 1 {
            println("pager: map_anon FAILED");
        }

        sys_page_fault_reply();
    }
}
