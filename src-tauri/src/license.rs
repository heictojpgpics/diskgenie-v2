//! License layer v2 (docs/LICENSING-ARCHITECTURE.md; supersedes the Dodo
//! Payments design in doc 06): DiskGenie-owned licensing over our
//! Cloudflare Worker + D1 backend.
//!
//! Security model (the short version — the full layer table lives in the
//! doc):
//! * The server issues **Ed25519-signed entitlement tokens**; this client
//!   verifies every token against the embedded PUBLIC key
//!   ([`LICENSE_PUBLIC_KEY_HEX`]), so a spoofed license server (hosts
//!   redirect, local mimic) cannot forge a license — the private key
//!   never leaves the Worker.
//! * Tokens bind to this machine's [`hardware_id`] and to the stored key
//!   hash; copying the DPAPI/Keychain-wrapped state to another machine
//!   fails verification.
//! * App→server requests are HMAC-SHA256-signed (timestamp + single-use
//!   nonce + body hash) — friction against casual endpoint abuse.
//! * Every privileged Tauri command re-checks [`posture`] in Rust (the
//!   UI lock is not the boundary).
//!
//! The protocol client sits behind the [`LicenseHttp`] seam so the whole
//! state machine is unit-testable with a fake transport + injected
//! clock; the Ed25519 fixture tests exercise the real signature path
//! with a test keypair.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Default production license-server base (change per deployment; the
/// env `DISKGENIE_LICENSE_API` overrides for tests/local dev).
const LICENSE_API_BASE: &str = "https://diskbytes-license.heictojpg-pics.workers.dev/";

/// Ed25519 public key (64 hex chars) — the ONLY key this binary holds.
/// The matching private seed lives as the Worker secret
/// `LICENSE_SIGNING_PRIVATE_KEY` (deployment guide: the license-server
/// repo README §2). Rotating the pair = regenerate + update here + ship.
pub const LICENSE_PUBLIC_KEY_HEX: &str =
    "4e4f51ef1593c184a1d1863d0cd53f5c3718c1674a1dfb3b85ee552d775fbb89";

/// HMAC-SHA256 request secret (64 hex) shared with the Worker secret
/// `CLIENT_REQUEST_SECRET`. NOTE (honest threat model, doc §2 L2): this
/// ships in a public-source binary — it is abuse friction, NOT the
/// security boundary; the boundary is [`LICENSE_PUBLIC_KEY_HEX`].
const CLIENT_SECRET_HEX: &str = "333d71177f5b8558e7ea8f45140742dfa71621e1000b899b682e340b80c61993";

/// The purchase page (external; collects name + email + billing address
/// at checkout — the address stays with the payment processor).
pub const LICENSE_PURCHASE_URL: &str = "https://diskgenie.app/pricing";

/// Revalidation interval (doc §6: 24 h).
pub const VALIDATION_INTERVAL_S: i64 = 24 * 60 * 60;

/// Offline grace window baked into every issued token (keep in sync with
/// the Worker var `TOKEN_TTL_DAYS`).
pub const GRACE_DAYS: i64 = 14;

/// Allowed clock skew for token issuance times.
const CLOCK_SKEW_S: i64 = 300;

/// Resolve the API base (env override for tests/local dev).
#[must_use]
pub fn api_base() -> String {
    std::env::var("DISKGENIE_LICENSE_API").unwrap_or_else(|_| LICENSE_API_BASE.to_string())
}

// ============================================================================
// HTTP seam
// ============================================================================

/// The HTTP seam: real = reqwest blocking; tests = scripted fake.
pub trait LicenseHttp {
    /// POST JSON with headers → (status, body).
    ///
    /// # Errors
    /// String transport error (network/TLS) — callers map to
    /// [`LicenseError::Network`]-class soft failures.
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String>;
}

/// The real transport (reqwest, 30 s timeout — same policy as doc 06 §6).
pub struct ReqwestLicense {
    client: reqwest::blocking::Client,
}

impl ReqwestLicense {
    /// Build with the documented timeout.
    ///
    /// # Errors
    /// String error when the TLS client cannot be constructed.
    pub fn new() -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("license http client: {e}"))?;
        Ok(Self { client })
    }
}

impl LicenseHttp for ReqwestLicense {
    fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String> {
        let mut req = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            // WIRE CONTRACT (the license.worker's guard.ts pins this UA
            // exactly — BAD_UA otherwise). The DEPLOYED worker still
            // validates the pre-rename value, so the client keeps it
            // until the owner's server cutover; the renamed server
            // source accepts BOTH values through a compat window, so
            // this flips to "DiskGenie-License-Client/1" in the SAME
            // change as LICENSE_API_BASE (see the server repo's
            // wrangler.jsonc cutover note).
            .header(reqwest::header::USER_AGENT, "DiskBytes-License-Client/1");
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req
            .body(body.to_string())
            .send()
            .map_err(|e| format!("network: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().map_err(|e| format!("network: {e}"))?;
        Ok((status, text))
    }
}

// ============================================================================
// Errors (typed UX copy + machine markers)
// ============================================================================

/// Typed license errors with the user-facing copy (mirrors the server's
/// error-code contract; docs §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseError {
    /// 404 — key not in our records.
    InvalidKey,
    /// 403 — key revoked or refunded.
    Inactive,
    /// 403 — yearly license past its expiry.
    Expired,
    /// 409 — the platform slot is held by another device.
    DeviceSlotTaken,
    /// 403 — this device is not the registered one.
    DeviceMismatch,
    /// 429 — rate limit (D1 fixed window) — retry after a pause.
    RateLimited,
    /// Transport failure → offline grace path.
    Network,
    /// 5xx / parse failure.
    ServerError,
    /// A server response FAILED local signature/binding verification —
    /// the anti-spoofing tripwire (treat as hard failure, never ok).
    Spoofed,
}

impl std::fmt::Display for LicenseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::InvalidKey => "Invalid license key. Please check it and try again.",
            Self::Inactive => "This license key is no longer active. Contact support.",
            Self::Expired => "Your yearly license has expired. Renew to keep DiskGenie Pro.",
            Self::DeviceSlotTaken => {
                "This key is already activated on another device. Contact support to move your license."
            }
            Self::DeviceMismatch => "This device isn't registered with this license key.",
            Self::RateLimited => "Too many attempts. Wait a minute and try again.",
            Self::Network => "Network unavailable. DiskGenie keeps working offline.",
            Self::ServerError => "The license server had a problem. Try again in a moment.",
            Self::Spoofed => {
                "The license server's response could not be verified. Reconnect and try again."
            }
        };
        f.write_str(s)
    }
}

