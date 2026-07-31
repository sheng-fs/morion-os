; ============================================================
; Morion OS 引导桩 (Boot Stub) — x86_64 UEFI 汇编入口点
; ============================================================
; 这是最小可信链的起点：UEFI 固件 → 本 stub → Rust 引导器
;
; 设计原则：
;   1. 极简 — 仅完成最少的寄存器初始化后跳转到 Rust
;   2. 可审计 — 代码量极小，易于安全审查和形式化验证
;   3. 签名验证入口 — 作为 Secure Boot 签名的最小单元
;   4. 无状态破坏 — 完整保留 UEFI 固件传递的 Handoff 信息
;
; UEFI 调用约定 (x86_64)：
;   - 入参: RCX = ImageHandle, RDX = *EFI_SYSTEM_TABLE
;   - 返回: RAX = EFI_STATUS
;   - 调用者清理栈
;   - 16 字节栈对齐 (外加 8 字节返回地址 = 进入时 RSP % 16 == 8)
;   - 影子空间: 必须在栈上为被调用者保留 32 字节
;   - 非易失寄存器: RBX, RBP, RDI, RSI, R12-R15, XMM6-XMM15

BITS 64
DEFAULT REL

; PE/COFF 区段定义
section .text  progbits  align=16

; ============================================================
; 导出入口符号 (替代 Rust 的 efi_main)
; 当使用汇编入口时，Cargo config 中设置 /ENTRY:boot_stub_entry
; ============================================================
global boot_stub_entry
export boot_stub_entry

boot_stub_entry:
    ; --------------------------------------------------
    ; 阶段 0: 保存固件传递的关键信息
    ; --------------------------------------------------
    ; UEFI 固件调用我们时：
    ;   RCX = EFI_HANDLE ImageHandle (当前加载的 EFI 应用)
    ;   RDX = *EFI_SYSTEM_TABLE (固件系统表)
    ;
    ; 我们将这些作为参数传递给 Rust 的 efi_main
    push    rbx                     ; 保存非易失寄存器
    push    rbp
    push    rdi
    push    rsi
    push    r12
    push    r13
    push    r14
    push    r15
    sub     rsp, 0x20              ; 预留调用影子空间 (32 字节)

    ; --------------------------------------------------
    ; 阶段 1: 栈安全检查
    ; --------------------------------------------------
    ; 确保栈指针在合理范围 (排除固件传递错误的情况)
    ; 如果栈顶为 NULL 则直接返回错误
    xor     eax, eax
    test    rsp, rsp
    jz      .fatal_error

    ; --------------------------------------------------
    ; 阶段 2: 可选 — 验证自身签名 (早期测量)
    ; --------------------------------------------------
    ; 如果启用了 Secure Boot，固件已经验证了本映像的签名。
    ; 此处可以进行二次自检 (测量自身哈希并扩展 TPM PCR)。
    ; 这作为一个独立的编译选项，默认跳过。

%ifdef SECURE_BOOT_SELF_CHECK
    ; TODO: 调用 TPM 2.0 PCR Extend 原语
    ; 将自身 .text 段哈希扩展到 PCR[4] (Boot Manager Code)
    call    measure_self
%endif

    ; --------------------------------------------------
    ; 阶段 3: 准备 Rust 入口调用
    ; --------------------------------------------------
    ; 确保 RCX, RDX 参数正确传递给 efi_main
    ; 如果固件传递的值可疑，可用已知值覆盖进行安全测试
    mov     rcx, rcx                ; ImageHandle (原样保留)
    mov     rdx, rdx                ; SystemTable  (原样保留)

    ; 确保方向标志清零 (UEFI 规范要求，但防御性设置)
    cld

    ; --------------------------------------------------
    ; 阶段 4: 跳转到 Rust 主引导逻辑
    ; --------------------------------------------------
    extern  efi_main
    call    efi_main

    ; --------------------------------------------------
    ; 阶段 5: 返回给固件
    ; --------------------------------------------------
    ; efi_main 返回 EFI_STATUS (u64) 在 RAX 中
    add     rsp, 0x20              ; 回收影子空间
    pop     r15
    pop     r14
    pop     r13
    pop     r12
    pop     rsi
    pop     rdi
    pop     rbp
    pop     rbx
    ret

    ; --------------------------------------------------
    ; 致命错误处理 — 停机
    ; --------------------------------------------------
.fatal_error:
    ; 最小化错误处理：直接返回错误码
    ; EFI_LOAD_ERROR = 1 (bit 63 set for error)
    mov     rax, 0x80000000_00000001
    add     rsp, 0x20
    pop     r15
    pop     r14
    pop     r13
    pop     r12
    pop     rsi
    pop     rdi
    pop     rbp
    pop     rbx
    ret

; ============================================================
; Secure Boot 自检测量函数 (可选)
; ============================================================
%ifdef SECURE_BOOT_SELF_CHECK
measure_self:
    ; 使用 SHA-256 对当前映像的 .text 段哈希
    ; 然后调用 TPM 2.0 EFI 协议扩展 PCR[4]
    ;
    ; 参数: 无 (隐式使用当前映像基址和大小)
    ; 破坏: RAX, RCX, RDX, R8, R9, R10, R11 (调用者保存)
    ;
    ; EFI_TCG2_PROTOCOL.HashLogExtendEvent 调用：
    ;   - 通过 SystemTable->BootServices->LocateProtocol 查找 TCG2
    ;   - PCRIndex = 4 (Boot Manager Code)
    ;   - 计算 .text 段 SHA-256
    ;   - 调用 HashLogExtendEvent
    push    rbp
    mov     rbp, rsp
    sub     rsp, 0x40              ; 局部变量空间 + 影子空间

    ; 当前实现: 暂为空操作
    ; 完整实现需要在 Rust 侧配合 EFI_TCG2_PROTOCOL

    leave
    ret
%endif

; ============================================================
; 只读数据段
; ============================================================
section .rdata  progbits  align=16

; 引导桩版本签名 (用于审计和调试)
boot_stub_signature:
    db  'M','o','r','i','o','n','B','o','o','t',0
boot_stub_version:
    dd  0x00000100                ; 版本 0.1.0
boot_stub_magic:
    dd  0x4F52424D                ; "MBRM" — Morion Boot R Stub Magic
