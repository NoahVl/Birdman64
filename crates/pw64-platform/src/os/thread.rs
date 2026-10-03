//! libultra thread API over the coroutine scheduler in [`super`].
//! Mirrors os/{createthread,startthread,stopthread,setthreadpri,
//! getthreadpri,yieldthread}.c. The C `OSThread` is only used as a handle:
//! its fields (context, priority, ...) are not maintained.

use super::{State, block, checkpoint, create_thread, reschedule, suspend, trace, with};
use std::ffi::c_void;

pub type ThreadEntry = unsafe extern "C-unwind" fn(*mut c_void);

/// Mirrors `osCreateThread`. `sp` (top of the C stack array) is ignored: the
/// thread runs on a large host stack above the RDRAM window (see `stack.rs`).
///
/// # Safety
/// `t` must point at OSThread storage the C owns; `entry` must be a valid
/// C-unwind entry function.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osCreateThread(
    t: *mut c_void,
    id: i32,
    entry: ThreadEntry,
    arg: *mut c_void,
    _sp: *mut c_void,
    pri: i32,
) {
    assert!(!t.is_null(), "osCreateThread(NULL)");
    let arg = arg as usize;
    // SAFETY: C thread entry with its argument.
    create_thread(
        t as usize,
        id,
        pri,
        Box::new(move || unsafe { entry(arg as *mut c_void) }),
    );
}

fn lookup(t: *mut c_void, what: &str) -> usize {
    with(|k| k.index(t as usize)).unwrap_or_else(|| panic!("{what}: unknown thread {t:?}"))
}

/// Mirrors `osStartThread`: a stopped (or waiting) thread becomes ready and
/// preempts the caller if its priority is higher.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osStartThread(t: *mut c_void) {
    let idx = lookup(t, "osStartThread");
    with(|k| {
        let th = k.thread_mut(idx);
        match th.state {
            State::Stopped | State::WaitRecv(_) | State::WaitSend(_) => {
                trace!("start thread id {}", th.id);
                k.make_ready(idx);
            }
            _ => {}
        }
    });
    reschedule();
}

/// Mirrors `osStopThread`; null stops the caller.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osStopThread(t: *mut c_void) {
    let idx = lookup(t, "osStopThread");
    let is_current = with(|k| {
        let cur = k.index(0) == Some(idx);
        let th = k.thread_mut(idx);
        if th.state != State::Dead {
            th.state = State::Stopped;
        }
        cur
    });
    if is_current {
        suspend();
    }
}

/// Mirrors `osSetThreadPri`; null means the caller. Dropping the caller to
/// `OS_PRIORITY_IDLE` parks it for good (see module docs of [`super`]).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osSetThreadPri(t: *mut c_void, pri: i32) {
    let idx = lookup(t, "osSetThreadPri");
    with(|k| {
        let th = k.thread_mut(idx);
        trace!("thread id {} pri {} -> {pri}", th.id, th.pri);
        th.pri = pri;
    });
    reschedule();
}

/// Mirrors `osGetThreadPri`; null means the caller.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osGetThreadPri(t: *mut c_void) -> i32 {
    let idx = lookup(t, "osGetThreadPri");
    with(|k| k.thread_mut(idx).pri)
}

/// Mirrors `osGetThreadId`; null means the caller.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osGetThreadId(t: *mut c_void) -> i32 {
    let idx = lookup(t, "osGetThreadId");
    with(|k| k.thread_mut(idx).id)
}

/// Mirrors `osYieldThread`: goes behind other ready threads of equal priority.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osYieldThread() {
    checkpoint();
    block(State::Ready);
}

/// No fault handling natively; only reached after `OS_EVENT_FAULT`, which
/// never fires.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn __osGetCurrFaultedThread() -> *mut c_void {
    std::ptr::null_mut()
}