impl LicenseError {
    /// Map a (status, error-code) pair per the server contract.
    fn from_response(status: u16, code: &str) -> Self {
        match (status, code) {
            (404, _) | (_, "KEY_NOT_FOUND") => Self::InvalidKey,
            (403, "KEY_REVOKED" | "KEY_REFUNDED" | "KEY_PENDING") => Self::Inactive,
            (403, "LICENSE_EXPIRED") => Self::Expired,
            (403, "DEVICE_MISMATCH") => Self::DeviceMismatch,
            (429, _) | (_, "RATE_LIMITED") => Self::RateLimited,
            (409, _) | (_, "DEVICE_SLOT_TAKEN") => Self::DeviceSlotTaken,
            _ => Self::ServerError,
        }
    }

    /// Hard failures deactivate local state (doc §6 posture semantics).
    /// Rate-limited is SOFT (retry with backoff — the window resets).
    #[must_use]
    pub fn is_hard(&self) -> bool {
        !matches!(self, Self::Network | Self::ServerError | Self::RateLimited)
    }
}

// ============================================================================
// Protocol types
// ============================================================================

/// The device facts collected at activation (doc §6: what we collect and
/// store in D1 — platform + fingerprint + audit strings, nothing else).
///
/// v2 (additive): component hashes + descriptive facts travel on
/// EVERY request (activate AND validate — the server-side wipe fix
/// needs the revalidation to carry the claim), serialized camelCase
/// with absent fields skipped. The composite [`hardware_hash`] stays
/// THE binding identity (algorithm unchanged — v1-activated devices
/// re-match after upgrade); the components give support swap
/// forensics (disk swap vs machine swap) and the admin panel a real
/// device census.
///
/// v3 (additive): baseboard serial, firmware UUID, BIOS build, CPU
/// core count, arch + the board/firmware component hashes — deep
/// hardware forensics (motherboard RMA vs machine swap vs VM).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceFacts {
    /// "windows" | "macos".
    pub platform: String,
    /// The hardware fingerprint (64 hex) — the binding identity.
    pub hardware_hash: String,
    /// Human-readable machine name (audit + support display).
    pub hostname: String,
    /// OS version string (audit + support display).
    pub os_version: String,
    /// App semver (audit + support display).
    pub app_version: String,
    /// sha256("machine:" + MachineGuid / IOPlatformUUID) — component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comp_machine: Option<String>,
    /// sha256("volume:" + system-drive serial / root fsid) — component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comp_volume: Option<String>,
    /// sha256("cpu:" + CPUID brand) — component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comp_cpu: Option<String>,
    /// CPU brand string for display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_brand: Option<String>,
    /// Total physical memory (MB).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_mb: Option<u64>,
    /// Machine model for display ("Dell Inc. XPS 15 9520" / "MacBookPro18,3").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_model: Option<String>,
    /// v3: baseboard serial (SMBIOS Type 2 / IOPlatformSerialNumber).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseboard_serial: Option<String>,
    /// v3: firmware UUID (SMBIOS Type 1 / IOPlatformUUID), canonical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_uuid: Option<String>,
    /// v3: BIOS/firmware build string (display-only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bios_version: Option<String>,
    /// v3: logical CPU count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    /// v3: CPU architecture ("x86_64" / "aarch64").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// v3: sha256("board:" + baseboard serial) — component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comp_board: Option<String>,
    /// v3: sha256("firmware:" + firmware UUID) — component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comp_firmware: Option<String>,
}

/// The server's success envelope (serde mirror of the Worker response).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntitlementDto {
    /// The signed compact token — the ONLY field the client trusts;
    /// every display datum (name/email/tier/expiry) is read from the
    /// VERIFIED claims inside it, never from the unsigned envelope.
    pub token: String,
}

/// The error envelope (the `code` field drives every decision; the
/// human copy is regenerated client-side from the typed mapping).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorDto {
    #[serde(default)]
    code: String,
}

// ============================================================================
// Entitlement token (Ed25519-signed; the anti-spoofing core)
// ============================================================================

/// The decoded token claims (serde mirror — every field the client
/// independently verifies; see [`verify_token`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenClaims {
    /// Fixed issuer "db-license".
    pub iss: String,
    /// Format version (1).
    pub ver: u32,
    /// Unique token id (hex) — part of the signed wire format; the
    /// client does not branch on it (single-use issuance is
    /// server-side policy).
    #[allow(dead_code)]
    pub jti: String,
    /// Issued-at (unix seconds).
    pub iat: i64,
    /// Token expiry (unix seconds) — the offline grace window.
    pub exp: i64,
    /// sha256(normalized key) hex — binds the token to OUR stored key.
    pub key: String,
    /// "yearly" | "lifetime".
    pub tier: String,
    /// Customer display name.
    #[serde(default)]
    pub name: String,
    /// Customer email.
    #[serde(default)]
    pub email: String,
    /// The hardware fingerprint the server bound this token to.
    pub hw: String,
    /// "windows" | "macos".
    pub plat: String,
    /// License expiry (yearly) — absent for lifetime.
    #[serde(default)]
    pub lexp: Option<i64>,
    /// Key id (server tokens since the kid/aud change; absent in the
    /// legacy fleet — the compat window). The client knows ONE key:
    /// kid 1 = the embedded production public key.
    #[serde(default)]
    pub kid: Option<u32>,
    /// Audience ("diskgenie" — server tokens since the kid/aud
    /// change; absent in the legacy fleet).
    #[serde(default)]
    pub aud: Option<String>,
}

/// Verify a compact token `b64url(payload).b64url(sig)` end-to-end:
/// signature against `public_key_hex` (callers pass
/// [`LICENSE_PUBLIC_KEY_HEX`]; tests pass the fixture key),
/// issuer/version/platform, key-hash binding, hardware binding, token
/// window, license expiry. ANY failure → [`LicenseError::Spoofed`]
/// (never a partial accept).
///
/// # Errors
/// [`LicenseError::Spoofed`] when any check fails.
pub fn verify_token(
    token: &str,
    public_key_hex: &str,
    expected_key_hash: &str,
    expected_hw: &str,
    platform: &str,
    now: i64,
) -> Result<TokenClaims, LicenseError> {
    let (payload_b64, sig_b64) = token.split_once('.').ok_or(LicenseError::Spoofed)?;
    let payload = b64url_decode(payload_b64).ok_or(LicenseError::Spoofed)?;
    let sig_bytes = b64url_decode(sig_b64).ok_or(LicenseError::Spoofed)?;
    let arr: [u8; 64] = sig_bytes.try_into().map_err(|_| LicenseError::Spoofed)?;
    let sig = Signature::from_bytes(&arr);

    let key_bytes = from_hex(public_key_hex).ok_or(LicenseError::Spoofed)?;
    let key_arr: [u8; 32] = key_bytes.try_into().map_err(|_| LicenseError::Spoofed)?;
    let verifying = VerifyingKey::from_bytes(&key_arr).map_err(|_| LicenseError::Spoofed)?;

    verifying
        .verify(&payload, &sig)
        .map_err(|_| LicenseError::Spoofed)?;

    let claims: TokenClaims =
        serde_json::from_slice(&payload).map_err(|_| LicenseError::Spoofed)?;
    if claims.iss != "db-license"
        || claims.ver != 1
        || claims.plat != platform
        || claims.key != expected_key_hash
        || claims.hw != expected_hw
        || claims.exp <= now
        || claims.iat > now + CLOCK_SKEW_S
        || matches!(claims.lexp, Some(lexp) if lexp <= now)
        // kid/aud (the server's claim checks mirrored client-side —
        // defense in depth: absent claims stay valid through the
        // fleet compat window, present claims must be OURS).
        || matches!(claims.kid, Some(k) if k != 1)
        || matches!(claims.aud.as_deref(), Some(a) if a != "diskgenie")
    {
        return Err(LicenseError::Spoofed);
    }
    Ok(claims)
}

