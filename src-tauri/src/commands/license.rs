//! License commands (docs/LICENSING-ARCHITECTURE.md §6): thin IPC over
//! [`crate::license`], the 24-hour revalidation scheduler, and the
//! hard command-layer gate every privileged operation re-checks.
//!
//! STRICT posture semantics (owner decision, session 10): there is no
//! free tier — unlicensed/degraded users keep the Monitor tab ONLY;
//! `require_licensed` refuses everything else at the Rust boundary
//! (the UI lock is a convenience, this is the gate).

use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::license::{
    self, EntitlementDto, LicenseApi, LicenseError, LicensePosture, LicenseState, ReqwestLicense,
    TokenClaims, GRACE_DAYS, LICENSE_PUBLIC_KEY_HEX, LICENSE_PURCHASE_URL, VALIDATION_INTERVAL_S,
};

/// Error marker prefix the command layer appends so the WebView can
/// detect gate refusals and open the activation flow (defense in depth
/// behind the UI lock). The frontend (state/license.ts) greps the same
/// prefixes.
pub const GATE_ACTIVATION_REQUIRED: &str = "ACTIVATION_REQUIRED";

/// The stale-license marker (grace window exhausted).
pub const GATE_STALE: &str = "LICENSE_STALE";

/// The managed license manager (state + scheduler control).
pub struct LicenseManager {
    state: Mutex<LicenseState>,
}

/// Managed state constructor: restores the DPAPI/Keychain-cached state.
/// Under the `ci-license-sim` feature (screenshots workflow ONLY —
/// never enabled in NSIS/MSIX production builds) the env
/// `DISKGENIE_LICENSE_SIM=1` seeds a simulated activated state so the
/// full app tour can run; `license_sim_set` flips it at runtime.
#[must_use]
pub fn license_manager() -> LicenseManager {
    // `mut` only for the feature-gated sim seeding below (the allow
    // keeps non-sim production builds warning-clean).
    #[allow(unused_mut)]
    let mut state = license::dpapi::load().unwrap_or_default();
    #[cfg(feature = "ci-license-sim")]
    if std::env::var("DISKGENIE_LICENSE_SIM").as_deref() == Ok("1") {
        let now = now_unix();
        state = sim_state(now);
    }
    LicenseManager {
        state: Mutex::new(state),
    }
}

/// The status payload (`license_status` + `license-changed` events).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseStatusView {
    pub posture: String,
    pub is_pro: bool,
    pub tier: String,
    /// Display identity (empty before activation).
    pub customer_name: String,
    pub customer_email: String,
    /// License expiry (unix seconds; 0 = lifetime).
    pub license_expires_at: i64,
    /// Days left in the offline grace window (0 otherwise).
    pub grace_days_left: i64,
    /// The purchase URL (one source of truth for the UI button).
    pub purchase_url: String,
    /// CI simulation marker (screenshots workflow only).
    pub simulated: bool,
}

fn view(state: &LicenseState, now: i64) -> LicenseStatusView {
    let (posture, days) = match license::posture(state, now) {
        LicensePosture::Unlicensed => ("unlicensed", 0),
        LicensePosture::Pro => ("pro", 0),
        LicensePosture::Grace { days_left } => ("grace", days_left),
        LicensePosture::Degraded => ("degraded", 0),
    };
    LicenseStatusView {
        posture: posture.to_string(),
        is_pro: posture == "pro" || posture == "grace",
        tier: state.tier.clone(),
        customer_name: state.customer_name.clone(),
        customer_email: state.customer_email.clone(),
        license_expires_at: state.license_expires_at,
        grace_days_left: days,
        purchase_url: LICENSE_PURCHASE_URL.to_string(),
        simulated: state.simulated,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Normalize + structurally validate a license key
/// (`DB` + 20 Crockford chars; docs §3.3). Returns `None` on a bad
/// shape (the caller surfaces the typed copy).
#[must_use]
pub fn normalize_key(raw: &str) -> Option<String> {
    let normalized: String = raw
        .trim()
        .to_uppercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let ok = normalized.len() == 22
        && normalized.starts_with("DB")
        && normalized
            .chars()
            .skip(2)
            .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c));
    ok.then_some(normalized)
}

/// The platform identifier the server binds slots by.
fn platform_string() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else {
        "windows"
    }
}

