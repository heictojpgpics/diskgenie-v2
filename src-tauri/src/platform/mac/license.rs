//! License platform surface: app data dir, machine GUID, drive
//! serial, CPUID brand, Keychain, hardlink identity.

use super::dir::statfs_of;
use std::ffi::{c_void, CString};

use super::ffi::{
    cf_boolean_true, sysctlbyname, CFDataCreate, CFDataGetBytePtr, CFDataGetLength,
    CFDictionaryAddValue, CFDictionaryCreateMutable, CFStringCreateWithCString, IOObjectRelease,
    IORegistryEntryCreateCFProperty, IOServiceGetMatchingService, IOServiceMatching, SecItemAdd,
    SecItemCopyMatching, SecItemDelete,
};
use super::objc::{cf_release, cf_string_to_string};

// ============================================================================
// License platform surface (the win.rs M10 section mirrored)
// ============================================================================

/// Per-user app-support directory for DiskGenie
/// (`~/Library/Application Support/DiskGenie`, created on demand). `.` when
/// `HOME` is unset (test runners).
#[must_use]
pub fn app_data_dir() -> std::path::PathBuf {
    let base = std::env::var("HOME").map_or_else(
        |_| std::path::PathBuf::from("."),
        |h| {
            std::path::PathBuf::from(h)
                .join("Library")
                .join("Application Support")
        },
    );
    let dir = base.join("DiskGenie");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// MachineGuid analogue: IOPlatformUUID (IOKit).
#[must_use]
pub fn machine_guid() -> Option<String> {
    unsafe {
        let name = CString::new("IOPlatformExpertDevice").ok()?;
        // SAFETY: IOServiceMatching returns a CF dictionary consumed by
        // IOServiceGetMatchingService.
        let matching = IOServiceMatching(name.as_ptr());
        if matching.is_null() {
            return None;
        }
        // SAFETY: matching consumed by the lookup (release-on-consume).
        // First argument is kIOMainPortDefault (0) — the master-port
        // parameter is ignored on macOS 12+ and 0 is the documented
        // default (mach_host_self would leak a send right).
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        // SAFETY: Create-rule CFString key over a C literal (NUL
        // included by the c"…" syntax).
        let key =
            CFStringCreateWithCString(std::ptr::null(), c"IOPlatformUUID".as_ptr(), 0x0800_0100);
        if key.is_null() {
            // SAFETY: release the service on the failure path.
            let _ = IOObjectRelease(service);
            return None;
        }
        // SAFETY: property from the live service object.
        let uuid_cf = IORegistryEntryCreateCFProperty(service, key, std::ptr::null(), 0);
        // SAFETY: release the key after the query.
        cf_release(key);
        let _ = IOObjectRelease(service);
        if uuid_cf.is_null() {
            return None;
        }
        let s = cf_string_to_string(uuid_cf);
        // SAFETY: release the property object.
        cf_release(uuid_cf);
        s
    }
}

/// System-drive serial analogue: the root volume's fsid as u32.
#[must_use]
pub fn system_drive_serial() -> Option<u32> {
    let c = CString::new("/").ok()?;
    statfs_of(&c).map(|st| st.f_fsid[0])
}

/// CPU brand via sysctl.
#[must_use]
pub fn cpuid_brand() -> Option<String> {
    sysctl_string("machdep.cpu.brand_string")
}

/// The machine's hostname via `kern.hostname` sysctl (v2 device
/// fact). GUI-launched macOS apps do NOT inherit the shell's
/// `HOSTNAME` variable (it is a shell-computed value, not exported),
/// so the v1 env-var lookup silently fell back to "Mac" on every
/// real activation — the sysctl is the kernel's own value.
#[must_use]
pub fn hostname() -> Option<String> {
    sysctl_string("kern.hostname").map(|h| h.split('.').next().unwrap_or(&h).to_string())
}

/// Hardware model identifier via `hw.model` (v2 device fact) — e.g.
/// "MacBookPro18,3", "Macmini9,1".
#[must_use]
pub fn machine_model() -> Option<String> {
    sysctl_string("hw.model")
}

/// Total physical memory in MB via `hw.memsize` (bytes) (v2 device
/// fact).
#[must_use]
pub fn ram_mb() -> Option<u64> {
    use super::ffi::sysctlbyname;
    let Ok(name) = CString::new("hw.memsize") else {
        return None;
    };
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: sysctlbyname into a fixed u64 slot.
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr(),
            &mut value as *mut u64 as *mut core::ffi::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value / (1024 * 1024))
}