// ============================================================================
// Persisted state + posture
// ============================================================================

/// Persisted license state (DPAPI/Keychain-wrapped on disk; doc §6).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LicenseState {
    /// Normalized key (22 chars, uppercase).
    #[serde(default)]
    pub license_key: String,
    /// 64-hex hardware fingerprint captured at activation.
    #[serde(default)]
    pub hardware_id: String,
    /// "windows" | "macos".
    #[serde(default)]
    pub platform: String,
    /// "yearly" | "lifetime" ("" before activation).
    #[serde(default)]
    pub tier: String,
    /// Customer display name (verified claims).
    #[serde(default)]
    pub customer_name: String,
    /// Customer email (verified claims).
    #[serde(default)]
    pub customer_email: String,
    /// License expiry (unix; 0 = lifetime / unknown).
    #[serde(default)]
    pub license_expires_at: i64,
    /// The latest verified token (its `exp` drives the grace window).
    #[serde(default)]
    pub token: String,
    /// Token expiry — the offline grace window's hard stop.
    #[serde(default)]
    pub token_exp: i64,
    /// When this device activated (display).
    #[serde(default)]
    pub activated_at: i64,
    /// Last successful server validation.
    #[serde(default)]
    pub last_validated_at: i64,
    /// Monotonic "last known good time" (clock-rollback guard).
    #[serde(default)]
    pub last_known_good: i64,
    /// CI simulation marker (`ci-license-sim` feature only).
    #[serde(default)]
    pub simulated: bool,
}

/// The runtime posture derived from state + now (doc §6, STRICT):
/// unlicensed/degraded are LOCKED (Monitor tab only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicensePosture {
    /// No license on file → locked.
    Unlicensed,
    /// Verified, fresh (checked within the interval).
    Pro,
    /// Verified, offline, token still inside its grace window.
    Grace {
        /// Whole days left in the offline window (display).
        days_left: i64,
    },
    /// Token window exhausted (or license expired) → locked.
    Degraded,
}

/// Derive the posture (PURE — injected clock for tests).
#[must_use]
pub fn posture(state: &LicenseState, now: i64) -> LicensePosture {
    if state.license_key.is_empty() {
        return LicensePosture::Unlicensed;
    }
    if state.token_exp <= now {
        return LicensePosture::Degraded;
    }
    // A yearly license past its own expiry is degraded even while the
    // token is technically valid (honest UX; the server also refuses).
    if state.license_expires_at > 0 && state.license_expires_at <= now {
        return LicensePosture::Degraded;
    }
    let reference = state.last_validated_at.max(state.last_known_good);
    let age = now - reference;
    if age < VALIDATION_INTERVAL_S {
        return LicensePosture::Pro;
    }
    let seconds_left = state.token_exp - now;
    LicensePosture::Grace {
        days_left: (seconds_left + 86_399) / 86_400,
    }
}

/// The device identity for binding (unchanged from doc 06 §3.5):
/// `SHA-256(MachineGuid ‖ system-drive volume serial ‖ CPUID)` via the
/// platform seam (IOPlatformUUID + fsid + CPU brand on macOS).
/// The algorithm is FROZEN — the server's stored device rows (and
/// every issued token's `hw` claim) match this exact digest; changing
/// the recipe would orphan every v1 activation.
///
/// # Errors
/// String error when any platform input is unreadable (all three
/// required — no partial identity).
pub fn hardware_id() -> Result<String, String> {
    let machine_guid = crate::platform::os::machine_guid().ok_or("MachineGuid unreadable")?;
    let serial = crate::platform::os::system_drive_serial().ok_or("volume serial unreadable")?;
    let cpuid = crate::platform::os::cpuid_brand().ok_or("CPUID unavailable")?;
    let mut hasher = Sha256::new();
    hasher.update(machine_guid.as_bytes());
    hasher.update(serial.to_le_bytes());
    hasher.update(cpuid.as_bytes());
    let digest = hasher.finalize();
    Ok(hex_of(&digest))
}

/// Hash one component identity (pure seam — tests pin the recipe):
/// `sha256(label || value)`, `None` when the input is unreadable.
#[must_use]
pub fn component_hash(label: &str, value: Option<String>) -> Option<String> {
    let v = value?;
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    hasher.update(v.as_bytes());
    Some(hex_of(&hasher.finalize()))
}

/// The per-component identity hashes (v2/v3 device claim): each binding
/// input hashed SEPARATELY so the server can tell WHICH component
/// changed when a device shows up with a new composite (disk swap vs
/// machine swap vs CPU swap — swap forensics, not a new binding).
/// Each field is `None` when that input is unreadable; v3 adds the
/// board + firmware components (motherboard RMA vs machine swap).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentHashes {
    /// sha256("machine:" + MachineGuid / IOPlatformUUID).
    pub machine: Option<String>,
    /// sha256("volume:" + system-drive serial / root fsid).
    pub volume: Option<String>,
    /// sha256("cpu:" + CPUID brand).
    pub cpu: Option<String>,
    /// sha256("board:" + baseboard serial) — v3.
    pub board: Option<String>,
    /// sha256("firmware:" + firmware UUID) — v3.
    pub firmware: Option<String>,
}

/// Collect the FULL device claim for this machine (v2/v3): the hard
/// composite fingerprint first (its failure is an activation error),
/// then every best-effort descriptive fact — each independently
/// `None` when unreadable (the server COALESCEs; a missing fact never
/// blanks a stored one). The command layer AND the live E2E both use
/// this exact path, so what CI proves is what production sends.
///
/// # Errors
/// String error when the composite [`hardware_id`] inputs are
/// unreadable (activation must not proceed with a soft identity).
pub fn collect_device_facts() -> Result<DeviceFacts, String> {
    let hardware_hash = hardware_id()?;
    let hostname = crate::platform::os::hostname()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "Mac".to_string()
            } else {
                "PC".to_string()
            }
        });
    let comp = component_hashes();
    Ok(DeviceFacts {
        platform: platform_string().to_string(),
        hardware_hash,
        hostname,
        os_version: crate::platform::os::os_version(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        comp_machine: comp.machine,
        comp_volume: comp.volume,
        comp_cpu: comp.cpu,
        cpu_brand: crate::platform::os::cpuid_brand(),
        ram_mb: crate::platform::os::ram_mb(),
        machine_model: crate::platform::os::machine_model(),
        baseboard_serial: crate::platform::os::baseboard_serial(),
        firmware_uuid: crate::platform::os::firmware_uuid(),
        bios_version: crate::platform::os::firmware_version(),
        cpu_cores: crate::platform::os::cpu_cores(),
        arch: Some(crate::platform::os::arch().to_string()),
        comp_board: comp.board,
        comp_firmware: comp.firmware,
    })
}