/// sha256 hex over the normalized key (the token's `key` binding).
fn key_hash_of(normalized: &str) -> String {
    crate::license::sha256_hex(normalized)
}

/// Current license status (posture/gating data for the UI).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn license_status(mgr: State<'_, LicenseManager>) -> LicenseStatusView {
    let state = mgr.state.lock().clone();
    view(&state, now_unix())
}

/// Activate a license key on this device (doc §4): collect facts →
/// server activate → **locally verify the Ed25519-signed token** (a
/// spoofed/mimicked server cannot pass this) → persist + notify.
/// Transport failures retry twice with backoff before surfacing.
///
/// # Errors
/// The typed [`LicenseError`] copy (invalid key / device slot / expired
/// / spoofed response) — the user-readable reason, never a silent
/// fallback.
#[tauri::command]
pub async fn activate_license(
    key: String,
    mgr: State<'_, LicenseManager>,
    app: AppHandle,
) -> Result<LicenseStatusView, String> {
    // ALL blocking work (facts + network retries + Keychain persist)
    // runs on the blocking pool: a SYNC command executes on the MAIN
    // thread, and the 30 s-timeout network + Keychain writes froze the
    // whole window for their entire duration (the mac license E2E's
    // permanently-"Activating" dialog — captured frame-perfect).
    let app_blocking = app.clone();
    let state =
        tauri::async_runtime::spawn_blocking(move || activate_blocking(&key, &app_blocking))
            .await
            .map_err(|e| format!("activation task failed: {e}"))??;
    let now = now_unix();
    let v = view(&state, now);
    *mgr.state.lock() = state;
    let _ = app.emit("license-changed", &v);
    Ok(v)
}

/// The blocking core of [`activate_license`] (runs off the main
/// thread): normalize → facts → server activate with retries → local
/// Ed25519 verification → Keychain persist. Returns the next state.
///
/// # Errors
/// The typed [`LicenseError`] copy (invalid key / device slot /
/// expired / spoofed response) — the user-readable reason, never a
/// silent fallback.
fn activate_blocking(key: &str, app: &AppHandle) -> Result<LicenseState, String> {
    let t0 = std::time::Instant::now();
    let Some(normalized) = normalize_key(key) else {
        return Err(LicenseError::InvalidKey.to_string());
    };
    eprintln!(
        "[bench] license activate start key_last4={}",
        &normalized[normalized.len().saturating_sub(4)..]
    );
    let facts = license::collect_device_facts()?;
    eprintln!(
        "[bench] license facts collected ms={}",
        t0.elapsed().as_millis()
    );
    let hw = facts.hardware_hash.clone();
    let http = ReqwestLicense::new()?;
    let api = LicenseApi::new(http);

    // Retry the transport (lightning-fast when healthy; two quick
    // retries with jittered backoff when the edge blips — doc §4
    // "error management"). RATE_LIMITED deliberately does NOT retry:
    // each attempt consumes another window slot, and the hourly window
    // cannot expire inside a seconds-scale retry loop — the typed copy
    // ("wait a minute") is the honest response.
    let mut attempt = 0u32;
    let mut outcome = api.activate(&normalized, &facts);
    while matches!(
        &outcome,
        Err(e) if matches!(e, LicenseError::Network | LicenseError::ServerError) && attempt < 2
    ) {
        attempt += 1;
        let jitter = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::from(d.subsec_nanos() % 150));
        std::thread::sleep(std::time::Duration::from_millis(
            250 * u64::from(attempt) + jitter,
        ));
        outcome = api.activate(&normalized, &facts);
    }
    eprintln!(
        "[bench] license network done attempts={} ms={}",
        attempt + 1,
        t0.elapsed().as_millis()
    );
    let dto = outcome.map_err(|e| {
        if let Some(an) = app.try_state::<crate::analytics::Analytics>() {
            an.capture(
                "license_activate_failed",
                &[("retry", serde_json::json!(attempt))],
            );
        }
        e.to_string()
    })?;

    // The anti-spoofing gate: verify the server's signed token against
    // the embedded public key + this machine's fingerprint before
    // trusting ANYTHING in the response.
    let now = now_unix();
    let claims = verify_entitlement(&dto, &normalized, &hw, now).map_err(|e| {
        if let Some(an) = app.try_state::<crate::analytics::Analytics>() {
            an.capture("license_activate_spoofed", &[]);
        }
        e.to_string()
    })?;

    let mut state = state_from_entitlement(&dto, &claims, &normalized, &hw, now);
    state.last_known_good = now;
    eprintln!(
        "[bench] license token verified ms={}",
        t0.elapsed().as_millis()
    );
    license::dpapi::save(&state)?;
    eprintln!("[bench] license persisted ms={}", t0.elapsed().as_millis());
    Ok(state)
}

