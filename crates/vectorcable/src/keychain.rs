//! Bridge app tokens (bridge roadmap step 1.8) in the macOS login Keychain:
//! one generic password per bridge, its account the bridge's `device_id`.
//!
//! The item's access list trusts the binary that created it, so after an
//! update macOS asks once whether the new `vectorcable` may read it.

use std::ffi::{c_char, c_void};
use std::ptr;

use vbcore::pin::TokenStore;

type CfTypeRef = *const c_void;
type OsStatus = i32;

const SERVICE: &str = "VectorCable bridge token";
const ERR_SEC_DUPLICATE_ITEM: OsStatus = -25299;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[repr(C)]
struct DictionaryKeyCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
}

#[repr(C)]
struct DictionaryValueCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: DictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: DictionaryValueCallBacks;
    static kCFBooleanTrue: CfTypeRef;
    fn CFDictionaryCreate(
        alloc: CfTypeRef,
        keys: *const CfTypeRef,
        values: *const CfTypeRef,
        count: isize,
        key_callbacks: *const DictionaryKeyCallBacks,
        value_callbacks: *const DictionaryValueCallBacks,
    ) -> CfTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CfTypeRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external: u8,
    ) -> CfTypeRef;
    fn CFStringGetCString(cf: CfTypeRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFDataCreate(alloc: CfTypeRef, bytes: *const u8, len: isize) -> CfTypeRef;
    fn CFDataGetBytePtr(data: CfTypeRef) -> *const u8;
    fn CFDataGetLength(data: CfTypeRef) -> isize;
    fn CFDataGetTypeID() -> usize;
    fn CFGetTypeID(cf: CfTypeRef) -> usize;
    fn CFRelease(cf: CfTypeRef);
}

#[link(name = "Security", kind = "framework")]
extern "C" {
    static kSecClass: CfTypeRef;
    static kSecClassGenericPassword: CfTypeRef;
    static kSecAttrService: CfTypeRef;
    static kSecAttrAccount: CfTypeRef;
    static kSecAttrLabel: CfTypeRef;
    static kSecValueData: CfTypeRef;
    static kSecReturnData: CfTypeRef;
    static kSecMatchLimit: CfTypeRef;
    static kSecMatchLimitOne: CfTypeRef;
    fn SecItemCopyMatching(query: CfTypeRef, result: *mut CfTypeRef) -> OsStatus;
    fn SecItemAdd(attributes: CfTypeRef, result: *mut CfTypeRef) -> OsStatus;
    fn SecItemUpdate(query: CfTypeRef, changes: CfTypeRef) -> OsStatus;
    fn SecItemDelete(query: CfTypeRef) -> OsStatus;
    fn SecCopyErrorMessageString(status: OsStatus, reserved: *const c_void) -> CfTypeRef;
}

/// A CoreFoundation object we own, released on drop.
struct Cf(CfTypeRef);

impl Cf {
    fn new(cf: CfTypeRef) -> Result<Cf, String> {
        if cf.is_null() {
            Err("out of memory".into())
        } else {
            Ok(Cf(cf))
        }
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: a non-null object from a Create or Copy call, released once.
        unsafe { CFRelease(self.0) }
    }
}

fn cf_string(s: &str) -> Result<Cf, String> {
    // SAFETY: the bytes outlive the call, which copies them.
    Cf::new(unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            s.as_ptr(),
            s.len() as isize,
            CF_STRING_ENCODING_UTF8,
            0,
        )
    })
}

fn cf_data(bytes: &[u8]) -> Result<Cf, String> {
    // SAFETY: as for `cf_string`.
    Cf::new(unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as isize) })
}

/// A dictionary of CF keys and values; it retains them, so the caller's
/// objects may be released afterwards.
fn dictionary(pairs: &[(CfTypeRef, CfTypeRef)]) -> Result<Cf, String> {
    let keys: Vec<CfTypeRef> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<CfTypeRef> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: both arrays hold `pairs.len()` live, non-null CF objects, and the
    // callbacks are CoreFoundation's own for CF types.
    Cf::new(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    })
}

