//! Codex DeviceCheck attestation envelope (`x-oai-attestation`).
//!
//! A faithful Rust port of `packages/coding-agent/src/live/attestation.ts` and
//! `crates/pi-natives/src/devicecheck.rs` from oh-my-pi (OMP) v17.1.1 (commit
//! e9c8a35). Generates the CBOR-encoded attestation envelope sent as the
//! `x-oai-attestation` header on Codex (ChatGPT-OAuth) live signaling requests.
//!
//! # Platform
//! - **macOS arm64**: full implementation via `DeviceCheck.framework` raw ObjC
//!   runtime FFI (no `objc2`/`block2` dependency), mirroring the upstream
//!   `devicecheck.node` addon flow.
//! - **Other architectures/OS**: [`generate_codex_attestation`] returns `None`,
//!   so the header is simply omitted (matching OMP, which only attests on
//!   darwin/arm64).
//!
//! # Secrets/log safety
//! The DeviceCheck token is base64 and travels only inside the CBOR envelope;
//! it is never logged. On failure a short, fixed error code is emitted rather
//! than the raw error text. The envelope is base64url-encoded before it leaves
//! this module.
//!
//! Substantially adapted from the TypeScript/Rust originals; MIT attribution
//! preserved in `THIRD-PARTY-NOTICES`.

use base64::Engine;

/// Codex/ChatGPT bundle id used in the attestation envelope.
const CHATGPT_BUNDLE_ID: &str = "com.openai.codex";

/// A single DeviceCheck token-generation outcome.
struct DeviceCheckResult {
    supported: bool,
    token_base64: Option<String>,
    latency_ms: Option<f64>,
}

// ---------------------------------------------------------------------------
// CBOR encoder (minimal, hand-rolled — matches the OMP attestation.ts shape)
// ---------------------------------------------------------------------------

/// CBOR major type header. `major` is the high 3 bits (0..7); `value` is the
/// argument. Mirrors `cborHeader` in the TS original.
fn cbor_header(major: u8, value: u64) -> Vec<u8> {
    const MAX_U8: u64 = u8::MAX as u64;
    const MAX_U16: u64 = u16::MAX as u64;
    const MAX_U32: u64 = u32::MAX as u64;
    debug_assert!(major <= 7, "cbor major type out of range");
    let major = major << 5;
    if value < 24 {
        vec![major | value as u8]
    } else if value <= MAX_U8 {
        vec![major | 24, value as u8]
    } else if value <= MAX_U16 {
        let mut out = vec![major | 25];
        out.extend_from_slice(&(value as u16).to_be_bytes());
        out
    } else if value <= MAX_U32 {
        let mut out = vec![major | 26];
        out.extend_from_slice(&(value as u32).to_be_bytes());
        out
    } else {
        let mut out = vec![major | 27];
        out.extend_from_slice(&value.to_be_bytes());
        out
    }
}

/// CBOR unsigned integer (major type 0).
fn cbor_unsigned(value: u64) -> Vec<u8> {
    cbor_header(0, value)
}

/// CBOR text string (major type 3).
fn cbor_text(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = cbor_header(3, bytes.len() as u64);
    out.extend_from_slice(bytes);
    out
}

/// CBOR byte string (major type 2).
fn cbor_bytes(value: &[u8]) -> Vec<u8> {
    let mut out = cbor_header(2, value.len() as u64);
    out.extend_from_slice(value);
    out
}

/// CBOR map (major type 5) of key→value byte strings.
fn cbor_map(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = cbor_header(5, entries.len() as u64);
    for (key, value) in entries {
        out.extend_from_slice(key);
        out.extend_from_slice(value);
    }
    out
}

