//! The Outlook / Microsoft 365 provider module: Microsoft Graph over the
//! host's HTTPS import, OAuth2 with Entra ID (authorization code + PKCE, a
//! public client with no secret), and the browser sign-in the host runs.
//!
//! Build for the host Kimo loads:
//!
//! ```sh
//! cargo build --release --target wasm32-unknown-unknown
//! cp target/wasm32-unknown-unknown/release/kimo_plugin_outlook.wasm outlook.wasm
//! ```
//!
//! Single-instance and single-threaded by the host's rules: one
//! `kimo_invoke` at a time, no threads, no WASI, and only the `kimo` imports
//! the manifest declared — which for this provider is `http_request` and
//! nothing socket-shaped: Graph is HTTPS all the way down.

mod abi;
mod graph;
mod oauth;
mod ops;

/// The request buffer the host copies into. One allocation is alive at a
/// time — the host allocates, writes, invokes — so a bump arena that is
/// reset at the end of every invoke is the whole allocator.
struct Arena {
    buf: Vec<u8>,
    used: usize,
}

impl Arena {
    fn alloc(&mut self, len: usize) -> u32 {
        if len > self.buf.len() - self.used {
            self.buf
                .resize((self.used + len).max(self.buf.len() * 2), 0);
        }
        let pointer = self.used;
        self.used += len;
        // The arena only lives in this module's linear memory; the pointer
        // is an offset into it, which is exactly what the ABI wants.
        self.buf.as_ptr() as u32 + pointer as u32
    }
}

static mut ARENA: Option<Arena> = None;
static mut RESPONSE: Vec<u8> = Vec::new();

#[no_mangle]
pub extern "C" fn kimo_abi_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn kimo_alloc(len: u32) -> u32 {
    let arena = unsafe { &mut *std::ptr::addr_of_mut!(ARENA) }.get_or_insert_with(|| Arena {
        buf: Vec::new(),
        used: 0,
    });
    arena.alloc(len as usize)
}

#[no_mangle]
pub extern "C" fn kimo_free(_pointer: u32, _len: u32) {
    // The arena resets with the invoke that owned it.
}

#[no_mangle]
pub extern "C" fn kimo_invoke(request_ptr: u32, request_len: u32) -> u64 {
    let request =
        unsafe { core::slice::from_raw_parts(request_ptr as *const u8, request_len as usize) };
    let response = ops::dispatch(request);
    // The arena's one allocation was the request, and dispatch has copied
    // everything it kept out of it.
    if let Some(arena) = unsafe { &mut *std::ptr::addr_of_mut!(ARENA) } {
        arena.used = 0;
    }
    unsafe {
        let slot = &mut *std::ptr::addr_of_mut!(RESPONSE);
        *slot = response;
        ((*slot).as_ptr() as u64) << 32 | (*slot).len() as u64
    }
}
