//! License platform surface: app data dir, machine GUID, drive
//! serial, CPUID brand, DPAPI protection, hardlink identity, and the
//! v2 device facts (hostname, RAM, machine model).

use windows::core::PCWSTR;

use super::wide;

// ============================================================================
// M10: License platform surface (doc 06; licensing doc §3/§5.4)
// ============================================================================

/// Per-user roaming app-data directory for DiskGenie
/// (`%APPDATA%\DiskGenie`, created on demand). `.` when `APPDATA` is
/// unset (test runners, portable mode).
#[must_use]
pub fn app_data_dir() -> std::path::PathBuf {
    let base = std::env::var("APPDATA")
        .map_or_else(|_| std::path::PathBuf::from("."), std::path::PathBuf::from);
    let dir = base.join("DiskGenie");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The machine's DNS hostname via `GetComputerNameExW` (v2 device
/// fact). The env-var fallback the v1 facts used misses GUI-launched
/// processes with scrubbed environments; the API is the reliable
/// source (and is what `hostname`/`ipconfig` report). `None` only if
/// the API itself fails (then the caller falls back to env → "PC").
#[must_use]
pub fn hostname() -> Option<String> {
    use windows::Win32::System::SystemInformation::GetComputerNameExW;
    use windows::Win32::System::SystemInformation::COMPUTER_NAME_FORMAT;
    let format = COMPUTER_NAME_FORMAT(1); // ComputerNameDnsHostname
    let mut buf = [0u16; 64];
    let mut len = u32::try_from(buf.len()).unwrap_or(64);
    // SAFETY: properly sized wide buffer + valid out-len per contract.
    let ok = unsafe {
        GetComputerNameExW(
            format,
            Some(windows::core::PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
    };
    if ok.is_err() {
        return None;
    }
    let slice = &buf[..(len as usize).min(buf.len())];
    let name = String::from_utf16_lossy(slice);
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Total physical memory in MB via `GlobalMemoryStatusEx` (v2 device
/// fact). `None` when the API fails.
#[must_use]
pub fn ram_mb() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).unwrap_or(0),
        ..MEMORYSTATUSEX::default()
    };
    // SAFETY: properly sized out-struct per the API contract.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    ok.is_ok().then_some(status.ullTotalPhys / (1024 * 1024))
}

/// SMBIOS machine identity for display (v2 device fact):
/// `HKLM\SYSTEM\CurrentControlSet\Control\SystemInformation` →
/// `SystemManufacturer` + `SystemModel` — the same source
/// `Win32_ComputerSystem` reports, read straight from the registry
/// (no WMI round trip; WMI is slow and flaky under low-privilege
/// service hobbles). `None` when both values are missing.
#[must_use]
pub fn machine_model() -> Option<String> {
    let manufacturer = reg_sz(
        r"SYSTEM\CurrentControlSet\Control\SystemInformation",
        "SystemManufacturer",
    )
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    let model = reg_sz(
        r"SYSTEM\CurrentControlSet\Control\SystemInformation",
        "SystemModel",
    )
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    match (manufacturer, model) {
        (Some(m), Some(p)) => Some(format!("{m} {p}")),
        (Some(m), None) => Some(m),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    }
}

/// One REG_SZ read from HKLM (shared by machine_model; the MachineGuid
/// path keeps its own specialized reader for its narrower contract).
fn reg_sz(sub_key: &str, value: &str) -> Option<String> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
        REG_VALUE_TYPE,
    };
    let sub = wide(sub_key);
    let mut hk = HKEY::default();
    // SAFETY: NUL-terminated subkey; out-handle slot valid.
    let open = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            None,
            KEY_READ,
            &mut hk,
        )
    };
    if !open.is_ok() {
        return None;
    }
    let name = wide(value);
    let mut ty = REG_VALUE_TYPE::default();
    let mut len = 0u32;
    // SAFETY: size probe.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut len),
        )
    };
    if err.is_err() || ty != REG_SZ || len == 0 {
        let _ = unsafe { RegCloseKey(hk) };
        return None;
    }
    let mut buf = vec![0u16; len as usize / 2 + 1];
    let mut len2 = len;
    // SAFETY: buffer covers the reported size.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut len2),
        )
    };
    let _ = unsafe { RegCloseKey(hk) };
    if err.is_err() {
        return None;
    }
    let raw = &buf[..len2 as usize / 2];
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Some(String::from_utf16_lossy(&raw[..end]))
}

