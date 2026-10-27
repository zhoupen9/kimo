//! A minimal reference provider module.
//!
//! It answers `plugin.describe`/every request with a fixed JSON response and
//! has no network, secrets or storage imports. Build it for
//! `wasm32-unknown-unknown`; the host loads the resulting `.wasm` file.
//!
//! This is a smoke-test provider, not a mail protocol: it exists to show the
//! exported ABI and a manifest a developer can copy.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

static RESPONSE: &[u8] = br#"{"ok":true,"result":{"provider":"echo","capabilities":[]}}"#;

/// The request buffer the host may write into. The sample ignores the
/// request, so one fixed page is enough.
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
