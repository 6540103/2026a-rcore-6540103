# 实验四：文件系统与 I/O —— `fstat` / `link` / `unlink`

## 一、实验目标

ch6 引入了基于 easy-fs 的文件系统。本章要完成：

1. **`sys_fstat`**：取得文件描述符对应的 inode 元信息（dev / ino / mode / nlink）；
2. **`sys_linkat`**：为已有文件创建硬链接；
3. **`sys_unlinkat`**：删除目录项并递减链接数；
4. 移植 ch5 的 `sys_get_time` / `sys_mmap` / `sys_munmap` / `sys_spawn` / `sys_set_priority`
   （ch6 的 spawn 改为从文件系统读取 ELF）。

## 二、实验环境

| 项 | 版本 |
|---|---|
| Rust | `nightly-2024-05-02` |
| 目标平台 | `riscv64gc-unknown-none-elf` |
| QEMU | 10.2.1 |
| 文件系统 | 仓库自带 `easy-fs` + `easy-fs-fuse` |

## 三、设计思路

### 3.1 硬链接需要"链接计数"

硬链接的本质是**多个目录项指向同一个 inode**。要正确统计"还有几个名字指向它"，
inode 里必须有一个链接计数。原来的 `DiskInode` 没有这个字段：

```rust
pub struct DiskInode {
    pub size: u32,
    pub direct: [u32; INODE_DIRECT_COUNT],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
}
```

于是加上 `nlink: u32`，并在 `initialize` 里初始化为 1（新建的文件天然有一个名字）。
磁盘布局的字节数是用 `size_of::<DiskInode>()` 算的，内核与 `easy-fs-fuse` 共用同一个结构体，
所以镜像格式会自动保持一致，不需要手工改常量。

### 3.2 easy-fs 需要补三个操作

原来的 `Inode` 只有 `find / create / ls / read_at / write_at / clear`，不足以支持链接：

| 新方法 | 作用 |
|---|---|
| `inode_id()` | 取 inode 编号（`fstat` 的 `ino`，也是给目录项用的 id） |
| `link()` / `unlink()` | 原子地增减 `nlink` 并同步到磁盘 |
| `add_dir_entry(name, id)` | 在目录里追加一个指向**已存在** inode 的目录项 |
| `remove_dir_entry(name)` | 删除目录项，并把最后一项搬过来填洞 |

`remove_dir_entry` 用的是"用末项填补空洞再缩小 size"的做法，
这样目录内容始终是连续的，不需要额外的空闲链表，也不会留下空槽。

### 3.3 内核侧：给 `File` trait 加 `inode()`

`sys_fstat` 拿到的是 `Arc<dyn File + Send + Sync>`，而只有 `OSInode` 背后才有真正的 inode。
所以在 trait 上加一个带默认实现的方法：

```rust
fn inode(&self) -> Option<Arc<Inode>> { None }
```

`OSInode` 返回 `Some(...)`，`Stdin` / `Stdout` 沿用默认的 `None`，
这样 `sys_fstat(0)` 之类的调用会自然返回 -1，而不是 panic 或误报。

## 四、关键代码

### 4.1 `sys_linkat`

```rust
let Some(inode) = ROOT_INODE.find(old_name.as_str()) else { return -1 };
// 新名字不能已经存在
if ROOT_INODE.find(new_name.as_str()).is_some() { return -1; }
if !ROOT_INODE.add_dir_entry(new_name.as_str(), inode.inode_id()) { return -1; }
inode.link();
0
```

### 4.2 `sys_unlinkat`

先按名字拿到 inode 句柄，再删目录项，最后递减计数——顺序不能反，
因为删掉目录项之后就再也按名字找不到这个 inode 了。

```rust
let Some(inode) = ROOT_INODE.find(name.as_str()) else { return -1 };
if ROOT_INODE.remove_dir_entry(name.as_str()).is_none() { return -1; }
inode.unlink();
0
```

### 4.3 `sys_fstat`

```rust
let inode = file.inode()?;        // 通过 File trait
stat.ino   = inode.inode_id() as u64;
stat.mode  = if inode.is_dir() { StatMode::DIR } else { StatMode::FILE };
stat.nlink = inode.nlink();
```

## 五、测试结果

```
[PASS] Test fstat OK!            [PASS] Test link OK!
[PASS] Test mass open/unlink OK! [PASS] Test set_priority OK!
[PASS] Test spawn0 OK!           [PASS] Test wait OK!
[PASS] Test 04_1 / 04_4 / 04_5 / 04_6 OK!
[PASS] file_test passed!         [PASS] exit pass.

Test passed: 31/31
```

## 六、遇到的问题与思考

### 6.1 `inode_id` 换算错误（本次最隐蔽的 bug）

第一版这样算：

```rust
((self.block_id * BLOCK_SZ + self.block_offset) / size_of::<DiskInode>()) as u32
```

看起来"绝对字节地址 ÷ inode 大小 = inode 编号"，但 **`block_id` 是绝对块号**，
而 `get_disk_inode_pos` 里的公式是

```
block_id = inode_area_start_block + inode_id / inodes_per_block
```

也就是说块号里含着 `inode_area_start_block` 这个偏移。直接除会把整个 inode 区
往后平移若干个 inode，`link` 建出来的目录项就指向了别的 inode。

表现非常隐蔽：`fstat` 的 `nlink` 全对（因为它读的是当前 inode 自己的字段），
只有 `link` 之后再按新名字读文件才暴露——读到的是空内容：

```
Panicked at src/bin/ch6_file2.rs:32, assertion `left == right` failed
  left: "Hello, world!"
 right: ""
```

修正方法是先把绝对块号减掉 inode 区起始块，换算成相对块号再算编号，
并把 `inode_area_start_block` 的可见性放宽到 `pub(crate)` 以便同 crate 内访问。

### 6.2 磁盘结构体加字段的安全性

给 `DiskInode` 加字段会改变它在磁盘上的大小。之所以安全，是因为
**镜像是在构建期由 `easy-fs-fuse` 用同一个 Rust 结构体重新生成的**，
内核和 fuse 工具编译的是同一份代码，两边对 `size_of::<DiskInode>()` 的认识一致。
如果镜像是外部预制的二进制，就必须考虑版本兼容。

### 6.3 spawn 的两种实现

ch5 的 spawn 从内核内嵌的 app 数据里按名字取 ELF；ch6 有了文件系统，
改成 `open_file(path, RDONLY)` + `read_all()`。接口对用户完全一样，
这也说明"进程创建"和"程序从哪来"是两件可以解耦的事。