/// `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` (hardware binding
/// input; doc 06 §3.5). `None` when unreadable.
pub fn machine_guid() -> Option<String> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
        REG_VALUE_TYPE,
    };
    let sub = wide(r"SOFTWARE\Microsoft\Cryptography");
    let mut hk = HKEY::default();
    // SAFETY: NUL-terminated subkey; out-handle slot valid.
    let open = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            None,
            KEY_READ,
            &mut hk,
        )
    };
    if !open.is_ok() {
        return None;
    }
    let name = wide("MachineGuid");
    let mut ty = REG_VALUE_TYPE::default();
    let mut len = 0u32;
    // SAFETY: size probe.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut len),
        )
    };
    if err.is_err() || ty != REG_SZ || len == 0 {
        let _ = unsafe { RegCloseKey(hk) };
        return None;
    }
    let mut buf = vec![0u16; len as usize / 2 + 1];
    let mut len2 = len;
    // SAFETY: buffer covers the reported size.
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut len2),
        )
    };
    let _ = unsafe { RegCloseKey(hk) };
    if err.is_err() {
        return None;
    }
    let raw = &buf[..len2 as usize / 2];
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Some(String::from_utf16_lossy(&raw[..end]))
}

/// The SYSTEM drive's volume serial (hardware binding input).
pub fn system_drive_serial() -> Option<u32> {
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let root = format!("{drive}\\");
    let wide_root = wide(&root);
    let mut serial: u32 = 0;
    // SAFETY: NUL-terminated volume root; out-pointer valid.
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide_root.as_ptr()),
            None,
            Some(&mut serial),
            None,
            None,
            None,
        )
    };
    ok.is_ok().then_some(serial)
}

/// CPUID brand string (hardware binding input; `None` on non-x86 or
/// CPUID-less CPUs).
#[cfg(target_arch = "x86_64")]
pub fn cpuid_brand() -> Option<String> {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{__cpuid, CpuidResult};
        // CPUID leaf 0x80000000 (safe intrinsic on x86_64).
        let max = __cpuid(0x8000_0000).eax;
        if max < 0x8000_0004 {
            return None;
        }
        let mut brand = String::new();
        for leaf in 0x8000_0002..=0x8000_0004 {
            // Extended brand leaves validated by max above.
            let CpuidResult { eax, ebx, ecx, edx } = __cpuid(leaf);
            for word in [eax, ebx, ecx, edx] {
                let chunk = word.to_le_bytes();
                brand.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
        // v3 fix: CPUID brand strings carry NUL padding between/after
        // the leaf fragments on QEMU/VMware-style virtual CPUs — a raw
        // .trim() keeps the NULs and the SERVER then has to drop the
        // whole field. Clean controls + collapse spaces client-side.
        let cleaned = clean_hw_string(&brand);
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    }
}

/// DPAPI-encrypt bytes (licensing doc §5.4 — local cache at rest).
///
/// # Errors
/// String error when `CryptProtectData` fails.
pub fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input blob points at `data` for the call; output blob is
    // filled by DPAPI (LocalAlloc) and copied+freed below.
    let result = unsafe {
        CryptProtectData(
            &input,
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            0,
            &mut output,
        )
    };
    if let Err(e) = result {
        return Err(format!("CryptProtectData: {e}"));
    }
    let bytes = unsafe {
        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let out = slice.to_vec();
        windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            output.pbData.cast::<core::ffi::c_void>(),
        )));
        out
    };
    Ok(bytes)
}

/// DPAPI-decrypt bytes.
///
/// # Errors
/// String error when `CryptUnprotectData` fails (wrong user/blob).
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input blob valid; output freed below (LocalFree).
    let result = unsafe { CryptUnprotectData(&input, None, None, None, None, 0, &mut output) };
    if let Err(e) = result {
        return Err(format!("CryptUnprotectData: {e}"));
    }
    let bytes = unsafe {
        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let out = slice.to_vec();
        windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            output.pbData.cast::<core::ffi::c_void>(),
        )));
        out
    };
    Ok(bytes)
}

