//! magic 自愈看门狗（共享任务）
//!
//! AP 启动链（BootROM/SPL/U-Boot 曾 cacheable 写 SRAM）的迟到脏行写回
//! 可能在 RP init 之后清掉某通道 magic（板上实锤：ch1/ch2.magic 在
//! t≈7~24s 被清一次，AP 侧另以 boot 时 invalidate 作废这些脏行）。
//! 没有看门狗的 bin（如旧版 ipc_demo）一旦被清就永久 invalid，AP 侧
//! `is_valid()` 永假——AP 程序 5s 超时 panic。
//!
//! 本任务每秒检查三通道 magic，丢失即幂等重跑 `intercom::init()` 恢复
//! 全部头。注意 re-init 会顺带清空各通道未读消息（magic 丢失时内容本已
//! 不可信，可接受）。
//!
//! 共享窗 boot 期 init 的职责已在 AP 内核 rt_shm probe 期（时机确定性
//! 排在 SPL / U-Boot memset / bootm 缓存 flush 之后），本看门狗是**纵深
//! 防御**：覆盖 init 之后的残余破坏（AP 运行中重启经 U-Boot 再 memset、
//! bootdelay 漂移等病态场景）。
//!
//! 所有使用 intercom 的 bin（ipc_demo / shm_ping）都应 spawn 本任务。

use fugit::ExtU64;
use ov_channels::SharedMemory;

/// 共享窗写入门控：SPL 执行期（头 ~1s）与 U-Boot `k3_clear_sram()`（~1.5s）
/// 都会破坏窗口内容，任何窗口写入必须等到上线 3s 之后。
pub const WRITE_GATE_MS: u64 = 3000;

/// magic 自愈看门狗 + 存活心跳。
///
/// 优先级要求：后台守护，须低于 IPC 服务任务（数值更大），不抢占消息处理。
#[executor::task]
pub async fn magic_watchdog() {
    futures::timer::after(WRITE_GATE_MS.millis()).await;

    // SAFETY: `SharedMemory::at` 返回指向 DT 保留区的 &'static 引用，跨 await
    // 持有安全（映射与设备同生命周期）；本任务只读 magic（is_valid）与调用
    // 幂等 init。
    let shm = unsafe { SharedMemory::<3>::at(ov_shm::shm::base()) };
    // 自愈上限 8 次：启动链写回是有限事件（板上实测仅 1 次），无上限会在
    // "窗口持续被外部破坏"的病态场景下无限 re-init 刷日志。
    let mut heals: u32 = 0;
    let mut secs: u64 = 0;
    loop {
        futures::timer::after(1000.millis()).await;
        secs += 1;

        // 不设"先见过 valid"前置：看门狗 t=4s 才首查（3s 门控 + 1s 周期），
        // 若迟到写回恰落在 3s init 与首查之间，前置条件会让自愈永不触发。
        // !is_valid 即幂等 re-init，heals 上限兜住病态循环。
        if !shm.is_valid() && heals < 8 {
            heals += 1;
            crate::intercom::note_magic_heal();
            log::info!(
                "[magic-wd] t={secs}s 三通道 magic 丢失（启动链迟到写回）→ 自愈 re-init（第 {heals} 次）"
            );
            crate::intercom::init();
        }

        if secs % 10 == 0 {
            log::info!("[magic-wd] alive t={secs}s");
        }
    }
}