/// Verify a fresh entitlement: signature, key binding, hardware
/// binding, platform, windows.
fn verify_entitlement(
    dto: &EntitlementDto,
    normalized_key: &str,
    hw: &str,
    now: i64,
) -> Result<TokenClaims, LicenseError> {
    license::verify_token(
        &dto.token,
        LICENSE_PUBLIC_KEY_HEX,
        &key_hash_of(normalized_key),
        hw,
        platform_string(),
        now,
    )
}

/// Build the persisted state from a verified entitlement.
fn state_from_entitlement(
    dto: &EntitlementDto,
    claims: &TokenClaims,
    normalized_key: &str,
    hw: &str,
    now: i64,
) -> LicenseState {
    LicenseState {
        license_key: normalized_key.to_string(),
        hardware_id: hw.to_string(),
        platform: platform_string().to_string(),
        tier: claims.tier.clone(),
        customer_name: claims.name.clone(),
        customer_email: claims.email.clone(),
        license_expires_at: claims.lexp.unwrap_or(0),
        token: dto.token.clone(),
        token_exp: claims.exp,
        activated_at: now,
        last_validated_at: now,
        last_known_good: 0,
        simulated: false,
    }
}

/// Deactivate this device (frees the platform slot server-side) and
/// clear the local cache. Network failure still clears locally (the
/// slot frees on the next server-side housekeeping or admin reset).
///
/// # Errors
/// String error when the remote deactivate fails with a hard error.
#[tauri::command]
pub async fn deactivate_license(
    mgr: State<'_, LicenseManager>,
    app: AppHandle,
) -> Result<(), String> {
    // The server-side slot free is network work — off the main thread
    // (see activate_license for the freeze this prevents).
    let current = mgr.state.lock().clone();
    let hard_err = tauri::async_runtime::spawn_blocking(move || deactivate_blocking(&current))
        .await
        .map_err(|e| format!("deactivation task failed: {e}"))?;
    hard_err?;
    let now = now_unix();
    *mgr.state.lock() = LicenseState::default();
    let v = view(&LicenseState::default(), now);
    let _ = app.emit("license-changed", &v);
    Ok(())
}

/// The blocking core of [`deactivate_license`]: frees the server-side
/// platform slot (network) and clears the Keychain item.
///
/// # Errors
/// String error when the remote deactivate fails with a hard error.
fn deactivate_blocking(state: &LicenseState) -> Result<(), String> {
    if state.license_key.is_empty() {
        return Ok(());
    }
    if !state.simulated {
        if let (Ok(facts), Ok(http)) = (license::collect_device_facts(), ReqwestLicense::new()) {
            let api = LicenseApi::new(http);
            if let Err(e) = api.deactivate(&state.license_key, &facts) {
                if e.is_hard() {
                    return Err(e.to_string());
                }
            }
        }
    }
    license::dpapi::clear();
    Ok(())
}

/// Run one validation NOW (the Pro status card's "Validate now") and
/// return the resulting status. Network failures leave the grace path
/// intact (doc §6 posture semantics).
///
/// # Errors
/// String error on a hard validation failure (revoked / expired /
/// device mismatch); soft failures keep the grace path and return Ok.
#[tauri::command]
pub async fn validate_now(
    app: AppHandle,
    mgr: State<'_, LicenseManager>,
) -> Result<LicenseStatusView, String> {
    // One validation NOW (the Pro status card's "Validate now") — the
    // network + Keychain work runs off the main thread (see
    // activate_license).
    let current = mgr.state.lock().clone();
    let app_blocking = app.clone();
    let (next, err) =
        tauri::async_runtime::spawn_blocking(move || validate_cycle(&current, &app_blocking))
            .await
            .map_err(|e| format!("validation task failed: {e}"))?;
    let now = now_unix();
    let v = view(&next, now);
    *mgr.state.lock() = next;
    let _ = app.emit("license-changed", &v);
    if let Some(e) = err {
        return Err(e);
    }
    Ok(v)
}