/// Shared sysctl string reader (kern./hw. leaf names — small values).
fn sysctl_string(name: &str) -> Option<String> {
    use super::ffi::sysctlbyname;
    let Ok(c) = CString::new(name) else {
        return None;
    };
    let mut buf = [0u8; 128];
    let mut len = buf.len();
    // SAFETY: sysctlbyname into a fixed buffer.
    let rc = unsafe {
        sysctlbyname(
            c.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let end = buf[..len.min(buf.len())]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(len);
    let s = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Keychain persistence (the DPAPI analogue).
// ─────────────────────────────────────────────────────────────────────

const KEYCHAIN_SERVICE: &str = "app.diskgenie";
const KEYCHAIN_ACCOUNT: &str = "license-state";

fn cf_key(name: &str) -> *mut c_void {
    // SAFETY: Create-rule CFString over a PROPERLY NUL-terminated
    // CString — the old `name.as_ptr().cast()` handed a bare &str
    // pointer to a C-string API (an out-of-bounds read that only
    // worked because rodata literals happened to sit before zeros;
    // caught while lint-hardening the mac platform).
    let Ok(c) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), 0x0800_0100) }
}

/// Build the SecItem attributes/query dictionary.
fn keychain_dict(
    service: *const c_void,
    account: *const c_void,
    data: *const c_void,
) -> *mut c_void {
    // SAFETY: CFDictionaryCreateMutable + AddValue with Create-rule keys.
    unsafe {
        let class = cf_key("kSecClass");
        let gen_pass = cf_key("kSecClassGenericPassword");
        let service_key = cf_key("kSecAttrService");
        let account_key = cf_key("kSecAttrAccount");
        let d = CFDictionaryCreateMutable(std::ptr::null(), 0, std::ptr::null(), std::ptr::null());
        CFDictionaryAddValue(d, class, gen_pass);
        CFDictionaryAddValue(d, service_key, service);
        CFDictionaryAddValue(d, account_key, account);
        if !data.is_null() {
            let data_key = cf_key("kSecValueData");
            CFDictionaryAddValue(d, data_key, data);
            cf_release(data_key);
        }
        cf_release(class);
        cf_release(gen_pass);
        cf_release(service_key);
        cf_release(account_key);
        d
    }
}

/// Run a Keychain FFI call on a worker thread under a hard wall-clock
/// bound. A locked keychain (or a headless session whose Security agent
/// cannot show its prompt) makes `SecItem*` block INDEFINITELY — the
/// licensing commands must answer the user even then. Real Keychain
/// operations complete in single-digit milliseconds, so 10 s is >1000x
/// headroom; on the timeout the stuck thread is abandoned (a one-thread
/// leak in an already-pathological session beats an eternal spinner).
fn with_keychain_watchdog<T>(op: impl FnOnce() -> T + Send + 'static) -> Result<T, String>
where
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("keychain-op".into())
        .spawn(move || {
            // Send best-effort: a receiver drop (timeout) must not panic
            // the worker thread.
            let _ = tx.send(op());
        })
        .map_err(|e| format!("Keychain worker spawn failed ({e})"))?;
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "Keychain did not respond within 10 s (locked or unavailable)".to_string())
}