/// The platform identifier the server binds slots by.
fn platform_string() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else {
        "windows"
    }
}

/// Collect this machine's [`ComponentHashes`].
#[must_use]
pub fn component_hashes() -> ComponentHashes {
    ComponentHashes {
        machine: component_hash("machine:", crate::platform::os::machine_guid()),
        volume: component_hash(
            "volume:",
            crate::platform::os::system_drive_serial().map(|s| s.to_string()),
        ),
        cpu: component_hash("cpu:", crate::platform::os::cpuid_brand()),
        board: component_hash("board:", crate::platform::os::baseboard_serial()),
        firmware: component_hash("firmware:", crate::platform::os::firmware_uuid()),
    }
}

/// sha256 hex of a string (key-hash binding; public for the command
/// layer).
#[must_use]
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex_of(&hasher.finalize())
}

// ============================================================================
// The protocol client
// ============================================================================

/// Nonce counter (process-lifetime) mixed into request nonces.
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Client for the three authenticated endpoints. Every request carries
/// the HMAC headers; every response is treated as untrusted input
/// (parse → verify signature → only then apply).
pub struct LicenseApi<H: LicenseHttp> {
    http: H,
    base: String,
    secret_hex: String,
}

impl<H: LicenseHttp> LicenseApi<H> {
    /// Build against the resolved base + shared secret.
    #[must_use]
    pub fn new(http: H) -> Self {
        Self {
            http,
            base: api_base(),
            secret_hex: CLIENT_SECRET_HEX.to_string(),
        }
    }

    /// Build with an explicit secret override (tests only).
    #[cfg(test)]
    #[must_use]
    pub fn with_secret(http: H, secret_hex: String) -> Self {
        Self {
            http,
            base: api_base(),
            secret_hex,
        }
    }

    /// A single-use nonce: counter + nanos + pid + the local hardware
    /// fingerprint hashed together (uniqueness is the property that
    /// matters for replay defense).
    fn nonce(&self) -> String {
        let count = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let pid = u64::from(std::process::id());
        let mut hasher = Sha256::new();
        hasher.update(count.to_le_bytes());
        hasher.update(nanos.to_le_bytes());
        hasher.update(pid.to_le_bytes());
        hasher.update(self.secret_hex.as_bytes());
        hex_of(&hasher.finalize())[..32].to_string()
    }

    /// Sign + POST one request; returns (status, body).
    fn signed_post(&self, path: &str, body: &str) -> Result<(u16, String), LicenseError> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let nonce = self.nonce();
        let body_hash = sha256_hex(body);
        let message = format!("{timestamp}.{nonce}.POST.{path}.{body_hash}");
        let sig = hmac_hex(&self.secret_hex, &message);
        let headers = vec![
            ("x-db-app".to_string(), "diskgenie".to_string()),
            (
                "x-db-version".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            ),
            ("x-db-timestamp".to_string(), timestamp.to_string()),
            ("x-db-nonce".to_string(), nonce),
            ("x-db-signature".to_string(), sig),
        ];
        let url = format!("{}{path}", self.base.trim_end_matches('/'));
        self.http
            .post_json(&url, &headers, body)
            .map_err(|_| LicenseError::Network)
    }

    /// POST + parse the success/error envelope into an outcome.
    fn call(
        &self,
        path: &str,
        body: &str,
    ) -> Result<Result<EntitlementDto, LicenseError>, LicenseError> {
        let (status, text) = self.signed_post(path, body)?;
        if (200..300).contains(&status) {
            let dto: EntitlementDto =
                serde_json::from_str(&text).map_err(|_| LicenseError::ServerError)?;
            return Ok(Ok(dto));
        }
        let code = serde_json::from_str::<ErrorDto>(&text)
            .map(|e| e.code)
            .unwrap_or_default();
        Ok(Err(LicenseError::from_response(status, &code)))
    }

    /// Activate a key on this device (doc §4). The response token MUST
    /// verify locally (in the command layer's apply step) before it is
    /// trusted.
    ///
    /// # Errors
    /// [`LicenseError`] per the endpoint contract.
    pub fn activate(&self, key: &str, facts: &DeviceFacts) -> Result<EntitlementDto, LicenseError> {
        let body = claim_body(key, facts);
        match self.call("/v1/activate", &body)? {
            Ok(dto) => Ok(dto),
            Err(e) => Err(e),
        }
    }

    /// The 24 h revalidation call. v2: carries the FULL device claim —
    /// the server refreshes hostname/os/app/components with COALESCE
    /// semantics (the v1 wire omitted them, and the v1 server then
    /// blanked the stored rows — the owner-reported "doesn't save
    /// hostname / Windows version" bug; fixed on both ends).
    ///
    /// # Errors
    /// [`LicenseError`] per the endpoint contract.
    pub fn validate(&self, key: &str, facts: &DeviceFacts) -> Result<EntitlementDto, LicenseError> {
        let body = claim_body(key, facts);
        match self.call("/v1/validate", &body)? {
            Ok(dto) => Ok(dto),
            Err(e) => Err(e),
        }
    }

    /// Deactivate this device (frees the platform slot; idempotent).
    ///
    /// # Errors
    /// [`LicenseError::Network`] only is soft; hard errors surface.
    pub fn deactivate(&self, key: &str, facts: &DeviceFacts) -> Result<(), LicenseError> {
        let body = serde_json::json!({
            "licenseKey": key,
            "hardwareHash": facts.hardware_hash,
            "platform": facts.platform,
        });
        let (status, _) = self.signed_post("/v1/deactivate", &body.to_string())?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            // Deactivation is idempotent server-side; non-2xx here is
            // almost always "already gone" — treat as success so the
            // client always clears local state.
            Ok(())
        }
    }
}

/// The activate/validate wire body: the full DeviceFacts claim (the
/// `skip_serializing_if` attrs keep absent optional fields OFF the
/// wire) plus the license key. One shape for both routes — the server
/// refreshes the same columns either way.
fn claim_body(key: &str, facts: &DeviceFacts) -> String {
    let mut value = serde_json::to_value(facts).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = value.as_object_mut() {
        obj.insert("licenseKey".to_string(), serde_json::json!(key));
    }
    value.to_string()
}