/// One validation cycle (PURE state application over the verified
/// protocol outcome — the unit-tested seam):
/// * fresh verified token → refresh timestamps/token
/// * transport/parse failure → grace continues (frozen)
/// * a response that FAILS local verification (spoofed server) →
///   grace continues (a mimic cannot extend anything; the token's own
///   `exp` is the hard stop — the honest choice during key rotation,
///   where a stale app build would otherwise mass-deactivate)
/// * explicit hard errors (revoked / expired / device mismatch) →
///   local deactivate
fn validate_and_apply(
    app: &AppHandle,
    mgr: &LicenseManager,
) -> (LicenseStatusView, Option<String>) {
    // The scheduler's path (its own thread — the blocking work is
    // already off the main thread there).
    let current = mgr.state.lock().clone();
    let (next, err) = validate_cycle(&current, app);
    let now = now_unix();
    let v = view(&next, now);
    *mgr.state.lock() = next;
    let _ = app.emit("license-changed", &v);
    (v, err)
}

/// The blocking core of one validation cycle (PURE function of the
/// input state — network + verification + persistence, no locks):
/// * fresh verified token → refresh timestamps/token
/// * transport/parse failure → grace continues (frozen)
/// * a response that FAILS local verification (spoofed server) →
///   grace continues (a mimic cannot extend anything; the token's own
///   `exp` is the hard stop — the honest choice during key rotation,
///   where a stale app build would otherwise mass-deactivate)
/// * explicit hard errors (revoked / expired / device mismatch) →
///   local deactivate
fn validate_cycle(state: &LicenseState, app: &AppHandle) -> (LicenseState, Option<String>) {
    let mut state = state.clone();
    if state.license_key.is_empty() || state.simulated {
        return (state, None);
    }
    let now = now_unix();
    let mut error = None;
    let (facts, hw) = match license::collect_device_facts() {
        Ok(f) => (f.clone(), f.hardware_hash),
        Err(e) => {
            // Fingerprint unreadable (WMI-hobbled machine?): keep the
            // grace path; the token expiry is the backstop.
            let _ = e;
            return (state, None);
        }
    };
    let result = match ReqwestLicense::new() {
        Ok(http) => LicenseApi::new(http).validate(&state.license_key, &facts),
        Err(_) => Err(LicenseError::Network),
    };
    // Engine telemetry (doc 07 §4) — no key material.
    if let Some(an) = app.try_state::<crate::analytics::Analytics>() {
        an.capture(
            "license_validate_result",
            &[
                ("valid", serde_json::json!(result.is_ok())),
                (
                    "network",
                    serde_json::json!(matches!(result, Err(LicenseError::Network))),
                ),
            ],
        );
    }
    match result {
        Ok(dto) => {
            // Spoofed/rotation-mismatch (the Err arm) is deliberately
            // soft — grace continues; the token's own `exp` is the hard
            // stop (the honest choice during key rotation).
            if let Ok(claims) = verify_entitlement(&dto, &state.license_key, &hw, now) {
                // Refresh in place (activation identity stays). All
                // display data comes from the VERIFIED claims.
                state.tier.clone_from(&claims.tier);
                state.customer_name.clone_from(&claims.name);
                state.customer_email.clone_from(&claims.email);
                state.license_expires_at = claims.lexp.unwrap_or(0);
                state.token.clone_from(&dto.token);
                state.token_exp = claims.exp;
                state.last_validated_at = now;
                state.last_known_good = state.last_known_good.max(now);
            }
        }
        Err(e) if e.is_hard() => {
            // Revoked / expired / device-mismatch: hard → local clear.
            license::dpapi::clear();
            error = Some(e.to_string());
            state = LicenseState::default();
        }
        // Network / server error: grace continues (timestamps frozen).
        Err(_) => {}
    }
    let _ = license::dpapi::save(&state);
    (state, error)
}

