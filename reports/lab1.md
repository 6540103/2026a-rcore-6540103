# 实验一：多道程序与分时多任务 —— `sys_trace` 系统调用

## 一、实验目标

在 rCore ch3（多道程序与分时多任务）的基础上，实现 `sys_trace` 系统调用（编号 410），使应用能够：

1. **读**任意地址的一个字节；
2. **写**任意地址的一个字节；
3. **查询**当前任务对某个系统调用的调用次数。

对应的测试程序是 `user/src/bin/ch3_trace.rs`。

## 二、实验环境

| 项 | 版本 |
|---|---|
| 主机 | Ubuntu 26.04 LTS / x86_64 |
| Rust | `nightly-2024-05-02`（rustc 1.80.0-nightly） |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| 组件 | rust-src、llvm-tools-preview、rustfmt、clippy |
| QEMU | 10.2.1（`qemu-system-riscv64`） |
| 固件 | 仓库自带 `bootloader/rustsbi-qemu.bin` |

## 三、设计思路

### 3.1 为什么计数要"每任务一份"

测试里第一条断言就是 `assert_eq!(0, count_syscall(SYSCALL_WRITE))`。
如果用一个全局计数器，前面跑过的任务（`power_*`、`yield*` 等）做过的 `write` 会残留下来，这个断言立刻失败。
因此计数器必须挂在任务控制块上，随任务切换而切换。

### 3.2 为什么计数要"先加再分发"

测试注释明确写着：

> 注意这次 `sys_trace` 调用本身也计入

并且 `assert_eq!(2, count_syscall(SYSCALL_TRACE))` —— 在这次调用执行**之前**只发生过 1 次 trace，
要让它读到 2，只能在**分发之前**就把本次调用记上。
所以计数点放在 `syscall()` 的最开头，而不是放在各个 `sys_*` 函数里。

### 3.3 三个请求的语义

`user/src/lib.rs` 里 `TraceRequest` 的定义顺序是 `Read, Write, Syscall`，对应 0 / 1 / 2。
`trace_read` 的封装是 `-1 => None, data => Some(data as u8)`，所以"读"要返回字节本身，"失败"用 -1 表示。

## 四、实现

### 4.1 新增常量（`os/src/config.rs`）

```rust
/// the max number of distinct syscalls (used by `sys_trace`)
pub const MAX_SYSCALL_NUM: usize = 512;
```

### 4.2 任务控制块加计数字段（`os/src/task/task.rs`）

```rust
pub struct TaskControlBlock {
    pub task_status: TaskStatus,
    pub task_cx: TaskContext,
    /// Per-task syscall counters, used by `sys_trace`
    pub syscall_times: [u32; MAX_SYSCALL_NUM],
}
```

`TaskControlBlock` 是 `Copy` 的，`[u32; 512]` 也是 `Copy`，因此原有的
`[TaskControlBlock { .. }; MAX_APP_NUM]` 数组初始化方式不用改结构。

### 4.3 任务管理器提供两个访问器（`os/src/task/mod.rs`）

```rust
fn incr_syscall_count(&self, syscall_id: usize) {
    let mut inner = self.inner.exclusive_access();
    let current = inner.current_task;
    if syscall_id < MAX_SYSCALL_NUM {
        inner.tasks[current].syscall_times[syscall_id] += 1;
    }
}

fn syscall_count(&self, syscall_id: usize) -> u32 {
    let inner = self.inner.exclusive_access();
    let current = inner.current_task;
    if syscall_id < MAX_SYSCALL_NUM {
        inner.tasks[current].syscall_times[syscall_id]
    } else {
        0
    }
}
```

这两个方法通过 `current_task` 取到**当前任务**，从而做到每任务独立计数。
模块级再包一层 `pub fn incr_syscall_count(..)` / `pub fn syscall_count(..)` 给 syscall 层调用。

### 4.4 计数点（`os/src/syscall/mod.rs`）

```rust
pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    // 先计数再分发：这样 `sys_trace` 查到的次数包含它自己这次调用
    crate::task::incr_syscall_count(syscall_id);
    match syscall_id { ... }
}
```

### 4.5 `sys_trace` 本体（`os/src/syscall/process.rs`）

```rust
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        // 读一个字节
        0 => unsafe { core::ptr::read_volatile(id as *const u8) as isize },
        // 写一个字节
        1 => {
            unsafe { core::ptr::write_volatile(id as *mut u8, data as u8) };
            0
        }
        // 查询当前任务对 syscall `id` 的调用次数
        2 => crate::task::syscall_count(id) as isize,
        _ => -1,
    }
}
```

读写用 `read_volatile` / `write_volatile`：这两个地址来自用户程序，
编译器不能假设"这里没有别人改"，必须真的每次访存。

## 五、测试结果

```
get_time OK! 121
Test power_3 OK!
Test power_5 OK!
Test power_7 OK!
Test write A OK!
Test write B OK!
Test write C OK!
current time_msec = ...
time_msec = ... after sleeping ... ticks, delta = ...ms!
Test sleep1 passed!
Test trace OK!
Test sleep OK!
[kernel] Panicked at src/task/mod.rs:156 All applications completed!
```

最后一句 `Panicked ... All applications completed!` 是 ch3 的**正常结束方式**：
所有任务都退出后，`run_next_task` 找不到可运行任务，主动 panic 表示全部完成。

`Test trace OK!` 说明 `ch3_trace` 的全部断言通过，包括：

- 不同系统调用的计数彼此独立；
- 计数随任务切换而不串味；
- `trace_read` / `trace_write` 能真实读写用户地址；
- 读代码段地址返回的不是 -1（即 `Some(_)`）。

## 六、遇到的问题与思考

1. **最初想把计数器做成全局 `static`**。写到一半发现 `assert_eq!(0, count_syscall(SYSCALL_WRITE))` 会失败：
   ch3 是静态加载多个 app 依次运行，全局计数会被前面的 app 污染。改成每任务一份才对。

2. **计数时机**：一开始放在 `sys_trace` 内部加，结果 `count_syscall(SYSCALL_TRACE)` 永远比期望少 1。
   放到 `syscall()` 分发前就对了。

3. **QEMU 版本**：课程要求 8.2.x，本地只有 10.2.1。实测内核启动、任务切换、全部测试均正常，
   说明这一章的代码没有依赖 QEMU 的版本细节。如果后续章节出现异常行为，再回头考虑版本因素。
