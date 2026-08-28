//! 抢占式 + 协作式调度器 (微内核核心原语 · 第 2 小步)
//!
//! 在第 1 小步 (任务 + 上下文切换 + 时钟抢占) 基础上完善:
//!   - 任务状态: `Ready` / `Running` / `Sleeping` / `Blocked`
//!   - 时钟 tick 计数, 驱动定时唤醒
//!   - 主动让出 `yield_now`, 定时睡眠 `sleep(ms)`
//!
//! 第 3 小步: 任务归属于保护域 (Domain), 切换任务时若域不同则切换 CR3。
//! 第 4 小步: IPC 阻塞支持 — `Blocked` 状态 + `block_current`/`wake_one`。
//!
//! 抢占由时钟中断 (IRQ0, 100 Hz) 驱动, 每 10ms 一次;
//! 协作式让出/睡眠由任务在运行中主动调用。

pub mod context;

use alloc::boxed::Box;
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::PhysFrame;
use x86_64::PhysAddr;

use context::{switch, TaskContext};

/// 最大任务数 (固定大小任务表)。
const MAX_TASKS: usize = 8;
/// 每任务内核栈大小 (32 KiB, 足以容纳中断帧 + 若干层函数调用)。
const STACK_SIZE: usize = 4096 * 8;

/// 任务状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Ready,
    Running,
    Sleeping,
    Blocked,
}

/// 内核任务控制块 (TCB)。
pub struct Task {
    pub id: u64,
    pub state: TaskState,
    pub ctx: TaskContext,
    /// 所属域页表根 (PML4) 物理地址, 切换任务时若与当前不同则切换 CR3。
    pub cr3: u64,
    /// 所属域 id (用于 IPC 消息路由)。
    pub domain: u64,
    /// 睡眠唤醒的 tick 时刻 (仅 `Sleeping` 状态有效)。
    sleep_until: u64,
    /// 阻塞等待的域 id (仅 `Blocked` 状态有效)。
    wait_on: u64,
    /// 持有任务栈的所有权, 防止其被释放 (栈地址记录在 `ctx.rsp` 中)。
    _stack: Box<[u8]>,
}

/// 调度器: 固定大小任务表 + 当前任务索引 + 时钟 tick 计数。
pub struct Scheduler {
    tasks: [Option<Task>; MAX_TASKS],
    current: usize,
    /// 自启动以来累计的时钟 tick 数 (100 Hz → 每 tick 10ms)。
    ticks: u64,
}

/// 毫秒 → tick 数 (100 Hz → 每 tick 10ms), 至少 1 tick。
fn ms_to_ticks(ms: u64) -> u64 {
    ((ms + 9) / 10).max(1)
}

/// 加载页表根 (CR3), 保留原 CR3 标志位。
fn load_cr3(pml4: u64) {
    let (_, flags) = Cr3::read();
    unsafe {
        Cr3::write(PhysFrame::containing_address(PhysAddr::new(pml4)), flags);
    }
}

impl Scheduler {
    fn new() -> Self {
        Self {
            tasks: core::array::from_fn(|_| None),
            current: 0,
            ticks: 0,
        }
    }

    /// 创建一个新任务 (归属指定域), 返回其任务表索引。
    fn spawn(&mut self, entry: extern "C" fn(), domain: u64) -> usize {
        let slot = self
            .tasks
            .iter()
            .position(|t| t.is_none())
            .expect("scheduler: no free task slot");

        let mut stack = alloc::vec![0u8; STACK_SIZE].into_boxed_slice();
        let ctx = unsafe { TaskContext::from_entry(entry, &mut stack) };
        let cr3 = crate::domain::pml4_of(domain);

        self.tasks[slot] = Some(Task {
            id: slot as u64,
            state: TaskState::Ready,
            ctx,
            cr3,
            domain,
            sleep_until: 0,
            wait_on: 0,
            _stack: stack,
        });
        slot
    }

    /// 从 `after` 之后 (环绕) 寻找下一个就绪任务。
    fn next_ready(&self, after: usize) -> Option<usize> {
        for i in 1..=MAX_TASKS {
            let idx = (after + i) % MAX_TASKS;
            if let Some(t) = &self.tasks[idx] {
                if t.state == TaskState::Ready {
                    return Some(idx);
                }
            }
        }
        None
    }
}

/// 全局调度器 (启动后由 `init` 填充)。
static SCHEDULER: Mutex<Option<Scheduler>> = Mutex::new(None);

/// 初始化调度器 (须在内核堆就绪后调用)。
pub fn init() {
    *SCHEDULER.lock() = Some(Scheduler::new());
}

/// 创建一个内核任务 (归属指定域)。
pub fn spawn(entry: extern "C" fn(), domain: u64) {
    SCHEDULER
        .lock()
        .as_mut()
        .expect("scheduler not initialized")
        .spawn(entry, domain);
}

