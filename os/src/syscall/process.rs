//! Process management syscalls
use crate::{
    config::PAGE_SIZE,
    mm::{MapPermission, PageTable, VirtAddr, PTEFlags},
    task::{
        change_program_brk, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next, with_current_memory_set,
    },
    timer::get_time_us,
};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let token = current_user_token();
    let us = get_time_us();
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
    // 逐字节写，这样即使 TimeVal 跨页也能正确处理
    for (i, b) in bytes.iter().enumerate() {
        if !write_user_byte(token, ts as usize + i, *b) {
            return -1;
        }
    }
    0
}

/// Translate `addr` in the given address space and return the byte if readable.
fn read_user_byte(token: usize, addr: usize) -> Option<u8> {
    let page_table = PageTable::from_token(token);
    let pte = page_table.translate(VirtAddr(addr).floor())?;
    // 必须是用户页：非规范地址（如 isize::MAX）会别名到内核映射，靠 U 位排除
    if !pte.is_valid() || !pte.flags().contains(PTEFlags::U) || !pte.readable() {
        return None;
    }
    let offset = addr % PAGE_SIZE;
    Some(pte.ppn().get_bytes_array()[offset])
}

/// Write one byte to `addr` if it is mapped and writable.
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

/// TODO: Finish sys_trace to pass testcases
/// HINT: You might reimplement it with virtual memory management.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    let token = current_user_token();
    match trace_request {
        // Read one byte from a user address
        0 => match read_user_byte(token, id) {
            Some(b) => b as isize,
            None => -1,
        },
        // Write one byte to a user address
        1 => {
            if write_user_byte(token, id, data as u8) {
                0
            } else {
                -1
            }
        }
        // Query how many times this task has called syscall `id`
        2 => crate::task::syscall_count(id) as isize,
        _ => -1,
    }
}

// YOUR JOB: Implement mmap.
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    // start 必须页对齐，长度非零
    if start % PAGE_SIZE != 0 || len == 0 {
        return -1;
    }
    // prot 只允许 1(R) / 2(W) / 3(RW)
    if !(1..=3).contains(&prot) {
        return -1;
    }
    let start_va = VirtAddr(start);
    let end_va = VirtAddr(start + len);
    // 页对齐后的结束地址（VirtAddr::ceil 给出末尾的下一个页号）
    let end_aligned = VirtAddr(end_va.ceil().0 * PAGE_SIZE);

    let mut perm = MapPermission::U;
    if prot & 1 != 0 {
        perm |= MapPermission::R;
    }
    if prot & 2 != 0 {
        perm |= MapPermission::W;
    }

    with_current_memory_set(|ms| {
        if !ms.is_area_free(start_va, end_aligned) {
            return -1;
        }
        ms.insert_framed_area(start_va, end_aligned, perm);
        0
    })
}

// YOUR JOB: Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    // 两端都必须页对齐，长度非零
    if start % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 || len == 0 {
        return -1;
    }
    let start_va = VirtAddr(start);
    let end_va = VirtAddr(start + len);
    with_current_memory_set(|ms| {
        if ms.remove_area(start_va, end_va) {
            0
        } else {
            -1
        }
    })
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
