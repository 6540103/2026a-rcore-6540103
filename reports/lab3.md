# 实验三：进程管理 —— `spawn` 与进程优先级

## 一、实验目标

ch5 把"任务"升级成了"进程"：每个进程有独立 pid、地址空间、父子关系，并支持 fork / exec / waitpid。
本章在此基础上完成：

1. **`sys_spawn`**：按名字创建一个新进程（直接加载 ELF，不复制父进程地址空间）；
2. **`sys_set_priority`**：设置当前进程优先级，并把调度器改成按优先级分配 CPU；
3. 顺带把 ch4 的 `sys_get_time` / `sys_mmap` / `sys_munmap` 在进程模型下补齐（ch5 不再有 `sys_trace`）。

## 二、实验环境

| 项 | 版本 |
|---|---|
| Rust | `nightly-2024-05-02` |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| QEMU | 10.2.1 |
| 检查器 | `rCore-Tutorial-Checker` |

## 三、设计思路

### 3.1 spawn 不是 fork + exec

fork 会**复制父进程的整个地址空间**，exec 再把它**替换**掉——对一个全新进程来说，前一步完全浪费。
spawn 直接按名字取 ELF 数据、构造新的 `TaskControlBlock`，省掉复制。

```rust
let task = current_task().unwrap();
let path = translated_str(token, path);          // 从用户态拷路径
let elf = get_app_data_by_name(path.as_str())?;  // 按名字取 ELF
let child = Arc::new(TaskControlBlock::new(elf)); // 全新地址空间
```

父子关系必须手动建立（`TaskControlBlock::new` 出来的进程 `parent` 是 `None`、
父进程的 `children` 里也没有它），否则 `wait` / `waitpid` 收不到这个子进程：

```rust
child.inner_exclusive_access().parent = Some(Arc::downgrade(&task));
task.inner_exclusive_access().children.push(child.clone());
```

两次加锁是**分开的作用域**，避免同时持有父子两把锁产生死锁风险。

### 3.2 优先级：stride 调度

原来的 `TaskManager` 是一个 `VecDeque`，纯 FIFO，优先级无从谈起。改成 **stride 调度**：

- 每个进程有 `priority`、`stride`、`pass`；
- `pass = BIG_STRIDE / priority`（BIG_STRIDE = 1_000_000_000）；
- 每次调度**选 stride 最小的**，选中后 `stride += pass`。

直观理解：优先级越高 → pass 越小 → stride 涨得越慢 → 越容易被再次选中，占用的 CPU 比例就是
`priority / 所有就绪进程 priority 之和`。

### 3.3 为什么优先级必须 ≥ 2

`pass = BIG_STRIDE / priority`。若 `priority = 1`，pass 就等于 BIG_STRIDE，
一步就把 stride 推到很大，调度会明显失衡；`priority = 0` 更是除零。
所以 `sys_set_priority` 对 `< 2` 一律返回 -1，这与测试完全一致：

```rust
assert_eq!(set_priority(10), 10);            // 成功，返回设置后的值
assert_eq!(set_priority(isize::MAX), isize::MAX);
assert_eq!(set_priority(0), -1);
assert_eq!(set_priority(1), -1);
assert_eq!(set_priority(-10), -1);
```

注意返回值是**设置后的优先级**，不是旧值——测试第二行要求 `set_priority(isize::MAX)`
返回 `isize::MAX`，如果返回旧值（10）就对不上了。

## 四、关键代码

### 4.1 stride 调度器（`os/src/task/manager.rs`）

```rust
pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
    let mut best: Option<(usize, usize)> = None; // (index, stride)
    for (i, task) in self.ready_queue.iter().enumerate() {
        let stride = task.inner_exclusive_access().stride;
        match best {
            None => best = Some((i, stride)),
            Some((_, bs)) if stride < bs => best = Some((i, stride)),
            _ => {}
        }
    }
    let (idx, _) = best?;
    let task = self.ready_queue.remove(idx);
    {
        let mut inner = task.inner_exclusive_access();
        let pass = inner.pass;
        inner.stride += pass;
    }
    Some(task)
}
```

### 4.2 set_priority

```rust
pub fn sys_set_priority(prio: isize) -> isize {
    if prio < 2 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.priority = prio as usize;
    inner.pass = crate::task::BIG_STRIDE / (prio as usize);
    prio
}
```

### 4.3 spawn

```rust
pub fn sys_spawn(path: *const u8) -> isize {
    let task = current_task().unwrap();
    let path = translated_str(task.get_user_token(), path);
    let Some(elf) = get_app_data_by_name(path.as_str()) else { return -1 };
    let child = Arc::new(TaskControlBlock::new(elf));
    let pid = child.getpid();
    { child.inner_exclusive_access().parent = Some(Arc::downgrade(&task)); }
    { task.inner_exclusive_access().children.push(child.clone()); }
    add_task(child);
    pid as isize
}
```

## 五、测试结果

```
[PASS] Test spawn0 OK!        [PASS] Test wait OK!
[PASS] Test waitpid OK!       [PASS] Test set_priority OK!
[PASS] get_time OK!           [PASS] Test sleep OK!       [PASS] Test sleep1 passed!
[PASS] Test 04_1 OK!          [PASS] Test 04_4 test OK!
[PASS] Test 04_5 ummap OK!    [PASS] Test 04_6 ummap2 OK!
[PASS] not found <Test sbrk failed!>
[PASS] not found <FAIL: T.T>

Test passed: 15/15
```

`spawn0` 连续创建 40 个子进程再逐个 `wait` 回收，能通过说明：
pid 分配、内核栈分配、就绪队列、父子链表、退出回收这一整套在压力下没有泄漏或错乱。

## 六、遇到的问题与思考

1. **`missing_docs` 连锁反应**。把 `PageTable` / `PTEFlags` 从内部 `use` 改成 `pub use` 之后，
   `PTEFlags` 的各个常量变成对外可见，触发了 `#![deny(missing_docs)]`。
   解决办法是给 8 个常量都补上文档注释，而不是关掉 lint。

2. **spawn 与 fork 的取舍**。一开始想直接用 `fork()` 再 `exec()`，但 fork 需要复制整个
   地址空间（含页表），代价高且与"新建一个进程"的语义不符。直接用 `TaskControlBlock::new`
   更贴近 spawn 的本意。

3. **调度器改造的兼容性**。把 `VecDeque` 换成 `Vec` 并按 stride 选最小，接口
   （`add` / `fetch`）没变，所以 `processor.rs` 一行都不用改——这得益于模板里
   调度器与执行流是分离的。
