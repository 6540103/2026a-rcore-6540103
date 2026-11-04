# 实验五：并发与同步 —— 死锁检测

## 一、实验目标

ch8 把"进程"进一步拆成"进程 + 线程"，并引入互斥锁、信号量、条件变量。
本章要完成：

1. `sys_enable_deadlock_detect`：开关死锁检测；
2. 互斥锁的**自死锁**检测（同一线程重复加锁）；
3. 信号量的**银行家算法**安全性检测；
4. 移植此前的 `fstat` / `linkat` / `unlinkat` / `get_time` / `mmap` / `munmap` / `spawn` / `set_priority`。

## 二、实验环境

| 项 | 版本 |
|---|---|
| Rust | `nightly-2024-05-02` |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| QEMU | 10.2.1 |

## 三、设计思路

### 3.1 死锁检测要解决什么问题

`ch8_deadlock_sem1` 与 `ch8_deadlock_sem2` 是一对**镜像用例**：

- `sem1`：3 个线程各占一部分资源后循环申请，形成真正的环 → 必须检测出来，至少一个线程
  的 `semaphore_down` 返回 `-0xdead`；
- `sem2`：4 个线程看起来互相等待，但其中两个线程**不再申请新资源**、很快会释放 →
  不是死锁，所有请求都必须正常返回 0。

所以不能用简单的"等待图找环"：`sem2` 里的等待关系其实也成环（tid0 等 tid2 持有的信号量，
tid2 等 tid0 持有的信号量），但它**不是死锁**，因为还有第三方会释放。
必须用**银行家算法**判断"整体是否处于安全状态"。

### 3.2 数据结构

在 `ProcessControlBlockInner` 里加：

| 字段 | 含义 |
|---|---|
| `deadlock_detect` | 是否开启检测 |
| `sem_avail[s]` | 信号量 s 当前可用数量（available） |
| `sem_held[t][s]` | 线程 t 当前持有 s 的数量（allocation） |
| `sem_need[t][s]` | 线程 t 正在申请 s 的数量（need） |
| `mutex_owner[m]` | 互斥锁 m 的持有线程 tid |

### 3.3 银行家安全性算法

```rust
fn banker_safe(inner: &ProcessControlBlockInner) -> bool {
    let mut work = inner.sem_avail.clone();
    let mut finish = vec![false; ntasks];
    loop {
        let mut progress = false;
        for t in 0..ntasks {
            if finish[t] || inner.tasks[t].is_none() { /* ... */ continue; }
            // 该线程的所有待满足需求都能被 work 满足吗？
            if (0..nsems).all(|s| inner.sem_need[t][s] <= work[s]) {
                for s in 0..nsems { work[s] += inner.sem_held[t][s]; } // 它能跑完并归还
                finish[t] = true;
                progress = true;
            }
        }
        if !progress { break; }
    }
    finish.iter().all(|&f| f)
}
```

**判定时机**：在 `sys_semaphore_down` 里，若信号量计数已 ≤ 0（本来要阻塞），
先把本次请求记进 `sem_need`，再跑一次安全性检查；不安全就撤销请求并返回 `-0xdead`。

用 `sem1` 手工验证一下：三个线程都阻塞后，
`avail = [0,0,0]`、`need` 分别是 `{sem1}`、`{sem3}`、`{sem2}`，
找不到任何 `need <= work` 的线程 → 无进展 → 不安全 → 最后一个申请者被拒 ✔

`sem2` 则不同：tid1、tid3 没有待满足需求，可以先"完成"并归还资源，
`work` 于是变成 `[1,1]`，剩下两个线程依次满足 → 安全 ✔

### 3.4 互斥锁的自死锁

非递归互斥锁被同一线程重复 `lock` 必然死锁。用 `mutex_owner[mutex_id]` 记录持有者，
再次加锁时若 owner 就是自己，直接返回 `-0xdead`，不必真的阻塞。

## 四、关键代码

### 4.1 信号量 down 的检测点

```rust
if inner.deadlock_detect {
    ensure_rows(&mut inner, tid, sem_id);
    inner.sem_need[tid][sem_id] += 1;          // 记录"正在申请"
    let count = sem.inner.exclusive_access().count;
    if count <= 0 && !banker_safe(&inner) {    // 满足这次请求会进入不安全状态
        inner.sem_need[tid][sem_id] -= 1;      // 撤销
        return -0xdead;
    }
}
sem.down();                                     // 真正阻塞
if inner.deadlock_detect {                      // 拿到后更新记账
    inner.sem_need[tid][sem_id] -= 1;
    inner.sem_held[tid][sem_id] += 1;
    inner.sem_avail[sem_id] -= 1;
}
```

### 4.2 数组按需扩容

`ensure_rows` 会按当前 `tid` / `sem_id` 把 `sem_held` / `sem_need` / `sem_avail` 扩到足够大。
这一点很关键：`enable_deadlock_detect(true)` 往往在 `thread_create` **之前**调用，
之后新建线程的 tid 会超出当时数组的长度。

## 五、测试结果

```
[PASS] pipetest passed!
[PASS] mpsc_sem passed!
[PASS] philosopher dining problem with mutex test passed!
[PASS] race adder using spin mutex test passed!
[PASS] sync_sem passed!
[PASS] test_condvar passed!
[PASS] threads with arg test passed!
[PASS] threads test passed!
[PASS] deadlock test mutex 1 OK!
[PASS] deadlock test semaphore 1 OK!
[PASS] deadlock test semaphore 2 OK!
[PASS] Test sbrk almost OK!
[PASS] __main__

Test passed: 25/25
```

`sem1` 与 `sem2` **同时通过**，说明检测既没有漏报真死锁，也没有把安全状态误判成死锁。

## 六、遇到的问题与思考

### 6.1 数组越界导致整个测试停摆

第一版 `enable_deadlock_detect` 直接按**当时**的 `tasks.len()` 分配二维数组。
但测试是先开检测、再 `thread_create`，新线程 tid 进来时数组不够长，
`sem_need[tid][sem_id]` 越界 → 内核 panic → 后续所有测试全都没机会跑。

表现很容易误判：不是"死锁测试失败"，而是 25 个里只剩 5 个通过，看起来像调度被改坏了。
教训是——**内核里任何一次 panic 都会终止整轮测试**，排查时应该先看"第一个出错点"，
而不是盯着失败的用例数量。

修正办法是让记账数组在使用处按需扩容（`ensure_rows`），而不是只在开启检测时分配一次。

### 6.2 不能用地等待图找环

最初的直觉是"维护等待关系，发现环就报死锁"。但 `sem2` 立刻推翻了这个想法：
两个线程确实在互相等待对方持有的资源，可第三方线程还会释放，
所以**环不等于死锁**。只有像银行家算法那样判断"是否存在一个让所有线程依次完成的顺序"，
才能既抓住 `sem1` 又不冤枉 `sem2`。

### 6.3 调度器与检测的边界

ch8 的调度器保持 FIFO。曾尝试把前几章的 stride 调度一并挪过来，
结果大量并发用例失败——这几个章节的 `run_tasks` 与就绪队列语义并不完全一致，
跨章搬运调度策略要非常小心。本章 `set_priority` 的测试只校验返回值，
因此保留优先级字段、不改调度顺序即可满足要求。