// ============================================================================
// DPAPI/Keychain persistence (unchanged contract from doc 06 §5.4)
// ============================================================================

/// DPAPI-encrypted persistence (Windows `CryptProtectData`; the Keychain
/// on macOS — see `platform::os::dpapi_protect`).
pub mod dpapi {
    use super::LicenseState;

    /// Where the encrypted license blob lives.
    #[must_use]
    pub fn license_path() -> std::path::PathBuf {
        crate::platform::os::app_data_dir().join("license.bin")
    }

    /// Encrypt + persist the state.
    ///
    /// # Errors
    /// String error when DPAPI or the write fails.
    pub fn save(state: &LicenseState) -> Result<(), String> {
        let json = serde_json::to_vec(state).map_err(|e| format!("serialize: {e}"))?;
        let blob = crate::platform::os::dpapi_protect(&json)?;
        std::fs::write(license_path(), blob).map_err(|e| format!("write license: {e}"))
    }

    /// Load + decrypt the state (`None` when absent/corrupt — fresh start).
    #[must_use]
    pub fn load() -> Option<LicenseState> {
        let blob = std::fs::read(license_path()).ok()?;
        let json = crate::platform::os::dpapi_unprotect(&blob).ok()?;
        serde_json::from_slice(&json).ok()
    }

    /// Remove the stored license (deactivation cleanup; R7.1-safe).
    pub fn clear() {
        diskgenie_core::snapshots::remove_app_data_file(&license_path());
    }
}

// ============================================================================
// Codec helpers (hand-rolled, zero-dep, fully tested)
// ============================================================================

/// Lowercase hex (public for the command layer's key-hash binding).
#[must_use]
pub fn hex_of(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

/// Hex decode (case-insensitive; even length).
// The 1.98 clippy wants `s.len().is_multiple_of(2)` here, but that
// method is stable since 1.87 and the crate MSRV is 1.80 — the two
// lints (manual_is_multiple_of vs incompatible_msrv) cannot both be
// satisfied, so the modulo form stays with an explicit allow.
#[allow(clippy::manual_is_multiple_of)]
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Option<Vec<u8>> = s
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some(u8::try_from(hi * 16 + lo).unwrap_or(0))
        })
        .collect();
    bytes
}