/// Start the license scheduler: an immediate launch validation + the
/// 24-hour revalidation loop (doc §6). Called from `setup`.
pub fn start_scheduler(app: &AppHandle) {
    let handle = Arc::new(app.clone());
    std::thread::Builder::new()
        .name("db-license".into())
        .spawn(move || {
            let mgr = handle.state::<LicenseManager>();
            let _ = validate_and_apply(&handle, &mgr);
            loop {
                // Sleep one validation interval; exit quietly when the
                // app is closing (emit failures end the loop naturally).
                std::thread::sleep(std::time::Duration::from_secs(
                    u64::try_from(VALIDATION_INTERVAL_S).unwrap_or(86_400),
                ));
                let _ = validate_and_apply(&handle, &mgr);
            }
        })
        .expect("license scheduler thread");
}

// ============================================================================
// The command-layer gate (defense in depth behind the UI lock)
// ============================================================================

/// The hard gate every privileged command re-checks (doc §2 L6):
/// PRO/grace pass; unlicensed and degraded REFUSE. The error string
/// carries a greppable marker the WebView uses to open the activation
/// flow (UI lock + this gate protect each other).
///
/// # Errors
/// Typed user copy with the `ACTIVATION_REQUIRED` / `LICENSE_STALE`
/// marker prefix.
pub fn require_licensed(mgr: &LicenseManager, now: i64) -> Result<(), String> {
    let state = mgr.state.lock();
    match license::posture(&state, now) {
        LicensePosture::Pro | LicensePosture::Grace { .. } => Ok(()),
        LicensePosture::Degraded => Err(format!(
            "{GATE_STALE}. DiskGenie couldn't verify your license for over {GRACE_DAYS} days. Reconnect to restore Pro features."
        )),
        LicensePosture::Unlicensed => Err(format!(
            "{GATE_ACTIVATION_REQUIRED}. Activate DiskGenie Pro to use this."
        )),
    }
}

// ============================================================================
// CI simulation (screenshots workflow only; never in production builds)
// ============================================================================

/// A simulated activated state for the tour (name/email/active-till
/// cards; marked `simulated: true` so the status view says so).
#[cfg(feature = "ci-license-sim")]
fn sim_state(now: i64) -> LicenseState {
    LicenseState {
        license_key: "DBSIM0CI0TOUR0SIM0KEY0".to_string(),
        hardware_id: String::new(),
        platform: platform_string().to_string(),
        tier: "lifetime".to_string(),
        customer_name: "Alex Morgan".to_string(),
        customer_email: "alex@diskgenie.app".to_string(),
        license_expires_at: 0,
        token: "sim".to_string(),
        token_exp: now + 365 * 86_400,
        activated_at: now - 32 * 86_400,
        last_validated_at: now,
        last_known_good: now,
        simulated: true,
    }
}

