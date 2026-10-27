//! A minimal reference extension.
//!
//! It demonstrates the `organizer.v1` response shape: no network, no secrets,
//! no egui. The host validates every message id before the plan reaches the
//! list. The fixed plan is for one message, so this is a shape sample rather
//! than a useful organiser.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

static RESPONSE: &[u8] = br#"{"ok":true,"result":{"groups":[{"id":"all","title":"All mail","message_ids":["m1"],"order":0}]}}"#;

const HEAP_BYTES: usize = 8192;
static mut HEAP: [u8; HEAP_BYTES] = [0; HEAP_BYTES];

#[no_mangle]
pub extern "C" fn kimo_abi_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn kimo_alloc(len: u32) -> u32 {
    if len as usize > HEAP_BYTES {
        return 0;
    }
    core::ptr::addr_of_mut!(HEAP) as *mut u8 as u32
}

#[no_mangle]
pub extern "C" fn kimo_free(_ptr: u32, _len: u32) {}

#[no_mangle]
pub extern "C" fn kimo_invoke(_request_ptr: u32, _request_len: u32) -> u64 {
    let pointer = RESPONSE.as_ptr() as u64;
    (pointer << 32) | RESPONSE.len() as u64
}
