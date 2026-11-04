# 实验二：地址空间 —— 虚拟内存下的系统调用与 `mmap` / `munmap`

## 一、实验目标

ch4 引入了**每任务独立页表**：内核与用户不再共享地址空间，内核也**不能再直接解引用用户指针**。
本章要在这个前提下完成四件事：

1. 用虚拟内存重写 `sys_get_time`（要求能处理 `TimeVal` 跨页的情况）；
2. 用虚拟内存重写 `sys_trace`（读写用户地址、查询系统调用次数）；
3. 实现 `sys_mmap`：按给定权限映射一段用户虚拟地址；
4. 实现 `sys_munmap`：解除并释放映射。

## 二、实验环境

| 项 | 版本 |
|---|---|
| Rust | `nightly-2024-05-02`（rustc 1.80.0-nightly） |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| QEMU | 10.2.1（`qemu-system-riscv64`） |
| 检查器 | `rCore-Tutorial-Checker` + `rCore-Tutorial-Test` |

## 三、设计思路

### 3.1 用户指针不能再直接解引用

ch3 里 `*ts = TimeVal { .. }` 能工作，是因为内核和用户共用地址空间。
ch4 里当前 satp 指向任务自己的页表（内核态用 `KERNEL_SPACE`），直接解引用用户虚拟地址
要么读到内核的东西，要么触发缺页。必须**通过页表翻译**成物理地址再访问。

为此加了三层工具：

- `read_user_byte(token, addr)` / `write_user_byte(token, addr, val)`：
  用 `PageTable::from_token(token)` 翻译单个字节，并校验权限；
- `translated_byte_buffer`（内核已有）：把一段用户缓冲区翻译成若干页内切片；
- 对 `TimeVal` 这种可能**跨页**的结构，干脆**逐字节写**，天然不受页边界影响。

### 3.2 mmap 的参数校验

从测试反推出四条规则：

| 情况 | 期望 | 依据 |
|---|---|---|
| `start` 未页对齐 | -1 | `mmap(start + len + 1, len, 3)` 应失败 |
| `prot` 不在 1..=3 | -1 | `prot=0` 和 `prot|8` 都应失败 |
| 与已有映射重叠 | -1 | 先映射 `[start, start+len)`，再映射 `[start-len, start+len+1)` 应失败 |
| `len` 不必页对齐 | 0 | `mmap(start - len, len + 1, 3)` 应成功（向上取整到 2 页） |

权限映射：`prot` 第 0 位是 R、第 1 位是 W，再并上 `MapPermission::U`。
注意 `prot=2`（只写）在 RISC-V 里是保留编码（W 必须同时 R），
测试正好要求这种映射**读会触发缺页**，所以直接按 R=0/W=1 映射即可。

重叠检测用的是新增的 `MemorySet::is_area_free`：遍历已有 `MapArea` 的
`[vpn_range.start*PAGE_SIZE, vpn_range.end*PAGE_SIZE)` 做区间相交判断。

### 3.3 munmap 的两端对齐与精确匹配

测试要求：

- `munmap(start, len + 1)` → -1（长度未对齐）
- `munmap(start + 1, len - 1)` → -1（起始未对齐）

因此 munmap 要求**两端都页对齐**，并且必须**精确匹配一个已有 MapArea** 才能删除。
新增 `MemorySet::remove_area` 完成：找到起点/终点完全一致的 area，从 `areas` 里摘掉，
调用 `MapArea::unmap` 解除页表项并释放物理帧（`FrameTracker` 析构即归还）。

### 3.4 sys_trace 必须校验 U 位（本次最关键的一个坑）

最初实现只判断 `pte.is_valid() && pte.readable()`，结果 `trace_read(isize::MAX)` 断言失败：

```
Panicked at src/bin/ch4_trace1.rs:17, assertion `left == right` failed
```

原因值得记一下：**`isize::MAX` 不是规范虚拟地址**。SV39 只使用低 39 位做页表索引，
而 rCore 的 `PageTable::translate` 不会检查地址是否规范。`isize::MAX` 的低 39 位
和 **trampoline**（`usize::MAX - PAGE_SIZE + 1`）落在同一组索引上，
于是遍历真的走到了 trampoline 的页表项——一个**内核页**，因此"读成功"了。