/// Tour hook: flip the simulated license state. ALWAYS registered (a
/// cfg-attribute inside `generate_handler!` is macro-territory we
/// avoid), but the body is feature-gated: NSIS/MSIX production builds
/// compile a hard refusal — the simulation cannot flip anything.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // State extraction is the tauri command contract
pub fn license_sim_set(
    mode: &str,
    mgr: State<'_, LicenseManager>,
    app: AppHandle,
) -> Result<LicenseStatusView, String> {
    #[cfg(not(feature = "ci-license-sim"))]
    {
        // Production build: no simulation surface (defense in depth —
        // the seeding in license_manager is compiled out too).
        let _ = (mode, &mgr, &app);
        Err("license simulation is not available in this build".to_string())
    }
    #[cfg(feature = "ci-license-sim")]
    {
        let now = now_unix();
        let next = if mode == "pro" {
            sim_state(now)
        } else {
            LicenseState::default()
        };
        let v = view(&next, now);
        *mgr.state.lock() = next;
        let _ = app.emit("license-changed", &v);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_normalization_accepts_messy_input() {
        assert_eq!(
            normalize_key(" db-xk2m9-qf3p8nr4t-2vw6y4 ").as_deref(),
            Some("DBXK2M9QF3P8NR4T2VW6Y4")
        );
        assert_eq!(
            normalize_key("DB.XK2M9 qf3p8nr4t2vw6y4").as_deref(),
            Some("DBXK2M9QF3P8NR4T2VW6Y4")
        );
    }

    #[test]
    fn key_normalization_rejects_wrong_shapes() {
        // Wrong length / prefix / confusable letters.
        assert!(normalize_key("DB-XK2M9-QF3P8").is_none());
        assert!(normalize_key(&format!("AB{}", "X".repeat(20))).is_none());
        assert!(normalize_key(&format!("DB{}", "ILOU".repeat(5))).is_none());
        assert!(normalize_key("").is_none());
    }

    #[test]
    fn view_shapes_by_posture() {
        let now = 1_790_000_000;
        let mut s = LicenseState {
            license_key: "K".into(),
            tier: "lifetime".into(),
            customer_name: "Alex Morgan".into(),
            customer_email: "alex@example.com".into(),
            last_validated_at: now,
            token_exp: now + 14 * 86_400,
            ..Default::default()
        };
        let v = view(&s, now);
        assert_eq!(v.posture, "pro");
        assert!(v.is_pro);
        assert_eq!(v.customer_name, "Alex Morgan");
        assert_eq!(v.license_expires_at, 0);
        assert_eq!(v.purchase_url, LICENSE_PURCHASE_URL);
        // 10 days stale with a token issued 10 days ago (14-day window,
        // 4 left) → grace 4.
        s.last_validated_at = now - 10 * 86_400;
        s.token_exp = now + 4 * 86_400;
        let v = view(&s, now);
        assert_eq!(v.posture, "grace");
        assert_eq!(v.grace_days_left, 4);
        assert!(v.is_pro, "grace keeps pro features (doc §6)");
        // Token exhausted → degraded → locked.
        s.token_exp = now - 1;
        let v = view(&s, now);
        assert_eq!(v.posture, "degraded");
        assert!(!v.is_pro);
        // Default → unlicensed.
        let v = view(&LicenseState::default(), now);
        assert_eq!(v.posture, "unlicensed");
        assert!(!v.is_pro);
        assert_eq!(v.customer_name, "");
    }

    #[test]
    fn gate_rules() {
        let now = 1_790_000_000;
        // Unlicensed: refused with the activation marker.
        let free = LicenseManager::state_for(&LicenseState::default());
        let err = require_licensed(&free, now).unwrap_err();
        assert!(err.starts_with("ACTIVATION_REQUIRED"));
        // Pro: pass.
        let pro = LicenseState {
            license_key: "K".into(),
            last_validated_at: now,
            token_exp: now + 86_400,
            ..Default::default()
        };
        assert!(require_licensed(&LicenseManager::state_for(&pro), now).is_ok());
        // Grace: pass (offline, inside the window).
        let grace = LicenseState {
            license_key: "K".into(),
            last_validated_at: now - 10 * 86_400,
            token_exp: now + 4 * 86_400,
            ..Default::default()
        };
        assert!(require_licensed(&LicenseManager::state_for(&grace), now).is_ok());
        // Degraded: refused with the stale marker.
        let deg = LicenseState {
            license_key: "K".into(),
            last_validated_at: now - 20 * 86_400,
            token_exp: now - 1,
            ..Default::default()
        };
        let err = require_licensed(&LicenseManager::state_for(&deg), now).unwrap_err();
        assert!(err.starts_with("LICENSE_STALE"));
    }

    /// State-from-entitlement carries the display identity + windows.
    #[test]
    fn state_from_entitlement_maps_fields() {
        let dto = EntitlementDto {
            token: "tok".into(),
        };
        let claims = TokenClaims {
            iss: "db-license".into(),
            ver: 1,
            jti: "j".into(),
            iat: 1_790_000_000,
            exp: 1_790_864_000,
            key: "k".into(),
            tier: "yearly".into(),
            name: "Renee Okafor".into(),
            email: "renee@example.com".into(),
            hw: "h".into(),
            plat: "windows".into(),
            lexp: Some(1_800_000_000),
            kid: None,
            aud: None,
        };
        let s = state_from_entitlement(&dto, &claims, "DBK", "hw", 1_790_000_000);
        assert_eq!(s.license_key, "DBK");
        assert_eq!(s.customer_name, "Renee Okafor");
        assert_eq!(s.license_expires_at, 1_800_000_000);
        assert_eq!(s.token_exp, 1_790_864_000);
        assert!(!s.simulated);
    }

    impl LicenseManager {
        /// Test constructor from a frozen state (the gate + view read
        /// posture through the same path as production).
        fn state_for(state: &LicenseState) -> Self {
            LicenseManager {
                state: Mutex::new(state.clone()),
            }
        }
    }
}