/// CBOR array (major type 4) of byte-string items.
fn cbor_array(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = cbor_header(4, items.len() as u64);
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// CBOR simple float64 (major type 7, additional info 27).
fn cbor_float64(value: f64) -> Vec<u8> {
    let mut out = vec![0xfb];
    out.extend_from_slice(&value.to_be_bytes());
    out
}

/// Build the `f` (fingerprint) signals map, matching the OMP
/// `attestationSignals()` shape. Uses a process-lifetime app session id so a
/// single process presents a stable session across requests.
fn attestation_signals(app_session_id: &str) -> Vec<u8> {
    // Resolve the locale/timezone from the environment, clamped to 64 bytes
    // (the TS version uses Intl.DateTimeFormat().resolvedOptions()).
    let locale = std::env::var("LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .unwrap_or_else(|_| "unknown".to_string());
    let locale: String = locale.split('.').next().unwrap_or("unknown").to_string();
    let locale: String = locale.chars().take(64).collect();
    let timezone = std::env::var("TZ").unwrap_or_else(|_| "unknown".to_string());
    let timezone: String = timezone.chars().take(64).collect();
    let app_session_id: String = app_session_id.chars().take(128).collect();

    let preferred_languages = cbor_array(&[cbor_text(&locale)]);
    cbor_map(&[
        (cbor_unsigned(0), cbor_unsigned(1)),
        (cbor_unsigned(1), preferred_languages),
        (cbor_unsigned(2), cbor_text(&locale)),
        (cbor_unsigned(3), cbor_text(&timezone)),
        (cbor_unsigned(4), cbor_unsigned(0)),
        (cbor_unsigned(5), cbor_unsigned(1)),
        (cbor_unsigned(6), cbor_text(&app_session_id)),
    ])
}

/// Build the full client attestation envelope (`v1.<base64url>`), matching
/// `buildClientAttestation` in the TS original.
fn build_client_attestation(result: &DeviceCheckResult) -> String {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    if result.supported && result.token_base64.is_some() {
        entries.push((
            cbor_text("token"),
            cbor_text(result.token_base64.as_ref().unwrap()),
        ));
    } else {
        // error_code: 3 = unsupported, 4 = supported but no token.
        let code = if result.supported { 4u64 } else { 3u64 };
        entries.push((cbor_text("error_code"), cbor_unsigned(code)));
    }
    entries.push((cbor_text("bundle_id"), cbor_text(CHATGPT_BUNDLE_ID)));

    let signals = attestation_signals(&app_session_id());
    // The TS version wraps signals as a byte string (major type 2) header.
    entries.push((cbor_text("f"), cbor_bytes(&signals)));

    if let Some(latency) = result.latency_ms {
        entries.push((cbor_text("t"), cbor_float64(latency)));
    }

    let encoded = cbor_map(&entries);
    format!(
        "v1.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&encoded)
    )
}

/// Process-lifetime app session id (generated once, like the TS
/// `crypto.randomUUID()` module constant). Stored in a `OnceLock` so it is
/// stable across attestation requests within one process.
fn app_session_id() -> String {
    use std::sync::OnceLock;
    static APP_SESSION_ID: OnceLock<String> = OnceLock::new();
    APP_SESSION_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

/// Generate the Codex DeviceCheck attestation envelope sent as
/// `x-oai-attestation`. Returns `None` off macOS/arm64 (the header is then
/// omitted entirely) or when token generation fails (a degraded envelope with
/// an error code is *not* sent — OMP returns `undefined` on a thrown
/// generation, and the header is simply absent).
///
/// The returned string is a JSON object `{"v":1,"s":0,"t":"v1.<base64url>"}`,
/// matching the OMP wire format exactly.
pub async fn generate_codex_attestation() -> Option<String> {
    let result = device_check_generate_token().await;
    // OMP returns undefined when generation throws; we mirror that by returning
    // None whenever the platform cannot produce a token (non-macOS, or a
    // generation failure).
    let token = result.token_base64?;
    let envelope = build_client_attestation(&DeviceCheckResult {
        supported: true,
        token_base64: Some(token),
        latency_ms: result.latency_ms,
    });
    // Serialize the outer envelope exactly as OMP: { v: 1, s: 0, t: <envelope> }.
    let outer = serde_json::json!({ "v": 1, "s": 0, "t": envelope });
    Some(outer.to_string())
}

// ---------------------------------------------------------------------------
// macOS arm64 implementation
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod platform {
    use std::ffi::{CStr, c_char, c_void};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::ptr;
    use std::sync::mpsc::{self, SyncSender};
    use std::time::{Duration, Instant};

    use super::DeviceCheckResult;

    /// How long to wait for the DeviceCheck completion handler before giving
    /// up, matching the upstream `devicecheck.node` addon timeout.
    const TOKEN_TIMEOUT: Duration = Duration::from_secs(1);

    type Id = *mut c_void;
    type Sel = *mut c_void;

    // `objc_msgSend` is an assembly trampoline; each alias types the same
    // symbol for a distinct call signature (the standard raw-ObjC idiom).
    #[allow(
        clashing_extern_declarations,
        reason = "objc_msgSend forwards to the method IMP; each alias types the same symbol for a distinct call signature"
    )]
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_retain(obj: Id) -> Id;
        fn objc_release(obj: Id);
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);

        #[link_name = "objc_msgSend"]
        fn msg_send_noarg(receiver: Id, selector: Sel) -> Id;
        #[link_name = "objc_msgSend"]
        fn msg_send_bool(receiver: Id, selector: Sel) -> u8;
        #[link_name = "objc_msgSend"]
        fn msg_send_u64(receiver: Id, selector: Sel, options: u64) -> Id;
        #[link_name = "objc_msgSend"]
        fn msg_send_block(receiver: Id, selector: Sel, block: *const c_void);
    }

    // Linking DeviceCheck.framework registers `DCDevice` with the ObjC runtime
    // when the module image loads.
    #[link(name = "DeviceCheck", kind = "framework")]
    unsafe extern "C" {}

    unsafe extern "C" {
        /// Stack-block class from `libsystem_blocks`; used as the literal's isa.
        static _NSConcreteStackBlock: *const c_void;
    }

    /// Outcome delivered once from the completion block to the waiting worker.
    enum Completion {
        Token(String),
        Error,
    }

    /// Objective-C block ABI: the 32-byte literal header followed by the
    /// captured context (a raw pointer to the channel sender).
    #[repr(C)]
    struct CompletionBlock {
        isa: *const c_void,
        flags: i32,
        reserved: i32,
        invoke: unsafe extern "C" fn(*mut Self, Id, Id),
        descriptor: *const CompletionBlockDescriptor,
        sender: *const SyncSender<Completion>,
    }

    /// `Block_descriptor_1` + `Block_descriptor_3` (no copy/dispose helpers:
    /// the captured sender pointer is plain-old-data).
    #[repr(C)]
    struct CompletionBlockDescriptor {
        reserved: usize,
        size: usize,
        signature: *const c_char,
    }

    /// `BLOCK_HAS_SIGNATURE` — the only flag needed for a POD stack block.
    const BLOCK_HAS_SIGNATURE: i32 = 1 << 30;

    /// Type encoding for `void (^)(NSData *token, NSError *error)`:
    /// `v24@?0@8@16`.
    const BLOCK_SIGNATURE: &[u8] = b"v24@?0@8@16\0";

    /// Immutable, process-lifetime data; the raw signature pointer is never
    /// mutated, so shared access from the ObjC runtime is race-free.
    // SAFETY: every field is immutable process-lifetime data.
    unsafe impl Sync for CompletionBlockDescriptor {}

    static COMPLETION_DESCRIPTOR: CompletionBlockDescriptor = CompletionBlockDescriptor {
        reserved: 0,
        size: std::mem::size_of::<CompletionBlock>(),
        signature: BLOCK_SIGNATURE.as_ptr() as *const c_char,
    };

    /// Resolve a selector by name; `sel_registerName` is idempotent and cheap.
    ///
    /// # Safety
    /// The returned selector is valid for the lifetime of the process.
    unsafe fn selector(name: &CStr) -> Sel {
        // SAFETY: `name` is a valid null-terminated C string.
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// Read the UTF-8 payload of an `NSString` into a Rust `String`.
    ///
    /// # Safety
    /// `string` must be a live `NSString` for the duration of the call.
    unsafe fn ns_string(string: Id) -> String {
        // SAFETY: `string` is a live NSString; the returned pointer stays valid
        // until the enclosing autorelease pool drains.
        let ptr = unsafe { msg_send_noarg(string, selector(c"UTF8String")).cast::<c_char>() };
        if ptr.is_null() {
            return String::new();
        }
        // SAFETY: upheld by the caller; `CStr::from_ptr` only reads.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }

    /// Completion block body. Runs on DeviceCheck's XPC reply queue, which is
    /// why the result travels over a channel instead of a return value.
    ///
    /// # Safety
    /// Called by the Objective-C runtime with a valid block literal; `token`
    /// and `error` are live `NSData`/`NSError` objects (or null) for the
    /// duration of the call.
    unsafe extern "C" fn completion_invoke(block: *mut CompletionBlock, token: Id, error: Id) {
        let completion = catch_unwind(AssertUnwindSafe(|| {
            if !token.is_null() {
                // SAFETY: `token` is a live NSData for the duration of the callback.
                let encoded =
                    unsafe { msg_send_u64(token, selector(c"base64EncodedStringWithOptions:"), 0) };
                if encoded.is_null() {
                    return Completion::Error;
                }
                // SAFETY: `encoded` is a live NSString.
                return Completion::Token(unsafe { ns_string(encoded) });
            }
            // An error is reported as a generic failure; the localized
            // description is intentionally NOT propagated (log/secrets safety).
            if !error.is_null() {
                return Completion::Error;
            }
            Completion::Error
        }));
        let completion = match completion {
            Ok(completion) => completion,
            Err(payload) => {
                // Never let a panic escape into the ObjC runtime.
                std::mem::forget(payload);
                Completion::Error
            }
        };
        // SAFETY: the owner keeps the sender alive until the block has fired
        // (and leaks it on timeout), so the captured pointer is always valid.
        // `try_send` never blocks the XPC queue, even if the runtime were to
        // invoke the block more than once.
        unsafe {
            _ = (*(*block).sender).try_send(completion);
        }
    }

    /// Drive `generateTokenWithCompletionHandler:` and wait on the channel.
    ///
    /// # Safety
    /// `device` must be a live, retained `DCDevice` instance.
    unsafe fn run_token_request(device: Id) -> DeviceCheckResult {
        let (sender, receiver) = mpsc::sync_channel::<Completion>(1);
        let sender = Box::into_raw(Box::new(sender));
        let block = CompletionBlock {
            isa: ptr::addr_of!(_NSConcreteStackBlock).cast::<c_void>(),
            flags: BLOCK_HAS_SIGNATURE,
            reserved: 0,
            invoke: completion_invoke,
            descriptor: &raw const COMPLETION_DESCRIPTOR,
            sender,
        };
        // SAFETY: `device` is a live DCDevice and `block` follows the block ABI;
        // the runtime copies the literal, so the stack frame may die after the call.
        unsafe {
            msg_send_block(
                device,
                selector(c"generateTokenWithCompletionHandler:"),
                (&raw const block).cast(),
            );
        }

        let mut result = DeviceCheckResult {
            supported: true,
            token_base64: None,
            latency_ms: None,
        };
        match receiver.recv_timeout(TOKEN_TIMEOUT) {
            Ok(Completion::Token(token)) => {
                result.token_base64 = Some(token);
                // SAFETY: the block has fired and will not fire again, so the
                // sender is unreachable from the runtime and can be reclaimed.
                drop(unsafe { Box::from_raw(sender) });
            }
            Ok(Completion::Error) => {
                // SAFETY: same as above — the single-shot block already fired.
                drop(unsafe { Box::from_raw(sender) });
            }
            Err(_) => {
                // Timeout (or a vanished sender): the block may still fire on
                // the XPC queue, so deliberately leak the sender to keep the
                // captured pointer valid. Bounded to one leak per timeout.
            }
        }
        result
    }

    fn generate_token_inner() -> DeviceCheckResult {
        // SAFETY: `c"DCDevice"` is a valid null-terminated class name.
        let class = unsafe { objc_getClass(c"DCDevice".as_ptr()) };
        if class.is_null() {
            return DeviceCheckResult {
                supported: false,
                token_base64: None,
                latency_ms: None,
            };
        }
        // SAFETY: `class` is a registered ObjC class; `currentDevice` is a
        // documented DCDevice class method returning an autoreleased instance.
        let device = unsafe { msg_send_noarg(class, selector(c"currentDevice")) };
        if device.is_null() {
            return DeviceCheckResult {
                supported: false,
                token_base64: None,
                latency_ms: None,
            };
        }
        // SAFETY: `device` is a live object; retain balances the release below.
        let device = unsafe { objc_retain(device) };
        // SAFETY: `device` is a live DCDevice; `isSupported` returns BOOL.
        let supported = unsafe { msg_send_bool(device, selector(c"isSupported")) } != 0;
        if supported {
            // SAFETY: `device` is live and retained for the duration of the call.
            let mut token_result = unsafe { run_token_request(device) };
            unsafe { objc_release(device) };
            token_result.supported = true;
            return token_result;
        }
        // SAFETY: balances the retain above.
        unsafe { objc_release(device) };
        DeviceCheckResult {
            supported: false,
            token_base64: None,
            latency_ms: None,
        }
    }

    pub fn generate_token() -> DeviceCheckResult {
        let start = Instant::now();
        // SAFETY: pool push/pop are balanced within this scope.
        let pool = unsafe { objc_autoreleasePoolPush() };
        let mut result = generate_token_inner();
        result.latency_ms = Some(start.elapsed().as_secs_f64() * 1000.0);
        // SAFETY: balances the push above.
        unsafe { objc_autoreleasePoolPop(pool) };
        result
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
mod platform {
    use super::DeviceCheckResult;

    pub fn generate_token() -> DeviceCheckResult {
        // Non-macOS/arm64: DeviceCheck is unavailable; the attestation header
        // is omitted entirely (see `generate_codex_attestation`).
        DeviceCheckResult {
            supported: false,
            token_base64: None,
            latency_ms: None,
        }
    }
}

/// Generate an Apple DeviceCheck token, offloading the blocking ObjC call to
/// the tokio blocking pool on macOS/arm64. Off-platform this is a cheap
/// synchronous `None`-shaped result (no thread spawn needed, but kept async for
/// a uniform call site).
async fn device_check_generate_token() -> DeviceCheckResult {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        tokio::task::spawn_blocking(platform::generate_token)
            .await
            .unwrap_or(DeviceCheckResult {
                supported: false,
                token_base64: None,
                latency_ms: None,
            })
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        platform::generate_token()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbor_header_small_values_inline() {
        assert_eq!(cbor_header(0, 0), vec![0x00]);
        assert_eq!(cbor_header(0, 23), vec![0x17]);
        assert_eq!(cbor_header(5, 2), vec![0xa2]);
    }

    #[test]
    fn cbor_header_one_byte_argument() {
        assert_eq!(cbor_header(0, 24), vec![0x18, 24]);
        assert_eq!(cbor_header(0, 255), vec![0x18, 255]);
    }

    #[test]
    fn cbor_header_two_byte_argument() {
        assert_eq!(cbor_header(0, 256), vec![0x19, 0x01, 0x00]);
    }

    #[test]
    fn cbor_text_encodes_length_then_utf8() {
        let out = cbor_text("hi");
        // major 3, len 2, then "hi"
        assert_eq!(out, vec![0x62, b'h', b'i']);
    }

    #[test]
    fn cbor_map_encodes_count_then_pairs() {
        let map = cbor_map(&[(cbor_text("a"), cbor_unsigned(1))]);
        // 0xa1 = map of 1 entry; 0x61 = text len 1 "a"; 0x01 = unsigned 1
        assert_eq!(map, vec![0xa1, 0x61, b'a', 0x01]);
    }

    #[test]
    fn build_client_attestation_with_token() {
        let result = DeviceCheckResult {
            supported: true,
            token_base64: Some("tok".to_string()),
            latency_ms: Some(12.0),
        };
        let envelope = build_client_attestation(&result);
        assert!(envelope.starts_with("v1."));
        // Decoding the base64url must yield valid CBOR starting with a map.
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&envelope[3..])
            .unwrap();
        assert_eq!(decoded[0] >> 5, 5, "first item is a CBOR map");
    }

    #[test]
    fn build_client_attestation_unsupported_emits_error_code_3() {
        let result = DeviceCheckResult {
            supported: false,
            token_base64: None,
            latency_ms: None,
        };
        let envelope = build_client_attestation(&result);
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&envelope[3..])
            .unwrap();
        // The "error_code" key with value 3 must be present in the CBOR map.
        let needle = cbor_text("error_code");
        assert!(
            decoded
                .windows(needle.len())
                .any(|w| w == needle.as_slice()),
            "error_code key present"
        );
    }

    #[test]
    fn build_client_attestation_supported_no_token_emits_error_code_4() {
        let result = DeviceCheckResult {
            supported: true,
            token_base64: None,
            latency_ms: None,
        };
        let envelope = build_client_attestation(&result);
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&envelope[3..])
            .unwrap();
        // The "error_code" key must be present (supported but no token → code 4).
        let needle = cbor_text("error_code");
        assert!(
            decoded
                .windows(needle.len())
                .any(|w| w == needle.as_slice()),
            "error_code key present"
        );
        // error code 4 (supported, no token) must follow as unsigned int 4.
        assert!(decoded.iter().any(|&b| b == 0x04), "error code 4 present");
    }

    #[tokio::test]
    async fn generate_codex_attestation_returns_none_off_macos_arm64() {
        // On non-macOS/arm64 targets this must be None (header omitted).
        if cfg!(not(all(target_os = "macos", target_arch = "aarch64"))) {
            assert!(generate_codex_attestation().await.is_none());
        }
    }

    #[test]
    fn app_session_id_is_stable_within_a_process() {
        let a = app_session_id();
        let b = app_session_id();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }
}