/// RFC 4648 base64url decode (no padding; accepts standard padding).
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    const BAD: u8 = 255;
    fn val(c: u8) -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => BAD,
        }
    }
    let trimmed = s.trim_end_matches('=');
    let bytes = trimmed.as_bytes();
    if bytes.iter().any(|&c| val(c) == BAD) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in bytes {
        acc = (acc << 6) | u32::from(val(c));
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    // Leftover bits must be zero padding (canonical length check).
    if bits >= 6 || (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

/// HMAC-SHA256 hex (message over a hex key).
fn hmac_hex(key_hex: &str, message: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let key = from_hex(key_hex).unwrap_or_default();
    let mut mac = HmacSha256::new_from_slice(&key).expect("hmac accepts any key length");
    mac.update(message.as_bytes());
    hex_of(&mac.finalize().into_bytes())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::cell::RefCell;

    /// Fixture keypair: the RFC 8032 test-vector #1 seed (matches the
    /// license-server repo's TEST_SIGNING_SEED — both sides of the token
    /// contract are pinned by the same fixture).
    const TEST_SEED_HEX: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

    fn b64url(bytes: &[u8]) -> String {
        const CHARS: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::new();
        let mut acc: u32 = 0;
        let mut bits: u32 = 0;
        for &b in bytes {
            acc = (acc << 8) | u32::from(b);
            bits += 8;
            while bits >= 6 {
                bits -= 6;
                s.push(CHARS[((acc >> bits) & 0x3F) as usize] as char);
            }
        }
        if bits > 0 {
            s.push(CHARS[((acc << (6 - bits)) & 0x3F) as usize] as char);
        }
        s
    }

    fn test_public_key_hex() -> String {
        let seed_arr: [u8; 32] = from_hex(TEST_SEED_HEX).unwrap().try_into().unwrap();
        let signing = SigningKey::from_bytes(&seed_arr);
        hex_of(&signing.verifying_key().to_bytes())
    }

    /// Sign a claims object with the fixture key (server-side stand-in).
    fn sign_fixture(claims: &serde_json::Value, key_hex: &str) -> String {
        let seed_arr: [u8; 32] = from_hex(key_hex).unwrap().try_into().unwrap();
        let signing = SigningKey::from_bytes(&seed_arr);
        let payload = serde_json::to_vec(claims).unwrap();
        let sig = signing.sign(&payload);
        format!("{}.{}", b64url(&payload), b64url(&sig.to_bytes()))
    }

    fn claims_json(
        hw: &str,
        key_hash: &str,
        iat: i64,
        exp: i64,
        lexp: Option<i64>,
    ) -> serde_json::Value {
        let mut v = serde_json::json!({
            "iss": "db-license",
            "ver": 1,
            "jti": "0123456789abcdef0123456789abcdef",
            "iat": iat,
            "exp": exp,
            "key": key_hash,
            "tier": "lifetime",
            "name": "Alex Morgan",
            "email": "alex@example.com",
            "hw": hw,
            "plat": "windows",
        });
        if let Some(lexp) = lexp {
            v["lexp"] = serde_json::json!(lexp);
        }
        v
    }

    // ── token verification: the anti-spoofing matrix ─────────────────

    #[test]
    fn token_accepts_genuine_fixture() {
        let key_hash = "a".repeat(64);
        let hw = "b".repeat(64);
        let now = 1_790_000_000;
        let token = sign_fixture(
            &claims_json(&hw, &key_hash, now - 60, now + 86_400, None),
            TEST_SEED_HEX,
        );
        let pub_hex = test_public_key_hex();
        let claims = verify_token(&token, &pub_hex, &key_hash, &hw, "windows", now).unwrap();
        assert_eq!(claims.tier, "lifetime");
        assert_eq!(claims.name, "Alex Morgan");
        assert!(claims.lexp.is_none());
    }

    /// The kid/aud claim matrix (the server's post-2026-10 tokens):
    /// absent claims stay valid (fleet compat window), present claims
    /// must be OURS — kid 1 + aud "diskgenie".
    #[test]
    fn token_kid_aud_claim_matrix() {
        let key_hash = "a".repeat(64);
        let hw = "b".repeat(64);
        let now = 1_790_000_500_i64;
        let pub_hex = test_public_key_hex();
        let base = |extra: serde_json::Value| {
            let mut v = claims_json(&hw, &key_hash, now - 500, now + 86_400, None);
            for (k, val) in extra.as_object().unwrap() {
                v[k.as_str()] = val.clone();
            }
            sign_fixture(&v, TEST_SEED_HEX)
        };
        // No kid/aud (legacy fleet): valid.
        assert!(
            verify_token(&base(serde_json::json!({})), &pub_hex, &key_hash, &hw, "windows", now)
                .is_ok()
        );
        // Present + correct: valid.
        assert!(verify_token(
            &base(serde_json::json!({"kid": 1, "aud": "diskgenie"})),
            &pub_hex,
            &key_hash,
            &hw,
            "windows",
            now
        )
        .is_ok());
        // Wrong audience: spoofed.
        assert!(verify_token(
            &base(serde_json::json!({"kid": 1, "aud": "other-product"})),
            &pub_hex,
            &key_hash,
            &hw,
            "windows",
            now
        )
        .is_err());
        // Unknown key id: spoofed.
        assert!(verify_token(
            &base(serde_json::json!({"kid": 2, "aud": "diskgenie"})),
            &pub_hex,
            &key_hash,
            &hw,
            "windows",
            now
        )
        .is_err());
    }

    #[test]
    fn token_rejects_every_tamper_path() {
        let key_hash = "a".repeat(64);
        let hw = "b".repeat(64);
        let now = 1_790_000_000;
        let pub_hex = test_public_key_hex();
        let token = sign_fixture(
            &claims_json(&hw, &key_hash, now - 60, now + 86_400, None),
            TEST_SEED_HEX,
        );

        // Wrong verifying key (a different server / rotation mismatch).
        let other_seed = "4".repeat(64);
        let forged = sign_fixture(
            &claims_json(&hw, &key_hash, now - 60, now + 86_400, None),
            &other_seed,
        );
        assert_eq!(
            verify_token(&forged, &pub_hex, &key_hash, &hw, "windows", now).unwrap_err(),
            LicenseError::Spoofed
        );
        // The PRODUCTION key must also reject fixture tokens (and the
        // fixture key must reject production-signed tokens — key
        // rotation mismatch is a hard failure, never a maybe).
        assert_eq!(
            verify_token(
                &token,
                LICENSE_PUBLIC_KEY_HEX,
                &key_hash,
                &hw,
                "windows",
                now
            )
            .unwrap_err(),
            LicenseError::Spoofed
        );

        // Tampered payload under the genuine signature.
        let (p, s) = token.split_once('.').unwrap();
        let payload: Vec<u8> = b64url_decode(p).unwrap();
        let mut claims: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        claims["exp"] = serde_json::json!(now + 10 * 86_400); // escalation attempt
        let escalated = format!("{}.{}", b64url(&serde_json::to_vec(&claims).unwrap()), s);
        assert_eq!(
            verify_token(&escalated, &pub_hex, &key_hash, &hw, "windows", now).unwrap_err(),
            LicenseError::Spoofed
        );

        // Wrong machine (hardware binding).
        let other_hw = "c".repeat(64);
        assert_eq!(
            verify_token(&token, &pub_hex, &key_hash, &other_hw, "windows", now).unwrap_err(),
            LicenseError::Spoofed
        );

        // Wrong key-hash binding.
        assert_eq!(
            verify_token(&token, &pub_hex, &"d".repeat(64), &hw, "windows", now).unwrap_err(),
            LicenseError::Spoofed
        );

        // Wrong platform.
        assert_eq!(
            verify_token(&token, &pub_hex, &key_hash, &hw, "macos", now).unwrap_err(),
            LicenseError::Spoofed
        );

        // Expired token window.
        assert_eq!(
            verify_token(
                &token,
                &pub_hex,
                &key_hash,
                &hw,
                "windows",
                now + 86_400 + 1
            )
            .unwrap_err(),
            LicenseError::Spoofed
        );

        // Issued in the future (beyond skew).
        let future = sign_fixture(
            &claims_json(&hw, &key_hash, now + 400, now + 86_400, None),
            TEST_SEED_HEX,
        );
        assert_eq!(
            verify_token(&future, &pub_hex, &key_hash, &hw, "windows", now).unwrap_err(),
            LicenseError::Spoofed
        );

        // Yearly license itself expired.
        let lexp = sign_fixture(
            &claims_json(&hw, &key_hash, now - 60, now + 86_400, Some(now - 1)),
            TEST_SEED_HEX,
        );
        assert_eq!(
            verify_token(&lexp, &pub_hex, &key_hash, &hw, "windows", now).unwrap_err(),
            LicenseError::Spoofed
        );

        // Garbage shapes.
        for garbage in ["", "no-dot", "aaa.bbb", "...."] {
            assert!(verify_token(garbage, &pub_hex, &key_hash, &hw, "windows", now).is_err());
        }
    }

    #[test]
    fn token_fixture_matches_server_test_vector() {
        // The license-server repo signs with noble/ed25519 using the SAME
        // seed; cross-implementation compatibility (RFC 8032) means the
        // public key here must equal the server fixture's derived key.
        assert_eq!(
            test_public_key_hex(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
    }

    // ── posture state machine (STRICT: degraded locks) ──────────────

    const NOW: i64 = 1_790_000_000;

    /// A pro state whose token was last refreshed at `last_validated`
    /// (the 14-day window counts from there — the server mints
    /// `exp = issued + 14d`, so the fixture models it the same way).
    fn pro_state(last_validated: i64) -> LicenseState {
        LicenseState {
            license_key: "DB".to_string() + &"1".repeat(20),
            hardware_id: "hw".into(),
            platform: "windows".into(),
            tier: "lifetime".into(),
            customer_name: "Alex".into(),
            customer_email: "a@b.c".into(),
            license_expires_at: 0,
            token: "t".into(),
            token_exp: last_validated + 14 * 86_400,
            activated_at: NOW - 100,
            last_validated_at: last_validated,
            last_known_good: last_validated,
            simulated: false,
        }
    }

    #[test]
    fn posture_fresh_pro() {
        assert_eq!(posture(&pro_state(NOW - 3_600), NOW), LicensePosture::Pro);
    }

    #[test]
    fn posture_grace_counts_days() {
        // Token issued 10 days ago (14-day window) → 4 days left.
        let s = pro_state(NOW - 10 * 86_400);
        assert_eq!(posture(&s, NOW), LicensePosture::Grace { days_left: 4 });
    }

    #[test]
    fn posture_degraded_when_token_expires() {
        let mut s = pro_state(NOW - 20 * 86_400);
        s.token_exp = NOW - 1;
        assert_eq!(posture(&s, NOW), LicensePosture::Degraded);
        // Boundary: exactly at exp → degraded.
        s.token_exp = NOW;
        assert_eq!(posture(&s, NOW), LicensePosture::Degraded);
    }

    #[test]
    fn posture_degraded_when_yearly_license_expired() {
        let mut s = pro_state(NOW);
        s.tier = "yearly".into();
        s.license_expires_at = NOW - 1;
        assert_eq!(posture(&s, NOW), LicensePosture::Degraded);
    }

    #[test]
    fn posture_unlicensed_when_no_key() {
        assert_eq!(
            posture(&LicenseState::default(), NOW),
            LicensePosture::Unlicensed
        );
    }

    // ── error mapping + copy ────────────────────────────────────────

    #[test]
    fn errors_map_from_server_contract() {
        assert_eq!(
            LicenseError::from_response(404, "KEY_NOT_FOUND"),
            LicenseError::InvalidKey
        );
        assert_eq!(
            LicenseError::from_response(403, "KEY_REVOKED"),
            LicenseError::Inactive
        );
        assert_eq!(
            LicenseError::from_response(403, "LICENSE_EXPIRED"),
            LicenseError::Expired
        );
        assert_eq!(
            LicenseError::from_response(409, "DEVICE_SLOT_TAKEN"),
            LicenseError::DeviceSlotTaken
        );
        assert_eq!(
            LicenseError::from_response(403, "DEVICE_MISMATCH"),
            LicenseError::DeviceMismatch
        );
        assert_eq!(
            LicenseError::from_response(500, ""),
            LicenseError::ServerError
        );
    }

    #[test]
    fn only_network_and_server_are_soft() {
        assert!(!LicenseError::Network.is_hard());
        assert!(!LicenseError::ServerError.is_hard());
        assert!(!LicenseError::RateLimited.is_hard());
        assert!(LicenseError::InvalidKey.is_hard());
        assert!(LicenseError::Spoofed.is_hard());
        assert!(LicenseError::DeviceSlotTaken.is_hard());
    }

    // ── protocol client over a fake transport ───────────────────────

    struct FakeHttp {
        responses: RefCell<Vec<Result<(u16, String), String>>>,
        urls: RefCell<Vec<String>>,
        headers: RefCell<Vec<Vec<(String, String)>>>,
        bodies: RefCell<Vec<String>>,
    }

    impl FakeHttp {
        fn with(rs: Vec<Result<(u16, String), String>>) -> Self {
            Self {
                responses: RefCell::new(rs),
                urls: RefCell::new(Vec::new()),
                headers: RefCell::new(Vec::new()),
                bodies: RefCell::new(Vec::new()),
            }
        }
    }

    impl LicenseHttp for FakeHttp {
        fn post_json(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &str,
        ) -> Result<(u16, String), String> {
            self.urls.borrow_mut().push(url.to_string());
            self.headers.borrow_mut().push(headers.to_vec());
            self.bodies.borrow_mut().push(body.to_string());
            self.responses
                .borrow_mut()
                .pop()
                .expect("scripted response available")
        }
    }

    fn facts() -> DeviceFacts {
        DeviceFacts {
            platform: "windows".into(),
            hardware_hash: "e".repeat(64),
            hostname: "DEV-PC".into(),
            os_version: "Win 11".into(),
            app_version: "0.1.0".into(),
            comp_machine: Some("a".repeat(64)),
            comp_volume: Some("b".repeat(64)),
            comp_cpu: Some("c".repeat(64)),
            cpu_brand: Some("Intel Core i7-1260P".into()),
            ram_mb: Some(16_384),
            machine_model: Some("Dell Inc. XPS 15 9520".into()),
            baseboard_serial: Some("BX24RTK81".into()),
            firmware_uuid: Some("4c4c4544-0042-4e10-8032-b2c04f475030".into()),
            bios_version: Some("DELL A08".into()),
            cpu_cores: Some(16),
            arch: Some("x86_64".into()),
            comp_board: Some("d".repeat(64)),
            comp_firmware: Some("f".repeat(64)),
        }
    }

    fn fixture_token(hw: &str, key_hash: &str, now: i64) -> String {
        sign_fixture(
            &claims_json(hw, key_hash, now - 60, now + 14 * 86_400, None),
            TEST_SEED_HEX,
        )
    }

    #[test]
    fn activate_verifies_signature_before_trusting() {
        let now = NOW;
        let f = facts();
        let token = fixture_token(&f.hardware_hash, &sha256_hex("DBK"), now);
        let body = serde_json::json!({
            "token": token,
            "license": { "tier": "lifetime", "name": "Alex", "email": "a@b.c", "expiresAt": null }
        });
        // 200 + GENUINE signature → accepted, and the HMAC headers were sent.
        let http = FakeHttp::with(vec![Ok((200, body.to_string()))]);
        let api = LicenseApi::with_secret(http, "a".repeat(64));
        let dto = api.activate("DBK", &f).unwrap();
        assert_eq!(dto.token, token);
        let headers = api.http.headers.borrow();
        let last = headers.last().unwrap();
        assert!(last.iter().any(|(k, _)| k == "x-db-signature"));
        assert!(last.iter().any(|(k, _)| k == "x-db-nonce"));
        assert!(last.iter().any(|(k, _)| k == "x-db-timestamp"));
        let urls = api.http.urls.borrow();
        assert!(urls[0].ends_with("/v1/activate"));

        // 200 + a token signed by a DIFFERENT key (spoofed server) →
        // activate() hands back the dto; verification happens in
        // `apply_entitlement` — the spoof rejection is covered by the
        // token matrix above. This test pins the transport contract.
    }

    #[test]
    fn network_failures_map_to_network_error() {
        let http = FakeHttp::with(vec![Err("down".into())]);
        let api = LicenseApi::with_secret(http, "a".repeat(64));
        assert_eq!(
            api.validate("DBK", &facts()).unwrap_err(),
            LicenseError::Network
        );
    }

    #[test]
    fn error_bodies_map_to_typed_errors() {
        for (status, code, expected) in [
            (404, "KEY_NOT_FOUND", LicenseError::InvalidKey),
            (403, "KEY_REVOKED", LicenseError::Inactive),
            (409, "DEVICE_SLOT_TAKEN", LicenseError::DeviceSlotTaken),
            (429, "RATE_LIMITED", LicenseError::RateLimited),
        ] {
            let body = serde_json::json!({ "ok": false, "code": code, "message": "x" });
            let http = FakeHttp::with(vec![Ok((status, body.to_string()))]);
            let api = LicenseApi::with_secret(http, "a".repeat(64));
            assert_eq!(api.validate("DBK", &facts()).unwrap_err(), expected);
        }
    }

    #[test]
    fn deactivate_is_always_ok_on_2xx_and_4xx() {
        for status in [200u16, 204, 404, 403] {
            let http = FakeHttp::with(vec![Ok((status, String::new()))]);
            let api = LicenseApi::with_secret(http, "a".repeat(64));
            assert!(api.deactivate("DBK", &facts()).is_ok());
        }
    }

    // ── v2: the full device claim on the wire ───────────────────────

    /// The v2 claim body carries every fact (the server-side wipe fix
    /// needs validate to carry the SAME claim as activate) and skips
    /// None fields (older clients' sparse claims stay byte-identical).
    #[test]
    fn claim_body_carries_full_facts_and_skips_absent() {
        let full = facts();
        let body = claim_body("DBK", &full);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["licenseKey"], "DBK");
        assert_eq!(v["hardwareHash"], "e".repeat(64));
        assert_eq!(v["hostname"], "DEV-PC");
        assert_eq!(v["osVersion"], "Win 11");
        assert_eq!(v["appVersion"], "0.1.0");
        assert_eq!(v["compMachine"], "a".repeat(64));
        assert_eq!(v["compVolume"], "b".repeat(64));
        assert_eq!(v["compCpu"], "c".repeat(64));
        assert_eq!(v["cpuBrand"], "Intel Core i7-1260P");
        assert_eq!(v["ramMb"], 16_384);
        assert_eq!(v["machineModel"], "Dell Inc. XPS 15 9520");
        assert_eq!(v["baseboardSerial"], "BX24RTK81");
        assert_eq!(v["firmwareUuid"], "4c4c4544-0042-4e10-8032-b2c04f475030");
        assert_eq!(v["biosVersion"], "DELL A08");
        assert_eq!(v["cpuCores"], 16);
        assert_eq!(v["arch"], "x86_64");
        assert_eq!(v["compBoard"], "d".repeat(64));
        assert_eq!(v["compFirmware"], "f".repeat(64));

        // Sparse claim: absent optional fields stay OFF the wire.
        let sparse = DeviceFacts {
            comp_machine: None,
            comp_volume: None,
            comp_cpu: None,
            cpu_brand: None,
            ram_mb: None,
            machine_model: None,
            baseboard_serial: None,
            firmware_uuid: None,
            bios_version: None,
            cpu_cores: None,
            arch: None,
            comp_board: None,
            comp_firmware: None,
            ..facts()
        };
        let body = claim_body("DBK", &sparse);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.get("compMachine").is_none());
        assert!(v.get("ramMb").is_none());
        assert!(v.get("machineModel").is_none());
        assert!(v.get("baseboardSerial").is_none());
        assert!(v.get("firmwareUuid").is_none());
        assert!(v.get("cpuCores").is_none());
        assert!(v.get("arch").is_none());
        assert!(v.get("compBoard").is_none());
        assert!(v.get("compFirmware").is_none());
        assert_eq!(v["hostname"], "DEV-PC");
    }

    /// validate() sends the SAME full claim as activate() — pin the
    /// v2 wire contract (the v1 validate omitted the facts and the
    /// server then wiped its stored rows — the owner-reported bug).
    #[test]
    fn validate_sends_the_full_claim() {
        let token = fixture_token(&facts().hardware_hash, &sha256_hex("DBK"), NOW);
        let body = serde_json::json!({ "token": token });
        let http = FakeHttp::with(vec![Ok((200, body.to_string()))]);
        let api = LicenseApi::with_secret(http, "a".repeat(64));
        assert!(api.validate("DBK", &facts()).is_ok());
        let sent = api.http.bodies.borrow()[0].clone();
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["licenseKey"], "DBK");
        assert_eq!(v["hardwareHash"], "e".repeat(64));
        assert_eq!(v["hostname"], "DEV-PC");
        assert_eq!(v["osVersion"], "Win 11");
        assert_eq!(v["appVersion"], "0.1.0");
        assert_eq!(v["compMachine"], "a".repeat(64));
        assert_eq!(v["cpuBrand"], "Intel Core i7-1260P");
        assert_eq!(v["ramMb"], 16_384);
        assert_eq!(v["machineModel"], "Dell Inc. XPS 15 9520");
    }

    /// activate() carries the same claim shape (one wire, two routes).
    #[test]
    fn activate_sends_the_full_claim() {
        let token = fixture_token(&facts().hardware_hash, &sha256_hex("DBK"), NOW);
        let body = serde_json::json!({ "token": token });
        let http = FakeHttp::with(vec![Ok((200, body.to_string()))]);
        let api = LicenseApi::with_secret(http, "a".repeat(64));
        assert!(api.activate("DBK", &facts()).is_ok());
        let sent = api.http.bodies.borrow()[0].clone();
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(v["licenseKey"], "DBK");
        assert_eq!(v["machineModel"], "Dell Inc. XPS 15 9520");
        assert_eq!(v["ramMb"], 16_384);
    }

    /// Component hashing: distinct labels → distinct digests; stable
    /// across calls; None propagates (unreadable input).
    #[test]
    fn component_hashing_is_labeled_stable_and_none_safe() {
        let m = component_hash("machine:", Some("guid-1".into())).unwrap();
        let v = component_hash("volume:", Some("42".into())).unwrap();
        let c = component_hash("cpu:", Some("Intel".into())).unwrap();
        assert_eq!(m.len(), 64);
        assert_ne!(m, v);
        assert_ne!(v, c);
        assert_eq!(
            m,
            component_hash("machine:", Some("guid-1".into())).unwrap()
        );
        assert!(component_hash("machine:", None).is_none());
        // The label is part of the digest (same value, different label).
        assert_ne!(m, component_hash("other:", Some("guid-1".into())).unwrap());
    }

    // ── codecs ──────────────────────────────────────────────────────

    #[test]
    fn b64url_roundtrips_rfc4648_vectors() {
        // RFC 4648 §10 test vectors (base64url, unpadded).
        let vectors: [(&str, &str); 5] = [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(b64url_decode(encoded).unwrap(), plain.as_bytes());
            // Canonical encoding check (no leftover bits accepted).
            assert_eq!(&b64url(plain.as_bytes()), encoded);
        }
        // url-safe alphabet + padded input accepted.
        assert_eq!(b64url_decode("Zm9vYg==").unwrap(), b"foob".to_vec());
        // Invalid characters + non-canonical lengths rejected.
        assert!(b64url_decode("a+bc").is_none());
        assert!(b64url_decode("abcde").is_none());
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(hex_of(&[0x00, 0xff, 0x10]), "00ff10");
        assert_eq!(hex_of(&[0xab; 32]).len(), 64);
        assert_eq!(from_hex("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert_eq!(from_hex("00FF10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("xyz").is_none());
    }

    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for
        // nothing?" → 5bdcc146bf60754e6a042426089575c75a003f089d2739839
        // dec58fb9691e... (first 32 hex chars checked).
        let key_hex = hex_of(b"Jefe");
        let out = hmac_hex(&key_hex, "what do ya want for nothing?");
        assert!(out.starts_with("5bdcc146bf60754e"));
        assert_eq!(out.len(), 64);
    }

    #[test]
    fn sha256_hex_known_vector() {
        // sha256("abc") begins ba7816bf.
        assert!(sha256_hex("abc").starts_with("ba7816bf"));
    }

    #[test]
    fn api_base_env_override() {
        std::env::set_var("DISKGENIE_LICENSE_API", "http://127.0.0.1:8787/");
        assert_eq!(api_base(), "http://127.0.0.1:8787/");
        std::env::remove_var("DISKGENIE_LICENSE_API");
        assert_eq!(api_base(), LICENSE_API_BASE);
    }
}
