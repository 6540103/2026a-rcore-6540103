use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use alloc::vec;
use alloc::vec::Vec;
use crate::task::{block_current_and_run_next, current_process, current_task};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;
/// sleep syscall
pub fn sys_sleep(ms: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_sleep",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let expire_ms = get_time_ms() + ms;
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
    0
}
/// mutex create syscall
pub fn sys_mutex_create(blocking: bool) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mutex: Option<Arc<dyn Mutex>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();
    if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;
        id as isize
    } else {
        process_inner.mutex_list.push(mutex);
        process_inner.mutex_list.len() as isize - 1
    }
}
/// mutex lock syscall
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_lock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let mutex = {
        let mut inner = process.inner_exclusive_access();
        let mutex = Arc::clone(inner.mutex_list[mutex_id].as_ref().unwrap());
        if inner.deadlock_detect {
            inner.mutex_owner.resize(mutex_id + 1, None);
            // 同一个线程重复锁一个非递归 mutex -> 必然自死锁
            if inner.mutex_owner[mutex_id] == Some(tid) {
                return -0xdead;
            }
        }
        mutex
    };
    mutex.lock();
    if process.inner_exclusive_access().deadlock_detect {
        process.inner_exclusive_access().mutex_owner[mutex_id] = Some(tid);
    }
    0
}
/// mutex unlock syscall
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_unlock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mutex = {
        let mut inner = process.inner_exclusive_access();
        let mutex = Arc::clone(inner.mutex_list[mutex_id].as_ref().unwrap());
        if inner.deadlock_detect {
            inner.mutex_owner.resize(mutex_id + 1, None);
            inner.mutex_owner[mutex_id] = None;
        }
        mutex
    };
    mutex.unlock();
    0
}
/// semaphore create syscall
pub fn sys_semaphore_create(res_count: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));
        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));
        process_inner.semaphore_list.len() - 1
    };
    if process_inner.deadlock_detect {
        // 记录资源总量，供银行家算法计算 available
        process_inner.sem_avail.resize(id + 1, 0);
        process_inner.sem_avail[id] = res_count as isize;
        let nsems = process_inner.semaphore_list.len();
        for row in process_inner.sem_held.iter_mut() {
            row.resize(nsems, 0);
        }
        for row in process_inner.sem_need.iter_mut() {
            row.resize(nsems, 0);
        }
    }
    id as isize
}
/// semaphore up syscall
pub fn sys_semaphore_up(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_up",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let sem = {
        let mut inner = process.inner_exclusive_access();
        let sem = Arc::clone(inner.semaphore_list[sem_id].as_ref().unwrap());
        if inner.deadlock_detect {
            ensure_rows(&mut inner, tid, sem_id);
            inner.sem_held[tid][sem_id] -= 1;
        }
        sem
    };
    sem.up();
    0
}
/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_down",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let sem = {
        let mut inner = process.inner_exclusive_access();
        let sem = Arc::clone(inner.semaphore_list[sem_id].as_ref().unwrap());
        if inner.deadlock_detect {
            ensure_rows(&mut inner, tid, sem_id);
            // 记录"正在申请"
            inner.sem_need[tid][sem_id] += 1;
            let count = sem.inner.exclusive_access().count;
            if count <= 0 && !banker_safe(&inner) {
                // 满足这次请求会进入不安全状态 -> 判定为死锁
                inner.sem_need[tid][sem_id] -= 1;
                return -0xdead;
            }
        }
        sem
    };
    sem.down();
    {
        let mut inner = process.inner_exclusive_access();
        if inner.deadlock_detect {
            inner.sem_need[tid][sem_id] -= 1;
            inner.sem_held[tid][sem_id] += 1;
        }
    }
    0
}
/// condvar create syscall
pub fn sys_condvar_create() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}
/// condvar signal syscall
pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_signal",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}
/// condvar wait syscall
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_wait",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}
/// enable deadlock detection syscall
///
/// YOUR JOB: Implement deadlock detection, but might not all in this syscall
pub fn sys_enable_deadlock_detect(enabled: usize) -> isize {
    trace!("kernel: sys_enable_deadlock_detect {}", enabled);
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    inner.deadlock_detect = enabled != 0;
    if inner.deadlock_detect {
        // available 从各信号量的真实计数同步过来
        // 记录各信号量的资源总量（available = total - 已分配）
        inner.sem_avail = inner
            .semaphore_list
            .iter()
            .map(|s| s.as_ref().unwrap().inner.exclusive_access().count)
            .collect();
        let nsems = inner.semaphore_list.len();
        let ntasks = inner.tasks.len();
        inner.sem_held = vec![vec![0isize; nsems]; ntasks];
        inner.sem_need = vec![vec![0isize; nsems]; ntasks];
        let nmutex = inner.mutex_list.len();
        inner.mutex_owner.resize(nmutex, None);
    }
    0
}

/// Grow the bookkeeping arrays so that `[tid][sem_id]` is addressable.
fn ensure_rows(inner: &mut crate::task::ProcessControlBlockInner, tid: usize, sem_id: usize) {
    let ntasks = inner.tasks.len().max(tid + 1);
    let nsems = inner.semaphore_list.len().max(sem_id + 1);
    inner.sem_held.resize(ntasks, Vec::new());
    inner.sem_need.resize(ntasks, Vec::new());
    for row in inner.sem_held.iter_mut() {
        row.resize(nsems, 0);
    }
    for row in inner.sem_need.iter_mut() {
        row.resize(nsems, 0);
    }
    inner.sem_avail.resize(nsems, 0);
}

/// Is the current resource-allocation state safe? (banker's safety algorithm)
fn banker_safe(inner: &crate::task::ProcessControlBlockInner) -> bool {
    let ntasks = inner.tasks.len();
    let nsems = inner.semaphore_list.len();
    // available = 资源总量 - 各线程已持有之和
    let mut work: Vec<isize> = inner.sem_avail.clone();
    work.resize(nsems, 0);
    for t in 0..ntasks {
        for s in 0..nsems {
            work[s] -= inner.sem_held[t][s];
        }
    }
    let mut finish = vec![false; ntasks];
    loop {
        let mut progress = false;
        for t in 0..ntasks {
            if finish[t] {
                continue;
            }
            if inner.tasks[t].is_none() {
                finish[t] = true;
                progress = true;
                continue;
            }
            let can = (0..nsems).all(|s| inner.sem_need[t][s] <= work[s]);
            if can {
                for s in 0..nsems {
                    work[s] += inner.sem_held[t][s];
                }
                finish[t] = true;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    finish.iter().all(|&f| f)
}

/// Current thread id
fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}
