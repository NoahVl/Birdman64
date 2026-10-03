//! Message queues and events: `osCreateMesgQueue`, `osSendMesg`,
//! `osJamMesg`, `osRecvMesg`, `osSetEventMesg` (libultra os/*mesg*.c).
//!
//! The ring buffer lives in the C `OSMesgQueue` itself, because C code reads
//! `validCount` directly (`MQ_IS_EMPTY` etc.). Blocked threads are tracked
//! Rust-side by thread state, so `mtqueue`/`fullqueue` stay null.

use super::{State, block, checkpoint, reschedule, with};
use std::ffi::c_void;

pub const OS_MESG_NOBLOCK: i32 = 0;

pub type OSMesg = *mut c_void;

/// libultra `OSMesgQueue`, native (x64) layout.
#[repr(C)]
pub struct OSMesgQueue {
    pub mtqueue: *mut c_void,
    pub fullqueue: *mut c_void,
    pub valid_count: i32,
    pub first: i32,
    pub msg_count: i32,
    pub msg: *mut OSMesg,
}

/// Appends a message. False if the queue is full.
///
/// # Safety
/// `mq` must point at a queue initialised by `osCreateMesgQueue`.
pub(crate) unsafe fn push_back(mq: *mut OSMesgQueue, msg: OSMesg) -> bool {
    // SAFETY: caller contract (the `# Safety` note): `mq` points at a queue
    // initialised by `osCreateMesgQueue`.
    let q = unsafe { &mut *mq };
    if q.valid_count >= q.msg_count {
        return false;
    }
    let slot = (q.first + q.valid_count) % q.msg_count;
    // SAFETY: `slot` < `msg_count`, and `q.msg` is the `msg_count`-slot
    // buffer `osCreateMesgQueue` stored in the queue.
    unsafe { *q.msg.add(slot as usize) = msg };
    q.valid_count += 1;
    true
}

/// Mirrors `osCreateMesgQueue`.
///
/// # Safety
/// `mq` must point at queue storage and `msg` at `count` `OSMesg` slots,
/// both outliving the queue (the C uses statics).
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osCreateMesgQueue(
    mq: *mut OSMesgQueue,
    msg: *mut OSMesg,
    count: i32,
) {
    // SAFETY: C passes a queue struct and a buffer of `count` messages.
    unsafe {
        mq.write(OSMesgQueue {
            mtqueue: std::ptr::null_mut(),
            fullqueue: std::ptr::null_mut(),
            valid_count: 0,
            first: 0,
            msg_count: count,
            msg,
        })
    };
}

/// Mirrors `osSendMesg`: appends, blocking while the queue is full.
///
/// # Safety
/// `mq` must point at a queue initialised by `osCreateMesgQueue`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osSendMesg(mq: *mut OSMesgQueue, msg: OSMesg, flag: i32) -> i32 {
    checkpoint();
    loop {
        // SAFETY: initialised queue (C contract).
        let full = unsafe { (*mq).valid_count >= (*mq).msg_count };
        if !full {
            break;
        }
        if flag == OS_MESG_NOBLOCK {
            return -1;
        }
        block(State::WaitSend(mq as usize));
    }
    with(|k| k.post(mq as usize, msg as usize));
    reschedule();
    0
}

/// Mirrors `osJamMesg`: like `osSendMesg` but puts the message at the front.
///
/// # Safety
/// `mq` must point at a queue initialised by `osCreateMesgQueue`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osJamMesg(mq: *mut OSMesgQueue, msg: OSMesg, flag: i32) -> i32 {
    checkpoint();
    loop {
        // SAFETY: initialised queue (C contract).
        let full = unsafe { (*mq).valid_count >= (*mq).msg_count };
        if !full {
            break;
        }
        if flag == OS_MESG_NOBLOCK {
            return -1;
        }
        block(State::WaitSend(mq as usize));
    }
    // SAFETY: not full, checked above; nothing ran in between.
    unsafe {
        let q = &mut *mq;
        q.first = (q.first + q.msg_count - 1) % q.msg_count;
        *q.msg.add(q.first as usize) = msg;
        q.valid_count += 1;
    }
    with(|k| k.wake_one(State::WaitRecv(mq as usize)));
    reschedule();
    0
}

/// Mirrors `osRecvMesg`: takes the oldest message, blocking while empty.
/// `msg` may be null (message discarded).
///
/// # Safety
/// `mq` must point at a queue initialised by `osCreateMesgQueue`; `msg`
/// (when not null) at writable `OSMesg` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osRecvMesg(
    mq: *mut OSMesgQueue,
    msg: *mut OSMesg,
    flag: i32,
) -> i32 {
    checkpoint();
    loop {
        // SAFETY: initialised queue (C contract).
        if unsafe { (*mq).valid_count } != 0 {
            break;
        }
        if flag == OS_MESG_NOBLOCK {
            return -1;
        }
        block(State::WaitRecv(mq as usize));
    }
    // SAFETY: non-empty initialised queue; `msg` null or writable.
    unsafe {
        let q = &mut *mq;
        if !msg.is_null() {
            *msg = *q.msg.add(q.first as usize);
        }
        q.first = (q.first + 1) % q.msg_count;
        q.valid_count -= 1;
    }
    with(|k| k.wake_one(State::WaitSend(mq as usize)));
    reschedule();
    0
}

/// Mirrors `osSetEventMesg`: `event` (OS_EVENT_*) will post `msg` to `mq`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osSetEventMesg(event: u32, mq: *mut OSMesgQueue, msg: OSMesg) {
    with(|k| {
        let slot = k
            .events
            .get_mut(event as usize)
            .unwrap_or_else(|| panic!("osSetEventMesg: bad event {event}"));
        *slot = (!mq.is_null()).then_some((mq as usize, msg as usize));
    });
}

/// Host side (SP/DP done, SI, ...): posts the message registered for
/// `event`, like the libultra interrupt handler (`__osEnqueue`, no blocking).
/// From a game thread, a woken higher-priority thread preempts the caller.
pub fn post_event(event: usize) {
    with(|k| k.post_event(event));
    reschedule();
}