/// Encrypt-and-store (a Keychain "generic password" item; the Keychain
/// itself provides the confidentiality DPAPI gives on Windows).
/// Watchdog-bounded: a blocked Security agent returns an error, never a
/// hang (the CI runner's login keychain blocks exactly this way).
///
/// # Errors
/// When the Keychain refuses the upsert (status != 0) or never answers.
pub fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    let payload = data.to_vec();
    with_keychain_watchdog(move || {
        // SAFETY: the FFI sequence below is the whole closure body; all
        // Create-rule objects are released on every path.
        unsafe {
            let service = cf_key(KEYCHAIN_SERVICE);
            let account = cf_key(KEYCHAIN_ACCOUNT);
            // SAFETY: Create-rule CFData over the owned payload.
            let cf_data = CFDataCreate(std::ptr::null(), payload.as_ptr(), payload.len() as isize);
            // Replace any existing item first (idempotent upsert).
            let del_query = keychain_dict(service, account, std::ptr::null());
            SecItemDelete(del_query);
            cf_release(del_query);
            let add_attrs = keychain_dict(service, account, cf_data);
            let status = SecItemAdd(add_attrs, std::ptr::null_mut());
            cf_release(add_attrs);
            cf_release(cf_data);
            cf_release(service);
            cf_release(account);
            if status != 0 {
                return Err(format!("Keychain write failed (status {status})"));
            }
            // The stored form is the raw payload (the Keychain is the
            // protection); return it so callers keep one byte contract.
            Ok(payload)
        }
    })
    .and_then(std::convert::identity)
}

/// Fetch-and-decrypt from the Keychain. Watchdog-bounded like the
/// write path: a locked keychain at boot must fail fast (the app runs
/// unlicensed and re-validates) instead of stalling the window.
///
/// # Errors
/// When the Keychain holds no item, returns no data, or never answers.
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    // `data` is the caller's fallback blob; when the Keychain holds the
    // item it wins (that IS the persisted state).
    let _ = data;
    with_keychain_watchdog(|| {
        // SAFETY: same FFI lifetime discipline as the write path; every
        // Create-rule object is released on every path.
        unsafe {
            let service = cf_key(KEYCHAIN_SERVICE);
            let account = cf_key(KEYCHAIN_ACCOUNT);
            let query = keychain_dict(service, account, std::ptr::null());
            let return_data_key = cf_key("kSecReturnData");
            // SAFETY: kCFBooleanTrue singleton (immutable).
            let yes = cf_boolean_true() as *mut c_void;
            CFDictionaryAddValue(query, return_data_key, yes);
            let mut result: *const c_void = std::ptr::null();
            let status = SecItemCopyMatching(query, &mut result);
            cf_release(query);
            cf_release(return_data_key);
            cf_release(service);
            cf_release(account);
            if status != 0 || result.is_null() {
                return Err("License state not found in the Keychain".into());
            }
            // SAFETY: CFData accessors on the returned item.
            let len = unsafe { CFDataGetLength(result) };
            let ptr = unsafe { CFDataGetBytePtr(result) };
            let out = if len > 0 {
                // SAFETY: ptr valid for len bytes per the CFData contract.
                unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec()
            } else {
                Vec::new()
            };
            // SAFETY: release the returned item.
            cf_release(result);
            Ok(out)
        }
    })
    .and_then(std::convert::identity)
}

/// POSIX stat() hardlink identity (st_dev, st_ino) — the dupes
/// exclusion on macOS comes free from the filesystem.
#[must_use] // parity with win.rs; on macOS the only caller (dupes) is cfg(windows)
#[allow(dead_code)] // API-parity stub: win.rs's consumer is Windows-gated (mac hardlink
                    // dedup is a backlog item — see docs/LEARNINGS-BACKLOG.md)
pub fn hardlink_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).ok()?;
    Some((md.dev(), md.ino()))
}

/// Same as [`hardlink_identity`] but accepts an ALREADY-OPEN handle
/// (the dupes engine's verify pass has one positioned past the
/// screens — reusing it saves the THIRD `File::open` per verified
/// member that the v2 caller used to do).
pub fn hardlink_identity_for_handle(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = file.metadata().ok()?;
    Some((md.dev(), md.ino()))
}

/// macOS product version for the license device facts (audit field;
/// `kern.osproductversion` via the sysctl seam — same call shape as
/// [`cpuid_brand`]).
#[must_use]
pub fn os_version() -> String {
    let Ok(name) = CString::new("kern.osproductversion") else {
        return "macOS".to_string();
    };
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    // SAFETY: sysctlbyname into a fixed buffer.
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && len > 0 && len <= buf.len() {
        let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
        let version = String::from_utf8_lossy(&buf[..end]).to_string();
        if version.is_empty() {
            "macOS".to_string()
        } else {
            format!("macOS {version}")
        }
    } else {
        "macOS".to_string()
    }
}