/// Hardlink identity `(volume serial, file index)` for a path (spec §10:
/// hardlinks are NOT duplicates). `None` = unavailable (treated unique).
/// Platform seam: the only Win32 file-id call site outside `scanner`
/// internals — `commands::dupes` consumes it without FFI.
pub fn hardlink_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let f = std::fs::File::open(path).ok()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid file handle; sized out-struct.
    if unsafe {
        GetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(f.as_raw_handle()),
            &mut info,
        )
    }
    .is_err()
    {
        return None;
    }
    Some((u64::from(info.dwVolumeSerialNumber), {
        let lo = u64::from(info.nFileIndexLow);
        let hi = u64::from(info.nFileIndexHigh);
        (hi << 32) | lo
    }))
}

/// Same as [`hardlink_identity`] but accepts an ALREADY-OPEN handle
/// (the dupes engine's verify pass has one positioned past the
/// screens — reusing it saves the THIRD `File::open` per verified
/// member that the v2 caller used to do). On Windows with Defender
/// active, each `File::open` triggers a scan at 5-50 ms; 10k
/// verified files × 20 ms = 200 s of pure open latency AFTER the
/// verify pass finishes — that's the "engine is hung on Done" the
/// user reported.
///
/// Returns `None` when the handle is invalid or the platform
/// call fails (the caller falls back to `node_id` as the file
/// index).
pub fn hardlink_identity_for_handle(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid file handle (the caller owns it); sized out-struct.
    if unsafe {
        GetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle()),
            &mut info,
        )
    }
    .is_err()
    {
        return None;
    }
    Some((u64::from(info.dwVolumeSerialNumber), {
        let lo = u64::from(info.nFileIndexLow);
        let hi = u64::from(info.nFileIndexHigh);
        (hi << 32) | lo
    }))
}

/// Windows version for the license device facts (audit field; D1 stores
/// it with the device registration). `RtlGetVersion` is the
/// manifest-lie-proof source (GetVersionEx lies without a shim).
#[must_use]
pub fn os_version() -> String {
    // RtlGetVersion lives in the WDK projection (Wdk_System_SystemServices
    // feature — already enabled for the turbo engine); the out-struct
    // type is the classic OSVERSIONINFOW.
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>()).unwrap_or(0),
        ..OSVERSIONINFOW::default()
    };
    // SAFETY: properly sized out-struct per the API contract.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status.is_ok() {
        format!(
            "Windows {}.{}.{}",
            info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
        )
    } else {
        "Windows".to_string()
    }
}

// ============================================================================
// v3 device facts: SMBIOS baseboard / firmware UUID / BIOS build / cores / arch
// ============================================================================

/// SMBIOS baseboard serial (v3 device fact): the registry mirror of the
/// Type 2 (Baseboard) structure, straight from the HARDWARE hive the
/// kernel rebuilds from firmware at every boot (readable without
/// admin). `None` when the OEM left it blank ("To be filled by O.E.M."
/// is treated as absent).
#[must_use]
pub fn baseboard_serial() -> Option<String> {
    let raw = reg_sz(r"HARDWARE\DESCRIPTION\System\BIOS", "BaseBoardSerialNumber")?;
    let cleaned = clean_hw_string(&raw);
    absent_marker(&cleaned).then_some(cleaned)
}

/// BIOS/firmware build string (v3 device fact, display-only):
/// e.g. "DELL   - 1072009", "American Megatrends Inc. 5.17". `None`
/// when blank.
#[must_use]
pub fn firmware_version() -> Option<String> {
    let raw = reg_sz(r"HARDWARE\DESCRIPTION\System\BIOS", "BIOSVersion")?;
    let cleaned = clean_hw_string(&raw);
    absent_marker(&cleaned).then_some(cleaned)
}

/// SMBIOS System UUID (v3 device fact): parsed straight from the raw
/// `GetSystemFirmwareTable("RSMB")` table — the Type 1 (System
/// Information) structure's UUID field at offset 8. This is the
/// hardware identity Windows Autopilot hashes; it survives OS
/// reinstall and drive swaps. `None` when the table is unreadable or
/// the UUID is the "not present / not set" sentinel.
#[must_use]
pub fn firmware_uuid() -> Option<String> {
    let table = raw_smbios()?;
    parse_smbios_uuid(&table)
}

/// Logical CPU count (v3 device fact). Windows groups/reservations can
/// make `available_parallelism` report less than the physical logical
/// count in exotic processor-group cases, but on consumer SKUs it is
/// the logical processor count.
#[must_use]
pub fn cpu_cores() -> Option<u32> {
    std::thread::available_parallelism()
        .ok()
        .map(|n| n.get() as u32)
}