修法：翻译后额外要求 `pte.flags().contains(PTEFlags::U)`。
用户页一定带 U 位，内核映射（trampoline、内核栈）不带，这样一来：

- `trace_read(isize::MAX)` → 别名到内核页 → 无 U 位 → `None` ✔
- `trace_read(0x80200000)` → 任务页表里根本没映射 → `None` ✔
- `trace_read(&var)` → 用户栈页有 U 位 → `Some(111)` ✔

## 四、关键代码

### 4.1 单字节翻译（带权限与 U 位校验）

```rust
fn read_user_byte(token: usize, addr: usize) -> Option<u8> {
    let page_table = PageTable::from_token(token);
    let pte = page_table.translate(VirtAddr(addr).floor())?;
    // 必须是用户页：非规范地址（如 isize::MAX）会别名到内核映射，靠 U 位排除
    if !pte.is_valid() || !pte.flags().contains(PTEFlags::U) || !pte.readable() {
        return None;
    }
    Some(pte.ppn().get_bytes_array()[addr % PAGE_SIZE])
}
```

### 4.2 `sys_get_time`：逐字节写，天然支持跨页

```rust
let bytes = unsafe {
    core::slice::from_raw_parts(&tv as *const TimeVal as *const u8, size_of::<TimeVal>())
};
for (i, b) in bytes.iter().enumerate() {
    if !write_user_byte(token, ts as usize + i, *b) {
        return -1;
    }
}
```

### 4.3 `sys_mmap`

```rust
if start % PAGE_SIZE != 0 || len == 0 { return -1; }
if !(1..=3).contains(&prot) { return -1; }

let start_va = VirtAddr(start);
let end_aligned = VirtAddr(VirtAddr(start + len).ceil().0 * PAGE_SIZE);

let mut perm = MapPermission::U;
if prot & 1 != 0 { perm |= MapPermission::R; }
if prot & 2 != 0 { perm |= MapPermission::W; }

with_current_memory_set(|ms| {
    if !ms.is_area_free(start_va, end_aligned) { return -1; }
    ms.insert_framed_area(start_va, end_aligned, perm);
    0
})
```

### 4.4 `sys_munmap`

```rust
if start % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 || len == 0 { return -1; }
with_current_memory_set(|ms| {
    if ms.remove_area(VirtAddr(start), VirtAddr(start + len)) { 0 } else { -1 }
})
```

## 五、测试结果

```
[PASS] get_time OK!              [PASS] Test sleep OK!
[PASS] current time_msec = ..    [PASS] time_msec = .. delta = ..ms!
[PASS] Test sleep1 passed!       [PASS] string from task trace test
[PASS] Test trace OK!            [PASS] Test 04_1 OK!
[PASS] Test 04_4 test OK!        [PASS] Test 04_5 ummap OK!
[PASS] Test 04_6 ummap2 OK!      [PASS] Test trace_1 OK!
[PASS] not found <Test sbrk failed!>
[PASS] not found <Should cause error, Test 04_2 fail!>
[PASS] not found <Should cause error, Test 04_3 fail!>

Test passed: 16/16
```

其中三条 `not found` 是**期望不出现**的：`ch4_mmap1`（只读页写入）、`ch4_mmap2`（只写页读取）、
`ch4b_sbrk`（越界收缩）都应当被内核拒绝并让进程缺页终止，**不能**打印出后续内容。

## 六、遇到的问题与思考

1. **非规范地址别名**（上面 3.4）。这是本章最隐蔽的问题：不报错、不崩溃，只是"多读到了一点东西"，
   而测试恰好用 `isize::MAX` 把它抓出来了。教训是：页表翻译只保证"这个 VPN 能翻出物理页"，
   不保证"这个地址对用户是合法的"，权限位和 U 位必须自己查。

2. **`TimeVal` 跨页**。题目提示"如果 `TimeVal` 被两页切开呢？"——直接 `*ts = ...` 在
   内核对用户指针本来就不可行；即使翻译成物理地址，一次 `write_volatile` 结构体也可能跨页。
   逐字节写是最省事且不会错的做法。

3. **页边界与取整方向**。`mmap` 的 `len` 向上取整、`munmap` 的 `len` 要求严格对齐，
   两者不对称但都有测试依据（后者测的是"参数不合法要拒绝"）。