// ============================================================================
// v3 device facts: platform serial / firmware UUID / cores / arch
// ============================================================================

/// Read one string property off the IOPlatformExpertDevice service
/// (the root of the platform device tree — machine serial, platform
/// UUID, firmware build all live here). Shared by
/// [`machine_guid`]/[`baseboard_serial`]/[`firmware_uuid`].
fn iokit_platform_property(key: &[u8]) -> Option<String> {
    // `key` must be NUL-terminated for CFStringCreateWithCString.
    let mut owned = Vec::with_capacity(key.len() + 1);
    owned.extend_from_slice(key);
    owned.push(0);
    unsafe {
        let name = CString::new("IOPlatformExpertDevice").ok()?;
        // SAFETY: IOServiceMatching returns a CF dictionary consumed by
        // IOServiceGetMatchingService.
        let matching = IOServiceMatching(name.as_ptr());
        if matching.is_null() {
            return None;
        }
        // SAFETY: matching consumed by the lookup (release-on-consume);
        // port 0 = kIOMainPortDefault (macOS 12+).
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        // SAFETY: create-rule CFString over the NUL-terminated key
        // (the FFI takes `*const c_char` — a signed-char pointer).
        let key_cf = CFStringCreateWithCString(
            std::ptr::null(),
            owned.as_ptr().cast::<std::os::raw::c_char>(),
            0x0800_0100, // kCFStringEncodingUTF8
        );
        if key_cf.is_null() {
            // SAFETY: release the service on the failure path.
            let _ = IOObjectRelease(service);
            return None;
        }
        // SAFETY: property read from the live service object.
        let prop = IORegistryEntryCreateCFProperty(service, key_cf, std::ptr::null(), 0);
        // SAFETY: release the key after the query.
        cf_release(key_cf);
        let _ = IOObjectRelease(service);
        if prop.is_null() {
            return None;
        }
        let s = cf_string_to_string(prop);
        // SAFETY: release the property object.
        cf_release(prop);
        s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }
}

/// SMBIOS baseboard-serial analogue (v3 device fact):
/// IOPlatformSerialNumber — the Mac's chassis serial etched at
/// manufacture. Survives reinstalls and drive swaps; changes only with
/// a logic-board replacement.
#[must_use]
pub fn baseboard_serial() -> Option<String> {
    iokit_platform_property(b"IOPlatformSerialNumber")
}

/// Firmware UUID (v3 device fact): IOPlatformUUID — the same identity
/// [`machine_guid`] hashes, exposed in its raw canonical form for the
/// server's component forensics (a changed UUID with a stable serial
/// is a logic-board swap; both changed = different machine).
#[must_use]
pub fn firmware_uuid() -> Option<String> {
    iokit_platform_property(b"IOPlatformUUID")
}

/// Firmware build string (v3 device fact, display-only): Apple exposes
/// it as the `firmware-version` IORegistry string on the platform
/// device ("2091.40.5.0.0", "iBoot:1234.56.7"). `None` when absent.
#[must_use]
pub fn firmware_version() -> Option<String> {
    iokit_platform_property(b"firmware-version")
}

/// Logical CPU count (v3 device fact) via `hw.logicalcpu`.
#[must_use]
pub fn cpu_cores() -> Option<u32> {
    use super::ffi::sysctlbyname;
    let Ok(name) = CString::new("hw.logicalcpu") else {
        return None;
    };
    let mut value: u32 = 0;
    let mut len = std::mem::size_of::<u32>();
    // SAFETY: sysctlbyname into a fixed u32 slot.
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr(),
            &mut value as *mut u32 as *mut core::ffi::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && value > 0).then_some(value)
}

/// CPU architecture (v3 device fact): `std::env::consts::ARCH`
/// ("x86_64" on Intel / "aarch64" on Apple Silicon).
#[must_use]
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}