/// 将当前任务标记为 `current_state` 并切换到下一个就绪任务。
///
/// 调用前必须已关闭中断 (IF=0)。单核 + 关中断下, 锁的临界区原子。
/// 锁在 `switch` 前释放, 避免被切换到的任务重新获取时死锁。
fn schedule_next(current_state: TaskState) {
    unsafe {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("scheduler not initialized");

        let current = sched.current;
        if let Some(t) = sched.tasks[current].as_mut() {
            t.state = current_state;
        }
        let next = sched
            .next_ready(current)
            .expect("scheduler: no runnable task");

        // 切换到新任务的地址空间 (仅当域不同)。
        let cur_cr3 = sched.tasks[current].as_ref().unwrap().cr3;
        let next_cr3 = sched.tasks[next].as_ref().unwrap().cr3;
        if cur_cr3 != next_cr3 {
            load_cr3(next_cr3);
        }

        let old = &mut sched.tasks[current].as_mut().unwrap().ctx as *mut TaskContext;
        let new = &sched.tasks[next].as_ref().unwrap().ctx as *const TaskContext;

        sched.current = next;
        sched.tasks[next].as_mut().unwrap().state = TaskState::Running;

        drop(guard);
        switch(old, new);
    }
}

/// 时钟中断处理: 递增 tick、唤醒到期任务、抢占调度。
///
/// 由 IRQ0 调用 (IF=0)。被抢占任务的执行流之后经中断帧 iret 恢复中断。
pub fn tick() {
    {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("scheduler not initialized");

        sched.ticks += 1;
        // 唤醒所有到期的睡眠任务。
        for task in sched.tasks.iter_mut().flatten() {
            if task.state == TaskState::Sleeping && sched.ticks >= task.sleep_until {
                task.state = TaskState::Ready;
            }
        }
    }
    schedule_next(TaskState::Ready);
}

/// 主动让出 CPU: 当前任务回到就绪态, 切换到下一个就绪任务。
///
/// 协作式调用 (IF=1)。关中断保护锁临界区; 被再次调度回来时恢复中断。
pub fn yield_now() {
    x86_64::instructions::interrupts::disable();
    schedule_next(TaskState::Ready);
    // 被再次调度回来后恢复中断 (时钟路径则经 iret 恢复)。
    x86_64::instructions::interrupts::enable();
}

/// 睡眠 `ms` 毫秒: 当前任务进入睡眠态, 由时钟 tick 在到期时唤醒。
pub fn sleep(ms: u64) {
    x86_64::instructions::interrupts::disable();
    {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("scheduler not initialized");
        let wake = sched.ticks + ms_to_ticks(ms);
        sched.tasks[sched.current]
            .as_mut()
            .expect("scheduler: no current task")
            .sleep_until = wake;
    }
    schedule_next(TaskState::Sleeping);
    // 被再次调度回来后恢复中断。
    x86_64::instructions::interrupts::enable();
}

/// 返回当前任务的域 id (须在关中断下调用)。
pub fn current_domain() -> u64 {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("scheduler not initialized");
    sched.tasks[sched.current]
        .as_ref()
        .expect("scheduler: no current task")
        .domain
}

/// 阻塞当前任务: 标记为 `Blocked`, 记录等待的域, 切换到下一就绪任务。
///
/// 由 `ipc::receive` 在邮箱为空时调用 (IF=0)。被 `send` 唤醒后返回。
pub fn block_current(on_domain: u64) {
    {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("scheduler not initialized");
        sched.tasks[sched.current]
            .as_mut()
            .expect("scheduler: no current task")
            .wait_on = on_domain;
    }
    schedule_next(TaskState::Blocked);
}

/// 唤醒一个阻塞在指定域邮箱上的任务 (若存在), 置为就绪。
///
/// 由 `ipc::send` 在消息入队后调用 (IF=0)。
pub fn wake_one(domain: u64) {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    for task in sched.tasks.iter_mut().flatten() {
        if task.state == TaskState::Blocked && task.wait_on == domain {
            task.state = TaskState::Ready;
            break;
        }
    }
}

/// 启动调度器: 从当前 (main) 执行流切换到首个任务, 永不返回。
pub fn run() -> ! {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");

    let first = sched
        .next_ready(MAX_TASKS - 1)
        .expect("scheduler: no task to run");

    // 切换到首个任务的地址空间。
    let first_cr3 = sched.tasks[first].as_ref().unwrap().cr3;
    load_cr3(first_cr3);

    let mut idle = TaskContext::empty();
    let old = &mut idle as *mut TaskContext;
    let new = &sched.tasks[first].as_ref().unwrap().ctx as *const TaskContext;

    sched.current = first;
    sched.tasks[first].as_mut().unwrap().state = TaskState::Running;

    drop(guard);
    unsafe { switch(old, new) };

    unreachable!("scheduler::run returned unexpectedly");
}
