//! Process management syscalls
use alloc::sync::Arc;

use crate::{
    config::PAGE_SIZE,
    loader::get_app_data_by_name,
    mm::{translated_refmut, translated_str, MapPermission, PageTable, VirtAddr, PTEFlags},
    task::{
        add_task, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next, TaskControlBlock,
    },
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel:pid[{}] sys_yield", current_task().unwrap().pid.0);
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let task = current_task().unwrap();
        task.exec(data);
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel::pid[{}] sys_waitpid [{}]", current_task().unwrap().pid.0, pid);
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let token = current_user_token();
    let us = crate::timer::get_time_us();
    let tv = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &tv as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };
    // 逐字节写，天然支持 TimeVal 跨页
    for (i, b) in bytes.iter().enumerate() {
        if !write_user_byte(token, ts as usize + i, *b) {
            return -1;
        }
    }
    0
}

/// Write one byte to `addr` if it is a writable user page.
fn write_user_byte(token: usize, addr: usize, value: u8) -> bool {
    let page_table = PageTable::from_token(token);
    match page_table.translate(VirtAddr(addr).floor()) {
        Some(pte)
            if pte.is_valid()
                && pte.flags().contains(PTEFlags::U)
                && pte.writable() =>
        {
            pte.ppn().get_bytes_array()[addr % PAGE_SIZE] = value;
            true
        }
        _ => false,
    }
}

/// YOUR JOB: Implement mmap.
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    if start % PAGE_SIZE != 0 || len == 0 {
        return -1;
    }
    if !(1..=3).contains(&prot) {
        return -1;
    }
    let start_va = VirtAddr(start);
    let end_aligned = VirtAddr(VirtAddr(start + len).ceil().0 * PAGE_SIZE);

    let mut perm = MapPermission::U;
    if prot & 1 != 0 {
        perm |= MapPermission::R;
    }
    if prot & 2 != 0 {
        perm |= MapPermission::W;
    }

    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if !inner.memory_set.is_area_free(start_va, end_aligned) {
        return -1;
    }
    inner
        .memory_set
        .insert_framed_area(start_va, end_aligned, perm);
    0
}

/// YOUR JOB: Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    if start % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 || len == 0 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    if inner
        .memory_set
        .remove_area(VirtAddr(start), VirtAddr(start + len))
    {
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// YOUR JOB: Implement spawn.
/// HINT: fork + exec =/= spawn
pub fn sys_spawn(path: *const u8) -> isize {
    let task = current_task().unwrap();
    let token = task.get_user_token();
    let path = translated_str(token, path);
    let Some(elf) = get_app_data_by_name(path.as_str()) else {
        return -1;
    };
    let child = Arc::new(TaskControlBlock::new(elf));
    let pid = child.getpid();
    {
        let mut child_inner = child.inner_exclusive_access();
        child_inner.parent = Some(Arc::downgrade(&task));
    }
    {
        let mut parent_inner = task.inner_exclusive_access();
        parent_inner.children.push(child.clone());
    }
    add_task(child);
    pid as isize
}

// YOUR JOB: Set task priority.
pub fn sys_set_priority(prio: isize) -> isize {
    // 优先级必须 >= 2：pass = BIG_STRIDE / prio，太小会让 stride 失控
    if prio < 2 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.priority = prio as usize;
    inner.pass = crate::task::BIG_STRIDE / (prio as usize);
    prio
}