/// The Security framework's words for `status`.
fn error(status: OsStatus) -> String {
    // SAFETY: a Copy call; the string is released by `Cf` and read into a
    // buffer of the size passed.
    unsafe {
        if let Ok(text) = Cf::new(SecCopyErrorMessageString(status, ptr::null())) {
            let mut buf = [0 as c_char; 256];
            if CFStringGetCString(
                text.0,
                buf.as_mut_ptr(),
                buf.len() as isize,
                CF_STRING_ENCODING_UTF8,
            ) != 0
            {
                let words = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy();
                return format!("{words} ({status})");
            }
        }
    }
    format!("Keychain error {status}")
}

/// The login Keychain, under one service name.
#[derive(Debug)]
pub struct Keychain {
    service: &'static str,
}

impl Keychain {
    pub fn new() -> Keychain {
        Keychain { service: SERVICE }
    }

    /// The item for `device_id`, plus `extra` pairs.
    fn query(&self, device_id: &str, extra: &[(CfTypeRef, CfTypeRef)]) -> Result<Cf, String> {
        let service = cf_string(self.service)?;
        let account = cf_string(device_id)?;
        // SAFETY: reading the Security framework's constant keys.
        let mut pairs = unsafe {
            vec![
                (kSecClass, kSecClassGenericPassword),
                (kSecAttrService, service.0),
                (kSecAttrAccount, account.0),
            ]
        };
        pairs.extend_from_slice(extra);
        dictionary(&pairs)
    }
}

impl TokenStore for Keychain {
    fn load(&self, device_id: &str) -> Option<String> {
        // SAFETY: constant keys; `found` is owned by `Cf` and checked to be
        // CFData before its bytes are read, within its length.
        unsafe {
            let query = self
                .query(
                    device_id,
                    &[
                        (kSecReturnData, kCFBooleanTrue),
                        (kSecMatchLimit, kSecMatchLimitOne),
                    ],
                )
                .ok()?;
            let mut found: CfTypeRef = ptr::null();
            let status = SecItemCopyMatching(query.0, &mut found);
            let found = Cf::new(found).ok()?;
            if status != 0 || CFGetTypeID(found.0) != CFDataGetTypeID() {
                return None;
            }
            let len = usize::try_from(CFDataGetLength(found.0)).ok()?;
            let bytes = CFDataGetBytePtr(found.0);
            if bytes.is_null() || len == 0 {
                return None;
            }
            String::from_utf8(std::slice::from_raw_parts(bytes, len).to_vec()).ok()
        }
    }

    fn save(&self, device_id: &str, token: &str) -> Result<(), String> {
        let data = cf_data(token.as_bytes())?;
        let label = cf_string(self.service)?;
        // SAFETY: constant keys and live dictionaries.
        unsafe {
            let item = self.query(
                device_id,
                &[(kSecValueData, data.0), (kSecAttrLabel, label.0)],
            )?;
            match SecItemAdd(item.0, ptr::null_mut()) {
                0 => Ok(()),
                ERR_SEC_DUPLICATE_ITEM => {
                    let query = self.query(device_id, &[])?;
                    let changes = dictionary(&[(kSecValueData, data.0)])?;
                    match SecItemUpdate(query.0, changes.0) {
                        0 => Ok(()),
                        status => Err(error(status)),
                    }
                }
                status => Err(error(status)),
            }
        }
    }

    fn remove(&self, device_id: &str) {
        if let Ok(query) = self.query(device_id, &[]) {
            // SAFETY: a live query dictionary.
            unsafe { SecItemDelete(query.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes, rewrites, reads and deletes one item under a test service name.
    #[test]
    #[ignore = "touches the login Keychain"]
    fn a_token_round_trips_through_the_keychain() {
        let k = Keychain {
            service: "VectorCable test token",
        };
        let device = "ffffffffffffffff";
        k.remove(device);
        assert_eq!(k.load(device), None);
        k.save(device, "0f0e0d0c0b0a09080706050403020100").unwrap();
        assert_eq!(
            k.load(device).as_deref(),
            Some("0f0e0d0c0b0a09080706050403020100")
        );
        k.save(device, "ffeeddccbbaa99887766554433221100").unwrap();
        assert_eq!(
            k.load(device).as_deref(),
            Some("ffeeddccbbaa99887766554433221100")
        );
        assert_eq!(k.load("eeeeeeeeeeeeeeee"), None);
        k.remove(device);
        assert_eq!(k.load(device), None);
    }
}
