//! 域 6 — fat32_srv: 独立 ELF, 入口由 libmorion 的 `_start` 提供。

#![no_std]
#![no_main]

#[no_mangle]
pub extern "C" fn morion_main(domain_id: u64) {
    morion_srv::announce("fat32_srv", domain_id);
    morion_srv::fat32_srv::run();
}