/// CPU architecture (v3 device fact): `std::env::consts::ARCH`
/// ("x86_64" / "aarch64") — the Rust target triple's arch segment.
#[must_use]
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// The raw SMBIOS table via `GetSystemFirmwareTable` with provider
/// 'RSMB'. The buffer starts with the `RawSMBIOSData` header
/// (calling method, major, minor, dmi revision, length) followed by
/// the structure stream.
fn raw_smbios() -> Option<Vec<u8>> {
    use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};
    // Size probe: NULL buffer returns the required size.
    // SAFETY: size-only probe per the API contract.
    let need = unsafe { GetSystemFirmwareTable(RSMB, 0, None) };
    if need == 0 {
        return None;
    }
    let mut buf = vec![0u8; need as usize];
    // SAFETY: buffer sized by the probe; the windows-rs 0.62 binding
    // takes `Option<&mut [u8]>` and derives (ptr, len) from the slice.
    let got = unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut buf)) };
    if got == 0 || got as usize > buf.len() {
        return None;
    }
    buf.truncate(got as usize);
    Some(buf)
}

/// Walk the SMBIOS structure stream and extract the Type 1 (System
/// Information) UUID, formatted in the canonical 8-4-4-4-12 form.
/// All-zero ("not set") and all-0xFF ("not present") UUIDs are treated
/// as absent. Handles SMBIOS 2.x/3.x (both table encodings keep the
/// same structure-stream layout after the 8-byte header).
fn parse_smbios_uuid(table: &[u8]) -> Option<String> {
    // RawSMBIOSData header: 1+1+1+1+4 = 8 bytes before the stream.
    const HEADER: usize = 8;
    if table.len() <= HEADER {
        return None;
    }
    let mut stream = &table[HEADER..];
    let mut type1_uuid: Option<[u8; 16]> = None;
    while stream.len() >= 4 {
        let hdr = &stream[..4];
        let struct_type = hdr[0];
        let length = hdr[1] as usize; // formatted area + header
        if length < 4 || stream.len() < length {
            break; // corrupt table — bail out honestly
        }
        if struct_type == 1 && length >= 0x1A && type1_uuid.is_none() {
            // Type 1 formatted area: UUID occupies [8, 24).
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&stream[8..24]);
            type1_uuid = Some(uuid);
        }
        // Skip the formatted area, then the string area (terminated by
        // a double NUL — walk until two consecutive zeros).
        let mut pos = length;
        while pos + 1 < stream.len() {
            if stream[pos] == 0 && stream[pos + 1] == 0 {
                pos += 2;
                break;
            }
            pos += 1;
        }
        if pos >= stream.len() {
            break;
        }
        stream = &stream[pos..];
        // Type 127 = end-of-table marker.
        if struct_type == 127 {
            break;
        }
    }
    let uuid = type1_uuid?;
    // SMBIOS UUIDs are "not set" (all zero) or "not present" (all 0xFF)
    // on many consumer boards / VMs — not an identity.
    if uuid.iter().all(|&b| b == 0) || uuid.iter().all(|&b| b == 0xFF) {
        return None;
    }
    // Canonical text form (network-byte order for the first three
    // fields matches how Windows displays it, since SMBIOS ≥ 2.6
    // defines the UUID as little-endian on the wire and tools show the
    // canonical RFC form).
    let hexs = |b: &[u8]| -> String {
        use std::fmt::Write as _;
        let mut s = String::with_capacity(b.len() * 2);
        for x in b {
            let _ = write!(s, "{x:02x}");
        }
        s
    };
    Some(format!(
        "{}-{}-{}-{}-{}",
        hexs(&uuid[0..4]),
        hexs(&uuid[4..6]),
        hexs(&uuid[6..8]),
        hexs(&uuid[8..10]),
        hexs(&uuid[10..16])
    ))
}

/// Strip control characters (NUL padding, stray CR/LF) and collapse
/// runs of spaces — SMBIOS/registry strings frequently arrive padded.
fn clean_hw_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_space = false;
    for ch in raw.chars() {
        if ch.is_control() {
            last_space = out.ends_with(' ');
            continue;
        }
        if ch == ' ' {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim_end().to_string()
}

/// True when the string is a placeholder the OEM never filled in —
/// "To be filled by O.E.M." and friends carry no identity.
fn absent_marker(s: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "to be filled",
        "default string",
        "not specified",
        "not applicable",
        "none",
    ];
    let lower = s.to_ascii_lowercase();
    !lower.is_empty() && !MARKERS.iter().any(|m| lower.contains(m))
}
