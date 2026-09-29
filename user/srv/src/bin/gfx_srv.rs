//! 域 15 — gfx_srv (图形服务): 独立 ELF, 入口由 libmorion 的 `_start` 提供。

#![no_std]
#![no_main]

#[no_mangle]
pub extern "C" fn morion_main(domain_id: u64) {
    morion_srv::announce("gfx_srv", domain_id);
    morion_srv::gfx_srv::run();
}
