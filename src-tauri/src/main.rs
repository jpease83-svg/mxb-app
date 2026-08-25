Warning: truncated output (original token count: 94682)
Total output lines: 8820

// Prevents an additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod antidebug;
mod bikefiles;
mod bikeswap;
mod bundle;
mod cancel;
mod cfg;
mod config;
mod cookie_session;
mod downloads;
mod dropzone;
mod edf;
mod fileshare;
mod frostmod;
mod frostmod_manage;
mod game;
mod gameproc;
mod gearrepair;
mod heightfield;
mod imgcache;
mod install;
mod integrity;
mod integritywatch;
mod library;
mod linkwalk;
mod logs;
mod lru;
mod memwatch;
mod modelswap;
mod mods;
mod modstate;
mod modwatch;
mod profilewatch;
mod mxb_fetch;
mod mxb_session;
mod overlay;
mod paint;
mod paintstudio;
mod paintwatch;
mod pkz;
/// Linux only: the Proton prefix the game runs in, and how to put a Windows program in it.
#[cfg(target_os = "linux")]
mod proton;
#[cfg(sidecar)]
mod sidecar;
mod presets;
mod paintsync;
mod reshade;
mod servers;
mod shop_catalog_session;
mod shop_credentials;
mod shop_fetch;
mod shop_installed;
mod shop_session;
mod soundmods;
mod texstore;
mod track;
mod upload;
mod vcruntime;
mod voice;
mod winehost;

use config::AppConfig;
use frostmod::ReloadOutcome;
use frostmod_manage::{FrostmodProcess, FrostmodStatus, InstallReport};
use library::InstalledMod;
use modwatch::ModWatcher;
use paintwatch::PaintWatcher;
// Decoding a paint's textures is per-texture CPU work over no shared state, and every path
// that does it wants the same treatment — so this sits here rather than in one function.
use rayon::prelude::*;
use profilewatch::ProfileWatcher;
use mods::mxb::WpModsSource;
use mods::{ModDetail, ModRating, ModSort, ModSource, ModSummary};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

/// The app window, as opposed to the transient ones the app opens alongside it (the
/// overlay, the mxb-mods.com clearance check, the shop login). `tauri.conf.json` declares
/// it without an explicit label, which is Tauri's default of `main`.
const MAIN_WINDOW: &str = "main";

/// The shop login WebView, opened on demand and closed once the session is captured.
const SHOP_LOGIN_WINDOW: &str = "shop-login";

/// Whether closing this window should park it in the tray rather than destroy it.
///
/// Only the main window. The transient ones are owned by the code that opened them, and
/// hiding one instead of closing it keeps its label registered for the life of the
/// process — the next attempt to build it then fails with "a webview with label `…`
/// already exists" and never opens again. A tester hit exactly that on the mxb-mods.com
/// clearance window: the first Cloudflare handshake worked, and every Retry afterwards
/// silently did nothing.
fn parks_in_tray(label: &str) -> bool {
    label == MAIN_WINDOW
}

/// Whether a window may make this IPC call.
///
/// Exists for the two windows that run a *remote* origin. [`mxb_fetch`] parks a hidden webview
/// on mxb-mods.com and hands catalog fetches back; [`shop_fetch`] parks one on
/// mxbikes-shop.com and hands the signed-in purchases page back. Giving each a capability is
/// what lets its page speak to us at all. But a capability grants IPC in general, and the
/// commands registered with `generate_handler!` are not covered by the permission ACL — so
/// without this, script on either site could call `create_config`, `install_mod` or anything
/// else the app exposes.
///
/// So both get an allowlist of exactly one call: emitting their result event. Every other
/// window is unaffected and keeps whatever its own capability file grants.
fn ipc_allowed(label: &str, command: &str) -> bool {
    if label != mxb_fetch::WINDOW && label != shop_fetch::WINDOW {
        return true;
    }
    command == "plugin:event|emit"
}

/// Whether the app is ready to use. Falls back to auto-detection when the config file
/// is missing, so the setup screen only appears when the MX Bikes folder genuinely
/// can't be found — not every time the saved config goes astray.
#[tauri::command]
fn is_configured(app: tauri::AppHandle) -> bool {
    config::load_or_detect(&app).is_some()
}

#[tauri::command]
fn get_config(app: tauri::AppHandle) -> AppConfig {
    config::load(&app).unwrap_or_default()
}

#[tauri::command]
fn create_config(
    app: tauri::AppHandle,
    watcher: State<ModWatcher>,
    config: AppConfig,
) -> Result<bool, String> {
    let mut cfg = config::finalize(config);
    // Detection came up empty and the user didn't pick a folder, so there is nothing to
    // save. Say so instead of writing a config with no folder in it: the setup screen
    // only reappears when `modsPath` is blank, so a silent save would bounce the user
    // straight back to the same screen with no explanation of what went wrong.
    if cfg.mods_path.trim().is_empty() {
        return Err(format!(
            "Couldn't find your {} folder automatically — choose it manually.",
            cfg.game().display
        ));
    }
    // Setup only sends the folders, so carry over first-run state from any config
    // that's already there — rewriting it would replay the intro and the tour.
    match config::load(&app) {
        Ok(prev) => {
            cfg.welcome_seen |= prev.welcome_seen;
            cfg.tour_done |= prev.tour_done;
            cfg.seen_version = prev.seen_version;
        }
        // Nothing came before: this install is new, and nothing in the version someone
        // just installed is news to them. Stamping it here is what keeps the release
        // showcase to upgrades — a first run gets the intro and the tour instead.
        Err(_) => cfg.seen_version = app.package_info().version.to_string(),
    }
    config::save(&app, &cfg).map_err(|e| format!("{e:#}"))?;
    // Begin watching straight away so a fresh setup doesn't need a restart before
    // manual downloads reload the game.
    if cfg.watch_mods_reload {
        modwatch::start(&app, &watcher, &cfg.mods_path);
    }
    Ok(true)
}

/// Run an mxb-mods.com call; if Cloudflare refuses it, run it again from inside a real
/// browser and keep using that transport for the rest of the session.
///
/// This used to earn a `cf_clearance` in a WebView and replay the cookie through the HTTP
/// client. A tester's log showed why that can't work: the challenge cleared in about a
/// second, the cookie was sent correctly, and Cloudflare served the interstitial to reqwest
/// anyway — a clearance is bound to the TLS fingerprint that earned it. So instead of moving
/// the cookie to the request, we move the request to the browser. See [`mxb_fetch`].
///
/// Once, not a loop: the second attempt is on a different transport, so if that is refused
/// too, trying a third time changes nothing. Only refusals a browser could plausibly satisfy
/// get this treatment — a 429 wants patience, not another request.
async fn with_clearance<T, F, Fut>(
    _app: &tauri::AppHandle,
    what: &str,
    op: F,
) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let err = match op().await {
        Ok(value) => return Ok(value),
        Err(err) => err,
    };
    match err.downcast_ref::<mods::mxb::Blocked>() {
        Some(blocked) if blocked.clearable() => {
            log::info!(
                "{what} blocked ({}) — retrying it from inside the WebView",
                blocked
                    .status
                    .map_or_else(|| "interstitial".to_string(), |s| s.to_string())
            );
        }
        // Not clearable, or not a block at all — a parse failure, a timeout, a 429.
        _ => {
            log::warn!("{what} failed and a browser wouldn't help: {err:#}");
            return Err(format!("{err:#}"));
        }
    }
    // Latches for the session: once this client's fingerprint has been refused on this
    // network, every later request would be refused the same way, so there is nothing to
    // gain from trying the HTTP client again first.
    mods::mxb::use_webview();
    match op().await {
        Ok(value) => {
            log::info!("{what} succeeded through the WebView");
            Ok(value)
        }
        // Report the browser's failure, not the original 403 — if the site is refusing a
        // real browser too, "open mxb-mods.com and hit Retry" is the wrong advice.
        Err(e) => {
            log::warn!("{what} failed through the WebView too: {e:#}");
            Err(format!("{e:#}"))
        }
    }
}

#[tauri::command]
async fn search_mods(
    app: tauri::AppHandle,
    query: String,
    category_id: u32,
    page: u32,
    sort: ModSort,
) -> Result<Vec<ModSummary>, String> {
    with_clearance(&app, "search", || {
        WpModsSource.search(&query, category_id, page, sort)
    })
    .await
}

/// Community scores for the mods currently on screen, keyed by post id. Ids the site
/// wouldn't answer for are left out rather than erroring — the cards just show no stars.
#[tauri::command]
async fn get_mod_ratings(ids: Vec<u64>) -> std::collections::HashMap<u64, ModRating> {
    mods::mxb::ratings(&ids).await
}

#[tauri::command]
async fn get_mod_detail(app: tauri::AppHandle, slug: String) -> Result<ModDetail, String> {
    with_clearance(&app, "mod detail", || WpModsSource.detail(&slug)).await
}

// ───────────────────────────── mxbikes-shop catalog ─────────────────────────────
//
// Browsing only. Nothing here installs or buys — the frontend opens the product page in
// the user's own browser. See `mods::shop_catalog`.

/// Whether this build has a shop credential at all. False hides the Shop tab entirely,
/// which is what forks and credential-less CI builds get.
#[tauri::command]
fn shop_catalog_available() -> bool {
    mods::shop_catalog::available()
}

/// Cheap and synchronous — it reports on what's already loaded and never fetches, so the
/// UI can poll it without cost.
#[tauri::command]
fn shop_catalog_status(app: tauri::AppHandle) -> mods::shop_catalog::ShopStatus {
    mods::shop_catalog::status(&app)
}

#[tauri::command]
async fn shop_catalog_categories(
    app: tauri::AppHandle,
) -> Result<Vec<mods::shop_catalog::ShopCategory>, String> {
    mods::shop_catalog::categories(&app)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn shop_catalog_search(
    app: tauri::AppHandle,
    query: String,
    category_id: Option<u64>,
    page: u32,
    sort: mods::shop_catalog::ShopSort,
    on_sale_only: bool,
) -> Result<mods::shop_catalog::ShopPage, String> {
    mods::shop_catalog::search(&app, &query, category_id, page, sort, on_sale_only)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn shop_catalog_detail(
    app: tauri::AppHandle,
    id: u64,
) -> Result<mods::shop_catalog::ShopModDetail, String> {
    mods::shop_catalog::detail(&app, id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Ignores the cache age and any `ETag` we hold — "Refresh" has to mean refresh, not
/// "ask politely and accept a 304".
#[tauri::command]
async fn shop_catalog_refresh(
    app: tauri::AppHandle,
) -> Result<mods::shop_catalog::ShopStatus, String> {
    mods::shop_catalog::force_refresh(&app)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn get_installed_mods(
    app: tauri::AppHandle,
    subpath: String,
) -> Result<Vec<InstalledMod>, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    library::scan_mods(&cfg.mods_path, &subpath).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn scan_library(
    app: tauri::AppHandle,
    subpath: String,
) -> Result<Vec<library::LibraryEntry>, String> {
    tauri::async_runtime::spawn_blocking(move || scan_library_blocking(app, subpath))
        .await
        .map_err(|e| format!("scan_library task failed: {e}"))?
}

fn scan_library_blocking(
    app: tauri::AppHandle,
    subpath: String,
) -> Result<Vec<library::LibraryEntry>, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    // ReShade presets aren't in the mods tree, so the scanner below would look in a folder
    // that doesn't exist and report every preset as not installed. Browse compares these
    // names against catalog titles to draw its "Installed" badge, so it has to be the real
    // list. See `reshade::status`.
    if reshade::is_reshade_subpath(&subpath) {
        return Ok(reshade::status(&cfg.reshade_dir())
            .presets
            .into_iter()
            .map(|p| library::LibraryEntry {
                modified: std::fs::metadata(&p.path)
                    .map(|m| library::mtime_ms(&m))
                    .unwrap_or(0),
                name: p.name,
                path: p.path,
                folder: String::new(),
                size: 0,
                kind: "loose".into(),
                category: "reshade".into(),
                parent: None,
            })
            .collect());
    }
    let sound_bikes = sound_bikes_of(&app);
    library::scan_library(&cfg.mods_path, &subpath, &sound_bikes, cfg.game()).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn scan_rider_targets(app: tauri::AppHandle) -> Result<library::RiderTargets, String> {
    tauri::async_runtime::spawn_blocking(move || scan_rider_targets_blocking(app))
        .await
        .map_err(|e| format!("scan_rider_targets task failed: {e}"))?
}

fn scan_rider_targets_blocking(app: tauri::AppHandle) -> Result<library::RiderTargets, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    Ok(library::scan_rider_targets(&cfg.mods_path))
}

/// Gear models the game can't reach where they are: files loose in an area root, or a package
/// buried a folder deep. See [`gearrepair`] for how each happened and what moves.
#[tauri::command]
async fn scan_gear_repairs(app: tauri::AppHandle) -> Result<Vec<gearrepair::GearRepair>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        Ok(gearrepair::plan(&cfg.mods_path))
    })
    .await
    .map_err(|e| format!("scan_gear_repairs task failed: {e}"))?
}

/// Carry out one repair, by the `id` its plan carries. Returns how many entries moved.
#[tauri::command]
async fn repair_gear(app: tauri::AppHandle, id: String) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        gearrepair::apply_one(&cfg.mods_path, &id).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("repair_gear task failed: {e}"))?
}

#[tauri::command]
async fn scan_bike_targets(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || scan_bike_targets_blocking(app))
        .await
        .map_err(|e| format!("scan_bike_targets task failed: {e}"))?
}

fn scan_bike_targets_blocking(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    Ok(library::scan_bike_targets(&cfg.mods_path, &cfg.profiles_dir()))
}

#[tauri::command]
async fn scan_model_swaps(app: tauri::AppHandle) -> Result<Vec<modelswap::BikeModels>, String> {
    tauri::async_runtime::spawn_blocking(move || scan_model_swaps_blocking(app))
        .await
        .map_err(|e| format!("scan_model_swaps task failed: {e}"))?
}

fn scan_model_swaps_blocking(app: tauri::AppHandle) -> Result<Vec<modelswap::BikeModels>, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    Ok(modelswap::scan_model_swaps(&cfg.mods_path))
}

/// Outcome of a Locker model/sound swap — mirrors `PresetApplyOutcome` so the UI can
/// report the same "refreshed live in-game" feedback the presets flow gives.
#[derive(serde::Serialize)]
struct SwapApplyOutcome {
    content_reload: ReloadOutcome,
    game_running: bool,
    live_refresh: gameproc::LiveRefresh,
    /// Model swaps only (`None` for sound). `live_refresh` re-runs the *customization*
    /// loader, which reloads paints/gear but never the mesh — the model needs FrostMod
    /// to re-apply the bike. See `frostmod::signal_refresh_model`.
    model_refresh: Option<frostmod::CommandOutcome>,
}

/// Outcome of switching ReShade preset.
///
/// Much thinner than [`SwapApplyOutcome`] because switching a preset touches nothing this app
/// owns: no content to reload, no loader to re-run. ReShade picks the file up itself, so the
/// only thing the UI can't work out on its own is whether a session is already running and
/// therefore whether the player sees it now or next launch.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReshadeApplyOutcome {
    game_running: bool,
}

/// Re-run the game's look loader live if instant refresh is enabled, else report it off.
fn live_refresh(enabled: bool) -> gameproc::LiveRefresh {
    if enabled {
        gameproc::refresh_look()
    } else {
        gameproc::LiveRefresh::Disabled
    }
}

/// Ask FrostMod to re-apply `bike` so a just-swapped model shows live. `None` when
/// instant refresh is off — the same switch that gates `live_refresh`, since both
/// reach into the running game.
///
/// The tag our installer recorded decides whether the command goes out at all. It used
/// to be sent unconditionally and only the *wording* adjusted afterwards, because the
/// worst an old FrostMod did was log an unknown verb and drop it. That is no longer the
/// worst: FrostMod v0.9.9 acts on the verb by replaying a bike-apply call it captured
/// earlier, which corrupts the game's bike state and crashes it to desktop at the next
/// bike the player picks by hand. So the check moved *before* the send — nothing is
/// written to the command file and no event is pulsed for a build we don't trust.
/// See `frostmod::MODEL_REFRESH_MIN_VERSION`.
fn model_refresh_cmd(
    app: &tauri::AppHandle,
    enabled: bool,
    bike: &str,
) -> Option<frostmod::CommandOutcome> {
    if !enabled {
        return None;
    }
    let tag = frostmod_manage::installed_version(app);
    if !frostmod::model_refresh_is_safe(tag.as_deref()) {
        return Some(frostmod::CommandOutcome::Withheld);
    }
    Some(frostmod::signal_refresh_model(bike))
}

#[tauri::command]
async fn apply_model_swap(
    app: tauri::AppHandle,
    bike: String,
    target: String,
) -> Result<SwapApplyOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || apply_model_swap_blocking(app, bike, target))
        .await
        .map_err(|e| format!("apply_model_swap task failed: {e}"))?
}

fn apply_model_swap_blocking(
    app: tauri::AppHandle,
    bike: String,
    target: String,
) -> Result<SwapApplyOutcome, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    let prev = modelswap::current_active(&cfg.mods_path, &bike);
    modelswap::apply_model_swap(&cfg.mods_path, &bike, &target).map_err(|e| format!("{e:#}"))?;
    // Make a bound sound travel with the model (case 2); independent sounds are left
    // untouched (case 1). Best-effort — the model swap itself already succeeded.
    if let Err(e) = soundmods::reconcile_after_model_swap(&cfg.mods_path, &bike, &prev, &target) {
        eprintln!("sound reconcile after model swap failed: {e:#}");
    }
    let content_reload = frostmod::signal_reload();
    // Ask FrostMod to re-apply the bike so the new model shows in the garage without a
    // class switch away-and-back. Only acts if `bike` is the selected one (decided
    // inside FrostMod, which is the only side that knows). Gated on the same
    // instant-refresh setting as the look refresh — both poke the live game.
    let model_refresh = model_refresh_cmd(&app, cfg.instant_refresh, &bike);
    Ok(SwapApplyOutcome {
        content_reload,
        game_running: gameproc::is_game_running(),
        live_refresh: live_refresh(cfg.instant_refresh),
        model_refresh,
    })
}

#[tauri::command]
async fn scan_sound_swaps(app: tauri::AppHandle) -> Result<Vec<soundmods::BikeSounds>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        Ok(soundmods::scan_sound_swaps(&cfg.mods_path))
    })
    .await
    .map_err(|e| format!("scan_sound_swaps task failed: {e}"))?
}

#[tauri::command]
async fn apply_sound_swap(
    app: tauri::AppHandle,
    bike: String,
    target: String,
) -> Result<SwapApplyOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        soundmods::apply_sound_swap(&cfg.mods_path, &bike, &target).map_err(|e| format!("{e:#}"))?;
        let content_reload = frostmod::signal_reload();
        Ok(SwapApplyOutcome {
            content_reload,
            game_running: gameproc::is_game_running(),
            live_refresh: live_refresh(cfg.instant_refresh),
            model_refresh: None, // a sound swap doesn't touch the model
        })
    })
    .await
    .map_err(|e| format!("apply_sound_swap task failed: {e}"))?
}

/// The ReShade card's whole state, read fresh from the folder ReShade lives in — see
/// [`reshade`].
#[tauri::command]
async fn reshade_status(app: tauri::AppHandle) -> Result<reshade::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        let mut status = reshade::status(&cfg.reshade_dir());
        // Only this side knows where that folder came from, and the card has to say so
        // before it offers to hand the folder back to the game's install dir.
        status.custom = !cfg.reshade_path.trim().is_empty();
        Ok(status)
    })
    .await
    .map_err(|e| format!("reshade_status task failed: {e}"))?
}

/// Point the ReShade card at a folder of the player's choosing. An empty string clears the
/// override, back to the game's install dir.
///
/// The pick is taken as given rather than validated: a folder with no ReShade in it is a
/// perfectly ordinary thing to land on mid-setup, and `reshade_status` says so plainly on
/// the very next read. Refusing it would leave the player with a dialog and no way to see
/// what the app thinks is wrong.
#[tauri::command]
fn set_reshade_path(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let mut cfg = config::load(&app).unwrap_or_default();
    cfg.reshade_path = path;
    config::save(&app, &cfg).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn apply_reshade_preset(
    app: tauri::AppHandle,
    name: String,
) -> Result<ReshadeApplyOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        reshade::apply(&cfg.reshade_dir(), &name).map_err(|e| format!("{e:#}"))?;
        // Unlike a content swap there is nothing to signal: ReShade owns its own config and
        // FrostMod has no part in it. All the UI needs is whether the player will see this
        // now or on the next launch.
        Ok(ReshadeApplyOutcome {
            game_running: gameproc::is_game_running(),
        })
    })
    .await
    .map_err(|e| format!("apply_reshade_preset task failed: {e}"))?
}

#[tauri::command]
async fn delete_reshade_preset(app: tauri::AppHandle, name: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        reshade::delete(&cfg.reshade_dir(), &name).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("delete_reshade_preset task failed: {e}"))?
}

#[tauri::command]
async fn bind_sound(app: tauri::AppHandle, bike: String, model: String, sound: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        soundmods::bind_sound(&cfg.mods_path, &bike, &model, &sound).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("bind_sound task failed: {e}"))?
}

#[tauri::command]
async fn unbind_sound(app: tauri::AppHandle, bike: String, model: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        soundmods::unbind_sound(&cfg.mods_path, &bike, &model).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("unbind_sound task failed: {e}"))?
}

#[tauri::command]
async fn detect_loose_swaps(app: tauri::AppHandle) -> Result<Vec<modelswap::LooseSwapBike>, String> {
    tauri::async_runtime::spawn_blocking(move || detect_loose_swaps_blocking(app))
        .await
        .map_err(|e| format!("detect_loose_swaps task failed: {e}"))?
}

fn detect_loose_swaps_blocking(app: tauri::AppHandle) -> Result<Vec<modelswap::LooseSwapBike>, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    Ok(modelswap::detect_loose_swaps(&cfg.mods_path))
}

#[tauri::command]
async fn register_loose_swaps(
    app: tauri::AppHandle,
    move_files: bool,
) -> Result<modelswap::RegisterReport, String> {
    tauri::async_runtime::spawn_blocking(move || register_loose_swaps_blocking(app, move_files))
        .await
        .map_err(|e| format!("register_loose_swaps task failed: {e}"))?
}

fn register_loose_swaps_blocking(
    app: tauri::AppHandle,
    move_files: bool,
) -> Result<modelswap::RegisterReport, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    modelswap::register_loose_swaps(&cfg.mods_path, move_files).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn detect_orphaned_setup(
    app: tauri::AppHandle,
) -> Result<Vec<modelswap::OrphanedSetup>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        Ok(modelswap::detect_orphaned_setup(&cfg.mods_path))
    })
    .await
    .map_err(|e| format!("detect_orphaned_setup task failed: {e}"))?
}

#[tauri::command]
async fn repair_orphaned_setup(app: tauri::AppHandle, bike: String) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        modelswap::repair_orphaned_setup(&cfg.mods_path, &bike).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("repair_orphaned_setup task failed: {e}"))?
}

#[tauri::command]
async fn get_pkz_meta(app: tauri::AppHandle, path: String) -> Result<pkz::PkzMeta, String> {
    tauri::async_runtime::spawn_blocking(move || get_pkz_meta_blocking(app, path))
        .await
        .map_err(|e| format!("get_pkz_meta task failed: {e}"))?
}

fn get_pkz_meta_blocking(app: tauri::AppHandle, path: String) -> Result<pkz::PkzMeta, String> {
    pkz::read_meta_cached(&app, &path).map_err(|e| format!("{e:#}"))
}

/// Metadata for many mods at once, but only for the ones already cached — `None` marks
/// an entry the caller still has to request individually.
///
/// The Library asks for this first so a known collection paints in a single round trip
/// instead of one request (and one archive read) per card.
#[tauri::command]
async fn get_pkz_meta_cached(
    app: tauri::AppHandle,
    paths: Vec<String>,
) -> Result<Vec<Option<pkz::PkzMeta>>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        paths
            .iter()
            .map(|p| pkz::read_meta_if_cached(&app, p))
            .collect()
    })
    .await
    .map_err(|e| format!("get_pkz_meta_cached task failed: {e}"))
}

#[tauri::command]
async fn get_pkz_preview(path: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || get_pkz_preview_blocking(path))
        .await
        .map_err(|e| format!("get_pkz_preview task failed: {e}"))?
}

fn get_pkz_preview_blocking(path: String) -> Result<Option<String>, String> {
    pkz::read_preview(std::path::Path::new(&path)).map_err(|e| format!("{e:#}"))
}

/// A track's metadata and contents. Cheap by construction — nothing is inflated — so the
/// track view can paint everything except the terrain immediately.
#[tauri::command]
async fn read_track_info(app: tauri::AppHandle, path: String) -> Result<track::TrackInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        track::read_info(&app, &path).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("read_track_info task failed: {e}"))?
}

/// A plain-text account of what a track's terrain looks like to the reader.
///
/// Shown in the viewer when a track's terrain won't load. The height format is undocumented,
/// so a track that fails is evidence we don't otherwise have — and the player holding it is
/// rarely the person who can rebuild the app to investigate.
#[tauri::command]
async fn diagnose_track(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || track::diagnose(std::path::Path::new(&path)))
        .await
        .map_err(|e| format!("diagnose_track task failed: {e}"))
}

/// A track's terrain grid, at no more than `max_dim` samples on its longest edge.
///
/// Returned as raw bytes rather than JSON: a grid is a few hundred thousand floats, and
/// serialising that as a JSON array costs more than reading it out of the archive did. The
/// app reads the header described in [`track::BLOB_HEADER`] and takes the rest in place.
#[tauri::command]
async fn load_track_terrain(
    app: tauri::AppHandle,
    path: String,
    max_dim: u32,
) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let master = track::load_master(&app, &path).map_err(|e| format!("{e:#}"))?;
        Ok(tauri::ipc::Response::new(track::terrain_blob(
            &master, max_dim,
        )))
    })
    .await
    .map_err(|e| format!("load_track_terrain task failed: {e}"))?
}

/// A picture of a track's surfaces, to lay over its terrain.
///
/// Empty — not an error — when the track's height file carries no coverage masks. That track
/// draws on its relief alone, which is what every track did before this existed, so there is
/// nothing here worth failing a view over.
#[tauri::command]
async fn load_track_overview(
    app: tauri::AppHandle,
    path: String,
    max_dim: u32,
) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let blob = track::overview_blob(&app, std::path::Path::new(&path), max_dim)
            .unwrap_or_default();
        tauri::ipc::Response::new(blob)
    })
    .await
    .map_err(|e| format!("load_track_overview task failed: {e}"))
}

#[tauri::command]
async fn unpack_paint(path: String) -> Result<Vec<paint::PaintTexture>, String> {
    tauri::async_runtime::spawn_blocking(move || unpack_paint_blocking(path))
        .await
        .map_err(|e| format!("unpack_paint task failed: {e}"))?
}

/// Paints decoded for the viewer, so re-opening one doesn't inflate it a second time.
///
/// The picker re-runs this on every selection change and on every re-open, and a gear paint is
/// tens of megabytes of DEFLATE — the pixels behind an entry, on the other hand, are small,
/// because each is downscaled to 1024² before it is stored.
const PAINT_CACHE_CAP: usize = 4;

fn paint_cache() -> &'static std::sync::Mutex<lru::Lru<Vec<paint::PaintTexture>>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<lru::Lru<Vec<paint::PaintTexture>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(lru::Lru::new(PAINT_CACHE_CAP)))
}

fn unpack_paint_blocking(path: String) -> Result<Vec<paint::PaintTexture>, String> {
    let t0 = std::time::Instant::now();
    // Path *and* mtime, as the bike cache does, so a paint re-saved under the same name misses.
    let key = bike_cache_key(&path);
    if let Some(t) = paint_cache().lock().ok().and_then(|mut c| c.get(&key).cloned()) {
        log::info!("unpack_paint {path}: cache hit ({:?})", t0.elapsed());
        return Ok(t);
    }

    let textures = paint::unpack_file(std::path::Path::new(&path)).map_err(|e| format!("{e:#}"))?;
    log::info!(
        "unpack_paint {path}: {} texture(s) in {:?} | {:.1} MB resident in the texture store",
        textures.len(),
        t0.elapsed(),
        texstore::resident_bytes() as f64 / (1024.0 * 1024.0),
    );
    if let Ok(mut c) = paint_cache().lock() {
        // Cloning an entry copies names, sizes and tokens — never pixels, which stay in the
        // texture store. The evicted paint's go with it; nothing else holds those tokens.
        if let Some(dropped) = c.insert(key, textures.clone()) {
            let tokens: Vec<String> = dropped.iter().map(|t| t.token.clone()).collect();
            texstore::release(&tokens);
        }
    }
    Ok(textures)
}

// ── Paint studio ────────────────────────────────────────────────────────────────────
//
// A `.pnt` is a packed container no image editor can write, so a livery drawn in GIMP has
// always needed somebody else's converter before the game would load it. These commands are
// both halves of that: images in (`paint_studio_save`), sheets out as editable TGA
// templates (`paint_studio_extract`), and the texture names a destination expects
// (`paint_studio_hints`) so a new paint binds to the same parts as the ones already there.

/// Where a built paint is written.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum PaintDest {
    /// Under the game's `mods` folder — `bikes/<Bike>/paints`,
    /// `rider/helmets/<Helmet>/paints`, `rider/riders/<Profile>/gloves`…
    Mods { rel: String },
    /// A folder the player picked themselves, for a paint they mean to share rather than
    /// install.
    Folder { path: String },
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedPaint {
    path: String,
    textures: Vec<String>,
    bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PaintTarget {
    path: String,
    exists: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PaintTemplate {
    dir: String,
    files: Vec<String>,
    textures: Vec<String>,
}

/// Read source images for the studio — the pixels land in the texture store, so the UI
/// previews them through exactly the same path as a decoded paint's.
#[tauri::command]
async fn paint_studio_load(paths: Vec<String>) -> Result<Vec<paintstudio::StudioImage>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        paths
            .iter()
            .map(|p| paintstudio::inspect(std::path::Path::new(p)).map_err(|e| format!("{e:#}")))
            .collect()
    })
    .await
    .map_err(|e| format!("paint_studio_load task failed: {e}"))?
}

/// Read one image at its full size, for the Designer to composite with.
///
/// Separate from `paint_studio_load` because that one answers "describe this file" with a
/// thumbnail, and the editor needs the pixels themselves — see `paintstudio::pixels`.
#[tauri::command]
async fn paint_studio_pixels(path: String) -> Result<paint::PaintTexture, String> {
    tauri::async_runtime::spawn_blocking(move || {
        paintstudio::pixels(std::path::Path::new(&path)).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("paint_studio_pixels task failed: {e}"))?
}

/// Stage a composited sheet, returning the file `paint_studio_save` should pack.
///
/// Takes the PNG as a raw request body rather than an argument: a 4096² sheet is megabytes,
/// and JSON would send it as a list of numbers. The sheet's texture name rides in a header
/// for the same reason — the body has to be the bytes and nothing else.
///
/// One staging directory per call. The caller saves immediately after staging every sheet, so
/// these are short-lived; they sit in the OS temp dir either way, which is where an editor
/// that's closed mid-flight should leave its scratch files.
#[tauri::command]
async fn paint_studio_stage(request: tauri::ipc::Request<'_>) -> Result<String, String> {
    let tauri::ipc::InvokeBody::Raw(png) = request.body() else {
        return Err("paint_studio_stage expects the sheet's PNG bytes as the request body".into());
    };
    let name = request
        .headers()
        .get("x-sheet-name")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let png = png.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let dir = install::staging_dir("paint");
        paintstudio::stage_sheet(&dir, &name, &png)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("paint_studio_stage task failed: {e}"))?
}

/// The file a save would write, resolved but not written — so the UI can ask before
/// replacing a paint that's already there.
#[tauri::command]
fn paint_studio_target(
    app: tauri::AppHandle,
    file_name: String,
    dest: PaintDest,
) -> Result<PaintTarget, String> {
    let path = resolve_paint_dest(&app, &file_name, &dest)?;
    Ok(PaintTarget { exists: path.is_file(), path: path.to_string_lossy().into_owned() })
}

/// `<dir>/<name>.pnt` for a destination, refusing anything that isn't a paint sitting where
/// a paint belongs.
///
/// The `Mods` arm goes through [`paintsync::safe_dest`] — the same check that vets a
/// destination sent by another player over paint sync. Nothing here is remote, but the rule
/// it enforces (a relative path, no traversal, at least two segments deep, ending in
/// `.pnt`) is exactly the rule a paint destination has to satisfy, and one boundary with
/// tests beats a second one written from memory.
fn resolve_paint_dest(
    app: &tauri::AppHandle,
    file_name: &str,
    dest: &PaintDest,
) -> Result<std::path::PathBuf, String> {
    let stem = install::sanitize(file_name.trim())
        .trim()
        .trim_end_matches('.')
        .trim_end_matches(".pnt")
        .trim()
        .to_string();
    if stem.is_empty() {
        return Err("Name this paint before saving it.".into());
    }
    let file = format!("{stem}.pnt");
    match dest {
        PaintDest::Mods { rel } => {
            let cfg = config::load(app).map_err(|e| format!("{e:#}"))?;
            let mods_dir = library::mods_subdir(&cfg.mods_path, "mods");
            let rel = rel.replace('\\', "/");
            let rel = rel.trim_matches('/');
            paintsync::safe_dest(&mods_dir, &format!("{rel}/{file}"))
                .ok_or_else(|| format!("'{rel}' isn't a folder a paint can be installed into"))
        }
        PaintDest::Folder { path } => {
            let dir = std::path::PathBuf::from(path);
            if !dir.is_dir() {
                return Err(format!("{} isn't a folder", dir.display()));
            }
            Ok(dir.join(file))
        }
    }
}

/// Build a `.pnt` from the chosen images and write it.
#[tauri::command]
async fn paint_studio_save(
    app: tauri::AppHandle,
    name: String,
    file_name: String,
    textures: Vec<paintstudio::BuildTexture>,
    dest: PaintDest,
    overwrite: bool,
) -> Result<SavedPaint, String> {
    let target = resolve_paint_dest(&app, &file_name, &dest)?;
    tauri::async_runtime::spawn_blocking(move || {
        if !overwrite && target.exists() {
            return Err(format!("{} is already there.", target.display()));
        }
        // The paint's own name is the one the game shows; an empty one falls back to the
        // file name, which is what every paint on disk is picked by anyway.
        let title = if name.trim().is_empty() {
            target.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            name.trim().to_string()
        };
        let bytes = paintstudio::build(&title, &textures).map_err(|e| format!("{e:#}"))?;
        let names = paint::texture_names(&bytes).map_err(|e| format!("{e:#}"))?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, &bytes)
            .map_err(|e| format!("write {}: {e}", target.display()))?;
        log::info!(
            "[paint studio] wrote {} ({} bytes, textures: {})",
            target.display(),
            bytes.len(),
            names.join(", ")
        );
        Ok(SavedPaint {
            path: target.to_string_lossy().into_owned(),
            textures: names,
            bytes: bytes.len() as u64,
        })
    })
    .await
    .map_err(|e| format!("paint_studio_save task failed: {e}"))?
}

/// Write a paint's sheets out as `.tga` files to edit — the way to start from a livery
/// that already fits the model instead of from a blank sheet.
#[tauri::command]
async fn paint_studio_extract(
    app: tauri::AppHandle,
    path: String,
    dest: Option<String>,
) -> Result<PaintTemplate, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let src = std::path::PathBuf::from(&path);
        let stem = src
            .file_stem()
            .map(|s| install::sanitize(&s.to_string_lossy()))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "paint".to_string());
        let dir = match dest {
            Some(d) => std::path::PathBuf::from(d).join(&stem),
            None => templates_root(&app).join(&stem),
        };
        let bytes = std::fs::read(&src).map_err(|e| format!("read {}: {e}", src.display()))?;
        let files = paintstudio::extract(&bytes, &dir).map_err(|e| format!("{e:#}"))?;
        let textures = files
            .iter()
            .filter_map(|f| f.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        Ok(PaintTemplate {
            dir: dir.to_string_lossy().into_owned(),
            files: files.iter().map(|f| f.to_string_lossy().into_owned()).collect(),
            textures,
        })
    })
    .await
    .map_err(|e| format!("paint_studio_extract task failed: {e}"))?
}

/// Where templates go when the player doesn't pick somewhere: their Documents folder, not
/// the mods folder — the game scans that, and a folder of loose sheets isn't a mod.
fn templates_root(app: &tauri::AppHandle) -> std::path::PathBuf {
    dirs_next::document_dir()
        .or_else(|| app.path().app_data_dir().ok())
        .unwrap_or_else(std::env::temp_dir)
        .join("MXB App")
        .join("Paint Templates")
}

/// The texture names a destination can paint.
///
/// A paint binds by name: call a sheet `livery` and it lands on the bodywork that asked for
/// `livery`, call it `my_livery` and it lands nowhere. Two sources answer it, and both are
/// needed. The paints already installed name what *they* replace — read from their headers,
/// so that half costs no pixels. The model's own mesh names everything it draws, which is
/// the half a paint can't supply: the OEM bikes ship a stock paint that replaces the wheels
/// and the chain and nothing else, so on a stock Husqvarna the sheets on offer were `chain`,
/// `wheel`, `wheels` — and `plastics`, the one anybody opens the Designer for, was missing.
#[tauri::command]
async fn paint_studio_hints(app: tauri::AppHandle, rel: String) -> Result<Vec<String>, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        let rel = rel.replace('\\', "/").trim_matches('/').to_string();
        Ok(paint_hints(&library::mods_subdir(&cfg.mods_path, &format!("mods/{rel}"))))
    })
    .await
    .map_err(|e| format!("paint_studio_hints task failed: {e}"))?
}

/// [`paint_studio_hints`] for a destination folder that's already been resolved.
fn paint_hints(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    fn add(names: &mut Vec<String>, found: Vec<String>) {
        for n in found {
            if !n.is_empty() && !names.iter().any(|s| s.eq_ignore_ascii_case(&n)) {
                names.push(n);
            }
        }
    }

    // A handful is plenty: paints for one model overwhelmingly supply the same names,
    // and this runs every time the destination changes.
    const SAMPLE: usize = 8;
    let mut seen = 0usize;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if seen >= SAMPLE || !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pnt")) {
                continue;
            }
            // Seeked, not read: a bike's paints are tens of megabytes each and the names are
            // in their headers. Reading eight of them whole put nineteen seconds between
            // picking a model and being told what it wants.
            if let Ok(found) = paint::texture_names_at(&p) {
                add(&mut names, found);
                seen += 1;
            }
        }
    }
    // Nothing installed loose: the model may be packed, and its own paints are the
    // same evidence. `<Model>.pkz` sits beside the `<Model>` folder this destination
    // lives in — for a bike as much as for a helmet.
    if names.is_empty() {
        if let (Some(sub), Some(model_dir)) = (dir.file_name(), dir.parent()) {
            let pkz = model_dir.with_extension("pkz");
            let tail = format!("/{}/", sub.to_string_lossy().to_ascii_lowercase());
            if pkz.is_file() {
                let want = |n: &str| {
                    let n = n.replace('\\', "/").to_ascii_lowercase();
                    n.contains(&tail) && n.ends_with(".pnt")
                };
                let packed = pkz::read_selected(&pkz, want).unwrap_or_default();
                for (_, bytes) in packed.iter().take(SAMPLE) {
                    add(&mut names, paint::texture_names_any(bytes).unwrap_or_default());
                }
            }
        }
    }
    // What the model itself draws, whether or not a paint has ever replaced it.
    //
    // Only for the model's own `paints` folder. A mesh names every texture on the item
    // without saying which of them belong to the goggles hanging off it — the paints are
    // the only thing that says that (see `on_goggle_side`) — and offering a helmet's shell
    // sheet to somebody painting its goggles would put the shell in the wrong file.
    let main_paints = dir.file_name().is_some_and(|s| s.eq_ignore_ascii_case("paints"));
    if let (true, Some(model_dir)) = (main_paints, dir.parent()) {
        add(&mut names, mesh_texture_names(model_dir));
    }
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Every texture a model's own mesh carries, by name.
///
/// Read from the mesh's texture records — names and dimensions, never the pixels beside
/// them — so a 54 MB bike is answered by a walk over its bytes rather than by inflating it.
/// A model that ships as a folder is read from there, and one that ships packed from the
/// `<Model>.pkz` beside it; a sealed file is unwrapped the way the viewer unwraps it.
fn mesh_texture_names(model_dir: &std::path::Path) -> Vec<String> {
    let mut meshes: Vec<Vec<u8>> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(model_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() && p.file_name().and_then(|n| n.to_str()).is_some_and(bikefiles::is_mesh)
            {
                meshes.extend(read_gear_file(&p));
            }
        }
    }
    if meshes.is_empty() {
        let pkz = model_dir.with_extension("pkz");
        if pkz.is_file() {
            for (_, d) in pkz::read_selected(&pkz, bikefiles::is_mesh).unwrap_or_default() {
                meshes.push(pkz::read_sidecar_blob(&d).unwrap_or(d));
            }
        }
    }
    let mut names: Vec<String> = Vec::new();
    for mesh in &meshes {
        for t in edf::embedded_textures(mesh) {
            if !names.iter().any(|s| s.eq_ignore_ascii_case(&t.name)) {
                names.push(t.name);
            }
        }
    }
    names
}

/// Raw RGBA for a texture the viewer was handed a token for.
///
/// Returns an `ipc::Response`, which travels as `application/octet-stream` and lands in the
/// webview as an `ArrayBuffer` — the pixels are never encoded, base64'd, or parsed as JSON
/// on the way. The frontend feeds the buffer straight to a `THREE.DataTexture`. `async`
/// keeps the copy off the main thread, as with every other command here.
#[tauri::command]
async fn texture_bytes(token: String) -> tauri::ipc::Response {
    tauri::ipc::Response::new(texstore::bytes_or_missing(&token))
}

/// Watch the paint files the 3D viewer is currently showing, replacing whatever it was
/// watching before. An empty list stops.
///
/// There is one viewer showing one paint, so this is a set-the-whole-thing call rather than
/// an add/remove pair: nothing can then leak a watch by forgetting to take one back.
#[tauri::command]
fn watch_paint_files(app: tauri::AppHandle, watcher: State<PaintWatcher>, paths: Vec<String>) {
    paintwatch::start(&app, &watcher, &paths);
}

#[tauri::command]
async fn unpack_pkz(path: String, out_dir: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || unpack_pkz_blocking(path, out_dir))
        .await
        .map_err(|e| format!("unpack_pkz task failed: {e}"))?
}

fn unpack_pkz_blocking(path: String, out_dir: String) -> Result<Vec<String>, String> {
    pkz::extract(std::path::Path::new(&path), std::path::Path::new(&out_dir))
        .map_err(|e| format!("{e:#}"))
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BikePaint {
    name: String,
    /// Where the `.pnt` sits on disk, for a paint installed loose in the bike's `paints`
    /// folder — the file the viewer watches so an edit re-dresses the model. `None` for a
    /// paint packed inside the archive: nothing rewrites one of those in place.
    path: Option<String>,
    textures: Vec<paint::PaintTexture>,
    changes_preview: bool,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BikeModel {
    nodes: Vec<edf::EdfNode>,
    paints: Vec<BikePaint>,
    /// The model's own textures — the look it ships with, before any paint replaces one.
    ///
    /// The same pixels the paints below already carry as fillers, said once on their own.
    /// Folded into a paint they're indistinguishable from that paint's own sheets, and the
    /// Designer's reference underlay needs the distinction: an OEM bike's stock `.pnt`
    /// replaces the wheels and the chain, so `plastics` is only ever in here.
    base: Vec<paint::PaintTexture>,
    /// Whether the parts were placed into one frame by the bike's `.geom`.
    ///
    /// False means every node still sits in its own local frame, so a vertex's position says
    /// nothing about where it is on the bike. The Designer names the flank a sheet region
    /// paints from the sign of x, and that answer is only worth giving once this is true.
    assembled: bool,
}

impl BikeModel {
    /// Every texture token the model holds, so evicting it can free the pixels too.
    ///
    /// `base` as well as the paints, and not only because it is a field now: a base texture
    /// that *every* paint overrides is folded into none of them, and before this was dropped
    /// without ever being released. Duplicates are free — `texstore::release` removes by key.
    fn tokens(&self) -> Vec<String> {
        self.paints
            .iter()
            .flat_map(|p| p.textures.iter())
            .chain(self.base.iter())
            .map(|t| t.token.clone())
            .collect()
    }
}

/// Bikes are big (geometry plus every paint's pixels), so hold only the few most recent.
const BIKE_CACHE_CAP: usize = 3;

fn bike_cache() -> &'static std::sync::Mutex<lru::Lru<BikeModel>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<lru::Lru<BikeModel>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(lru::Lru::new(BIKE_CACHE_CAP)))
}

fn mtime_nanos(path: &std::path::Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Cache key for whatever lives at `source`: its path, when it was last written, and how
/// big it is.
///
/// Size is in there for the viewer's live reload. A paint being re-saved every few seconds
/// is the one caller that rewrites a file under the cache, and mtime alone would serve it
/// stale pixels on any filesystem whose timestamps are coarser than the gap between two
/// saves — FAT32 rounds to two seconds. A recompressed `.pnt` almost never comes back the
/// same length.
fn bike_cache_key(source: &str) -> String {
    let path = std::path::Path::new(source);
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    format!("{source}:{}:{size}", mtime_nanos(path))
}

/// A swap preview is keyed by both folders it's built from — the same bike renders
/// differently per variant, and either side can change under us.
fn swap_cache_key(set: &modelswap::PreviewSet) -> String {
    format!(
        "{}#{}:{}:{}",
        set.bike_dir.display(),
        set.variant_dir.display(),
        mtime_nanos(&set.bike_dir),
        mtime_nanos(&set.variant_dir),
    )
}

#[tauri::command]
async fn load_bike_model(source: String) -> Result<BikeModel, String> {
    tauri::async_runtime::spawn_blocking(move || load_bike_model_blocking(source))
        .await
        .map_err(|e| format!("load_bike_model task failed: {e}"))?
}

/// Draw `bike` as the model-swap variant `variant` would leave it, without applying the
/// swap. The file set is assembled in memory (see `gather_preview_files`) — nothing on
/// disk moves, so this is safe to run with the game open.
#[tauri::command]
async fn preview_model_swap(
    app: tauri::AppHandle,
    bike: String,
    variant: String,
) -> Result<BikeModel, String> {
    tauri::async_runtime::spawn_blocking(move || preview_model_swap_blocking(app, bike, variant))
        .await
        .map_err(|e| format!("preview_model_swap task failed: {e}"))?
}

fn preview_model_swap_blocking(
    app: tauri::AppHandle,
    bike: String,
    variant: String,
) -> Result<BikeModel, String> {
    let t0 = std::time::Instant::now();
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    let set =
        modelswap::preview_set(&cfg.mods_path, &bike, &variant).map_err(|e| format!("{e:#}"))?;
    let label = format!("{bike} · {variant}");
    let key = format!("{}#p{:x}", swap_cache_key(&set), paints_stamp(&set.bike_dir));
    if let Some(m) = bike_cache().lock().ok().and_then(|mut c| c.get(&key).cloned()) {
        log::info!("preview_model_swap {label}: cache hit ({:?})", t0.elapsed());
        return Ok(m);
    }

    let files = gather_preview_files(&set).map_err(|e| format!("{e:#}"))?;
    let installed = installed_paints(&set.bike_dir);
    build_bike_model(&label, key, files, installed, t0)
}

/// A stamp over the loose paints beside a bike, for the cache key to carry.
///
/// The bike's own file can't see them. A `.pnt` is written into `<bike>/paints/`, which
/// leaves the `.pkz`'s mtime and size exactly as they were — and on a bike loaded from its
/// folder, writing a file inside `paints/` doesn't touch the folder above it either. So the
/// key matched, the cache answered, and the model handed back was the one read before the
/// paint existed: you saved, the bike didn't change, and nothing in the log looked wrong.
///
/// Name, length and mtime per `.pnt`, sorted so `read_dir` order can't shuffle the answer.
/// Covers all three ways the set can move — a paint added, removed, or re-saved in place.
fn paints_stamp(source: &std::path::Path) -> u64 {
    let folder = if source.is_dir() {
        source.to_path_buf()
    } else {
        source.with_extension("")
    };
    let mut rows: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(folder.join("paints")) {
        for e in entries.flatten() {
            let path = e.path();
            if !path.extension().is_some_and(|x| x.eq_ignore_ascii_case("pnt")) {
                continue;
            }
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            rows.push(format!(
                "{}:{len}:{}",
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                mtime_nanos(&path),
            ));
        }
    }
    rows.sort_unstable();
    // FNV-1a. Not a security question — this only has to change when the folder does.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for row in &rows {
        for b in row.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    }
    h
}

fn load_bike_model_blocking(source: String) -> Result<BikeModel, String> {
    let t0 = std::time::Instant::now();
    let key = format!(
        "{}#p{:x}",
        bike_cache_key(&source),
        paints_stamp(std::path::Path::new(&source)),
    );
    if let Some(m) = bike_cache().lock().ok().and_then(|mut c| c.get(&key).cloned()) {
        log::info!("load_bike_model {source}: cache hit ({:?})", t0.elapsed());
        return Ok(m);
    }

    let files = gather_bike_files(std::path::Path::new(&source)).map_err(|e| format!("{e:#}"))?;
    let installed = installed_paints(std::path::Path::new(&source));
    build_bike_model(&source, key, files, installed, t0)
}

/// Turn a bike's files into the viewer's model: resolve each part's mesh through the
/// `.hrc`s, bind its textures, decode the paints. Shared by a bike loaded from disk and a
/// model-swap preview assembled in memory — `label` only names it in the log.
fn build_bike_model(
    label: &str,
    key: String,
    files: Vec<(String, Vec<u8>)>,
    // The loose paints beside the bike, as `installed_paints` answers them.
    installed: Vec<(String, String, Vec<u8>)>,
    t0: std::time::Instant,
) -> Result<BikeModel, String> {
    let t_read = t0.elapsed();

    let mut nodes = Vec::new();
    // Every mesh the bike ships, by file name — usually just `model.edf`, but a bike can
    // carry one per part. Which are actually used is decided by the `.hrc`s below.
    let mut edfs: std::collections::HashMap<String, &Vec<u8>> = std::collections::HashMap::new();
    let mut geom: Option<&Vec<u8>> = None;
    let mut gfx_bytes: Option<&Vec<u8>> = None;
    let mut hrcs: std::collections::HashMap<String, &Vec<u8>> = std::collections::HashMap::new();
    let mut tga_jobs: Vec<(String, &[u8])> = Vec::new();
    // (display name, bytes, shipped-in-the-archive, path on disk if it has one)
    let mut pnt_jobs: Vec<(String, &[u8], bool, Option<&str>)> = Vec::new();
    for (name, data) in &files {
        let bn = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
        if bn.ends_with(".edf") {
            edfs.insert(bn.clone(), data);
        } else if bn.ends_with(".geom") {
            geom = Some(data);
        } else if bn.ends_with("gfx.cfg") {
            gfx_bytes = Some(data);
        } else if let Some(stem) = bn.strip_suffix(".hrc") {
            let stem = stem.rsplit("__").next().unwrap_or(stem);
            hrcs.insert(stem.to_string(), data);
        } else if let Some(stem) = bn.strip_suffix(".tga") {
            // Lowercased stem — the frontend matches textures case-insensitively.
            tga_jobs.push((stem.to_string(), data.as_slice()));
        } else if bn.ends_with(".pnt") {
            pnt_jobs.push((paint_display_name(&bn), data.as_slice(), true, None));
        }
    }

    let gfx = gfx_bytes.map(|b| cfg::parse_gfx(b)).unwrap_or_default();
    // Group each part's level0 node under the mesh its `.hrc` names. Bikes that point
    // every part at one `model.edf` collapse to a single group — the original path.
    let mut scenes: Vec<(String, Vec<String>)> = Vec::new();
    let mut node_part: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // Fixed part order — `gfx` is a map, and node order must not shuffle between runs.
    for part in cfg::GFX_PARTS {
        let Some(gp) = gfx.get(part) else { continue };
        let Some(hrc_file) = gp.hrc.as_deref() else { continue };
        let stem = hrc_file.trim_end_matches(".hrc").trim_end_matches(".HRC");
        let Some(bytes) = hrcs.get(&stem.to_ascii_lowercase()) else {
            log::warn!("[viewer] gfx.cfg part '{part}' wants {hrc_file}, which the bike doesn't ship");
            continue;
        };
        let hrc = cfg::parse(bytes);
        let Some(node) = cfg::hrc_level0(&hrc, stem) else { continue };
        let scene = cfg::hrc_level0_scene(&hrc)
            .map(|s| s.replace('\\', "/"))
            .and_then(|s| s.rsplit('/').next().map(str::to_ascii_lowercase))
            .unwrap_or_else(|| "model.edf".to_string());
        node_part.insert(node.to_ascii_lowercase(), part.to_string());
        match scenes.iter_mut().find(|(f, _)| *f == scene) {
            Some((_, level0)) => level0.push(node),
            None => scenes.push((scene, vec![node])),
        }
    }

    // Parse each referenced mesh and bind its textures against *its own* bytes: a
    // submesh's material index selects from that file's texture pool, so a part must
    // never be bound through another file's pool.
    let mut used: Vec<&Vec<u8>> = Vec::new();
    for (file, level0) in &scenes {
        let Some(data) = edfs.get(file) else {
            log::warn!("[viewer] an .hrc wants {file}, which the bike doesn't ship");
            continue;
        };
        let mut part_nodes = edf::parse_with_levels(data, level0);
        bind_textures(&mut part_nodes, data, &gfx, &node_part);
        nodes.append(&mut part_nodes);
        used.push(data);
    }
    // No gfx.cfg/.hrc to go on (or none of it resolved) — fall back to the bike's base
    // mesh and let the parser's own level0 heuristic pick the parts.
    if nodes.is_empty() {
        if let Some(data) = base_edf(&edfs) {
            nodes = edf::parse_with_levels(data, &[]);
            bind_textures(&mut nodes, data, &gfx, &node_part);
            used.push(data);
        }
    }
    for (fname, path, data) in &installed {
        pnt_jobs.push((paint_display_name(fname), data.as_slice(), false, Some(path)));
    }
    // Whether the parts ended up in one frame. Logged rather than printed: it decides what the
    // Designer may say about a sheet's flanks, so "was this bike assembled?" has to be
    // answerable from the log file after the fact, not only from a terminal nobody kept.
    let assembled = match geom {
        Some(g) => {
            let ok = edf::assemble_bike(&mut nodes, g);
            if !ok {
                log::warn!("[viewer] {label}: .geom present but missing mount points — parts unassembled");
            }
            ok
        }
        None => {
            if !nodes.is_empty() {
                log::warn!("[viewer] {label}: no .geom alongside the mesh — parts unassembled");
            }
            false
        }
    };
    edf::to_right_handed(&mut nodes);
    // Nothing to draw. Returning a model with no nodes is worse than failing: the viewer reads
    // it as a successful load and puts its stand-in bike on screen, which reads as "this is your
    // bike" rather than "none of this bike arrived". A cloud-synced archive that hasn't been
    // downloaded lands here — every entry reads short, so the `.edf` never appears.
    if nodes.is_empty() {
        return Err(format!(
            "{label} holds no readable mesh — if the file is cloud-synced, it may not be fully downloaded yet"
        ));
    }
    let t_parse = t0.elapsed();

    let mut base: Vec<paint::PaintTexture> = tga_jobs
        .par_iter()
        .filter_map(|(stem, data)| paint::decode_image(stem, data))
        .collect();
    // Textures embedded in the meshes actually shown. Parts often share a name (each
    // file embeds the plastics it needs), so keep the first of each.
    let mut seen: std::collections::HashSet<String> =
        base.iter().map(|t| t.name.to_ascii_lowercase()).collect();
    for data in &used {
        for tex in paint::extract_edf_textures(data) {
            if seen.insert(tex.name.to_ascii_lowercase()) {
                base.push(tex);
            }
        }
    }
    let mut paints: Vec<(BikePaint, bool)> = pnt_jobs
        .par_iter()
        .filter_map(|(name, data, shipped, path)| {
            paint::decode_any(data).ok().map(|pnt| {
                (
                    BikePaint {
                        name: name.clone(),
                        path: path.map(str::to_string),
                        textures: pnt.into_par_iter().map(paint::into_texture).collect(),
                        changes_preview: false, // resolved below, once bindings are known
                    },
                    *shipped,
                )
            })
        })
        .collect();
    let base_count = base.len();
    let t_textures = t0.elapsed();

    let bound: std::collections::HashSet<String> = nodes
        .iter()
        .flat_map(|n| {
            n.texture
                .iter()
                .chain(n.submeshes.iter().filter_map(|s| s.texture.as_ref()))
        })
        .map(|t| t.to_ascii_lowercase())
        .collect();
    for (p, shipped) in &mut paints {
        p.changes_preview = *shipped
            || (!bound.is_empty()
                && p.textures
                    .iter()
                    .any(|t| bound.contains(&t.name.to_ascii_lowercase())));
        if !p.changes_preview {
            log::info!(
                "[viewer] paint '{}' won't move the preview: it ships {:?}, and the parts shown bind {:?}",
                p.name,
                p.textures.iter().map(|t| &t.name).collect::<Vec<_>>(),
                bound,
            );
        }
    }
    let mut paints: Vec<BikePaint> = paints.into_iter().map(|(p, _)| p).collect();

    // Kept before the folding below, which is where the model's own look stops being
    // telling apart from a paint's. Names and tokens only — no pixels are copied.
    let model_base = base.clone();

    for p in &mut paints {
        let own: std::collections::HashSet<String> =
            p.textures.iter().map(|t| t.name.to_ascii_lowercase()).collect();
        p.textures.extend(
            base.iter()
                .filter(|t| !own.contains(&t.name.to_ascii_lowercase()))
                .cloned(),
        );
    }
    if paints.is_empty() {
        paints.push(BikePaint {
            name: "Stock".into(),
            // The mesh's own textures, which live inside it rather than in a `.pnt`.
            path: None,
            textures: base,
            changes_preview: true, // the model's own textures, by definition
        });
    }

    let distinct_tex: std::collections::HashSet<&str> = paints
        .iter()
        .flat_map(|p| p.textures.iter().map(|t| t.token.as_str()))
        .collect();
    log::info!(
        "load_bike_model {label}: {} paint(s) + {base_count} base tex | read {t_read:?}, parse {:?}, decode {:?}, total {:?} | {} distinct texture(s), {:.1} MB resident in the texture store",
        paints.len(),
        t_parse - t_read,
        t_textures - t_parse,
        t0.elapsed(),
        distinct_tex.len(),
        texstore::resident_bytes() as f64 / (1024.0 * 1024.0),
    );
    for p in &paints {
        let mut names: Vec<&str> = p.textures.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        log::info!("  paint '{}' textures: {}", p.name, names.join(", "));
    }
    for n in &nodes {
        let subs: Vec<String> = n
            .submeshes
            .iter()
            .map(|s| {
                format!(
                    "{}->{}{}",
                    s.name,
                    s.texture.as_deref().unwrap_or("(none)"),
                    match s.uv_tile {
                        Some(0) | None => String::new(),
                        Some(t) => format!("@tile{t}"),
                    }
                )
            })
            .collect();
        log::info!("  node '{}' placed={} {}", n.name, n.placed, subs.join(", "));
    }

    let model = BikeModel { nodes, paints, base: model_base, assembled };
    if let Ok(mut c) = bike_cache().lock() {
        // The evicted bike's pixels go with it — nothing else references them.
        if let Some(dropped) = c.insert(key, model.clone()) {
            texstore::release(&dropped.tokens());
        }
    }
    Ok(model)
}

fn bind_textures(
    nodes: &mut [edf::EdfNode],
    edf_bytes: &[u8],
    gfx: &std::collections::HashMap<String, cfg::GfxPart>,
    node_part: &std::collections::HashMap<String, String>,
) {
    // A bike embeds every texture it draws, its stock livery included, so it declares
    // nothing beyond them.
    let colors = edf::declared_colors(edf_bytes, &[]);

    for n in nodes.iter_mut() {
        let part = node_part.get(&n.name.to_ascii_lowercase());
        let overrides = part.and_then(|p| gfx.get(p)).map(|p| &p.textures);
        if n.materials.is_empty() {
            log::warn!("[viewer] node '{}' has no material table — falling back", n.name);
        }
        // A material id is local to its node, so ask the node's own table.
        let material_texture = |mat: Option<u32>| -> Option<&String> {
            let slot = n.materials.get(mat? as usize).copied().flatten()?;
            colors.get(slot)
        };
        // A node with no submesh table draws on its first material.
        n.texture = material_texture(Some(0)).or_else(|| colors.first()).cloned();
        for sm in n.submeshes.iter_mut() {
            let group = sm.name.to_ascii_lowercase();
            // 1. An explicit gfx texture (animated chain, number plate) is authoritative.
            if let Some(tex) = overrides.and_then(|o| {
                o.get(&group)
                    .or_else(|| o.iter().find(|(g, _)| group.ends_with(&format!("_{g}"))).map(|(_, t)| t))
            }) {
                sm.texture = Some(tex.clone());
                continue;
            }
            // 2. The node's material table picks the colour texture this range was drawn on.
            if let Some(t) = material_texture(sm.mat) {
                sm.texture = Some(t.clone());
                continue;
            }
            // 3. No material recorded → leave unbound so it renders neutral grey, never smeared.
            sm.texture = None;
        }
    }
}

fn paint_display_name(file_name: &str) -> String {
    let stem = file_name
        .rsplit('/')
        .next()
        .unwrap_or(file_name)
        .trim_end_matches(".pnt")
        .trim_end_matches(".PNT");
    let mut chars = stem.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Stock".into(),
    }
}

/// The loose `.pnt`s installed beside a bike, as (file name, full path, bytes).
///
/// The path rides along because these are the only paints that can change under the viewer:
/// they are ordinary files a painter re-saves, where the ones inside the archive are not.
fn installed_paints(source: &std::path::Path) -> Vec<(String, String, Vec<u8>)> {
    let folder = if source.is_dir() {
        source.to_path_buf()
    } else {
        source.with_extension("")
    };
    let paints_dir = folder.join("paints");
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&paints_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pnt")) {
                if let (Some(name), Some(full), Ok(bytes)) = (
                    p.file_name().and_then(|n| n.to_str()),
                    p.to_str(),
                    std::fs::read(&p),
                ) {
                    out.push((name.to_string(), full.to_string(), bytes));
                }
            }
        }
    }
    out
}

// Any `.edf`, not just `model.edf`: a bike may ship one mesh per part, named by its
// `.hrc` (see `scene_files_for_parts`). Shadow meshes ride along unused. Shared with
// `modelswap` so the swapper and the viewer classify the same files the same way.
use bikefiles::is_viewer_file as wanted_bike_file;

/// The bike's main mesh when the `.hrc`s can't say which it is: `model.edf` by
/// convention, else the shortest non-shadow name — a per-part set like `96cr250.edf` /
/// `96cr250_fs.edf` / `96cr250_s.edf` (shadow) reduces to the chassis.
fn base_edf<'a>(
    edfs: &std::collections::HashMap<String, &'a Vec<u8>>,
) -> Option<&'a Vec<u8>> {
    if let Some(data) = edfs.get("model.edf") {
        return Some(data);
    }
    edfs.iter()
        .filter(|(name, _)| !name.ends_with("_s.edf"))
        .min_by_key(|(name, _)| (name.len(), name.to_string()))
        .or_else(|| edfs.iter().min_by_key(|(name, _)| (name.len(), name.to_string())))
        .map(|(_, data)| *data)
}

fn gather_bike_files(p: &std::path::Path) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    use anyhow::{bail, Context};
    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("edf")) {
        let bytes = std::fs::read(p).with_context(|| format!("read {p:?}"))?;
        return Ok(vec![("model.edf".to_string(), bytes)]);
    }
    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pkz")) {
        return pkz::read_selected(p, wanted_bike_file);
    }
    if p.is_dir() {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(p).with_context(|| format!("read dir {p:?}"))? {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).map(str::to_string);
            if path.is_file() && name.as_deref().is_some_and(wanted_bike_file) {
                if let (Some(name), Ok(bytes)) = (name, std::fs::read(&path)) {
                    out.push((name, bytes));
                }
            }
        }
        // A mesh of any name will do — `model.edf` is the convention, not a rule.
        if out.iter().any(|(n, _)| n.to_ascii_lowercase().ends_with(".edf")) {
            return Ok(out);
        }
        let sibling = p.with_extension("pkz");
        if sibling.exists() {
            return pkz::read_selected(&sibling, wanted_bike_file);
        }
        bail!("no .edf mesh for bike folder {p:?}");
    }
    bail!("can't load a bike model from {p:?}")
}

/// Add `incoming` to `files`, replacing any entry of the same name — later wins, which is
/// how the game reads a bike too: loose files layer over the packed archive, and a swap's
/// files layer over the loose ones.
fn overlay_files(files: &mut Vec<(String, Vec<u8>)>, incoming: Vec<(String, Vec<u8>)>) {
    for (name, data) in incoming {
        let bn = name.rsplit(['/', '\\']).next().unwrap_or(&name).to_ascii_lowercase();
        match files
            .iter_mut()
            .find(|(n, _)| n.rsplit(['/', '\\']).next().unwrap_or(n).eq_ignore_ascii_case(&bn))
        {
            Some(slot) => *slot = (name, data),
            None => files.push((name, data)),
        }
    }
}

/// Read the named files out of `dir`, keeping only what the viewer draws with.
fn read_named(dir: &std::path::Path, names: &[String]) -> Vec<(String, Vec<u8>)> {
    names
        .iter()
        .filter(|n| wanted_bike_file(n))
        .filter_map(|n| std::fs::read(dir.join(n)).ok().map(|b| (n.clone(), b)))
        .collect()
}

/// The bike's packed model, either inside the folder or as its `<Bike>.pkz` sibling —
/// both layouts exist, and it's the fallback a Stock preview shows.
fn packed_bike(bike_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Ok(rd) = std::fs::read_dir(bike_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pkz")) {
                return Some(p);
            }
        }
    }
    let sibling = bike_dir.with_extension("pkz");
    sibling.exists().then_some(sibling)
}

/// The bytes behind a `PreviewSet`: the loose files that stay, with the variant's laid over
/// them. Reverting to Stock parks every loose mesh, so the packed model goes underneath —
/// otherwise there'd be nothing to draw, which is precisely what Stock means.
fn gather_preview_files(
    set: &modelswap::PreviewSet,
) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    use anyhow::bail;
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let loose_mesh = set
        .root_keep
        .iter()
        .chain(set.variant_files.iter())
        .any(|f| bikefiles::is_mesh(f));
    if !loose_mesh {
        if let Some(pkz) = packed_bike(&set.bike_dir) {
            overlay_files(&mut out, pkz::read_selected(&pkz, wanted_bike_file)?);
        }
    }
    overlay_files(&mut out, read_named(&set.bike_dir, &set.root_keep));
    overlay_files(&mut out, read_named(&set.variant_dir, &set.variant_files));
    if !out.iter().any(|(n, _)| bikefiles::is_mesh(n)) {
        bail!("this model has no mesh to show — the bike would have no model at all");
    }
    Ok(out)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RiderPart {
    part: String,
    nodes: Vec<edf::EdfNode>,
    textures: Vec<paint::PaintTexture>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RiderModel {
    parts: Vec<RiderPart>,
}

#[tauri::command]
async fn load_rider_model(
    app: tauri::AppHandle,
    loadout: presets::Loadout,
) -> Result<RiderModel, String> {
    tauri::async_runtime::spawn_blocking(move || load_rider_model_blocking(app, loadout))
        .await
        .map_err(|e| format!("load_rider_model task failed: {e}"))?
}

fn load_rider_model_blocking(
    app: tauri::AppHandle,
    loadout: presets::Loadout,
) -> Result<RiderModel, String> {
    let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
    let base = library::mods_subdir(&cfg.mods_path, "mods/rider");
    let mut parts = Vec::new();

    for spec in &GEAR {
        let (model, paint, goggles) = match spec.part {
            "helmet" => (
                loadout.helmet.as_str(),
                loadout.helmet_paint.as_str(),
                loadout.goggles_paint.as_str(),
            ),
            "boots" => (loadout.boots.as_str(), loadout.boots_paint.as_str(), ""),
            _ => (loadout.protection.as_str(), loadout.protection_paint.as_str(), ""),
        };
        if let Some(p) = load_gear(&cfg, &base, spec, model, paint, goggles, &loadout.rider) {
            parts.push(p);
        }
    }

    let suit = load_rider_paint(&cfg, &base, "suit", &loadout.rider, "paints", &loadout.suit_paint);
    let gloves =
        load_rider_paint(&cfg, &base, "gloves", &loadout.rider, "gloves", &loadout.gloves_paint);
    if !loadout.suit_paint.is_empty() && suit.is_none() {
        log::warn!("[rider] suit paint '{}' did not load for profile '{}'", loadout.suit_paint, loadout.rider);
    }
    if !loadout.gloves_paint.is_empty() && gloves.is_none() {
        log::warn!("[rider] glove paint '{}' did not load for profile '{}'", loadout.gloves_paint, loadout.rider);
    }
    let suit_texs = suit.as_ref().map(|s| s.textures.clone()).unwrap_or_default();
    let glove_texs = gloves.as_ref().map(|g| g.textures.clone()).unwrap_or_default();
    let mut body_texs = suit_texs;
    body_texs.extend(glove_texs);
    match load_rider_body(&cfg, &loadout.rider, body_texs) {
        Some(body) => parts.push(body),
        None => {
            if let Some(s) = suit {
                parts.push(s);
            }
            if let Some(g) = gloves {
                parts.push(g);
            }
        }
    }

    Ok(RiderModel { parts })
}

#[tauri::command]
async fn load_rider_body_model(
    app: tauri::AppHandle,
    profile: String,
) -> Result<Vec<edf::EdfNode>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        Ok(load_rider_body_nodes(&cfg, &profile).unwrap_or_default())
    })
    .await
    .map_err(|e| format!("load_rider_body_model task failed: {e}"))?
}

/// The textures a body mesh carries itself, memoised alongside the mesh.
///
/// These depend on the model and not on the loadout, but reaching them means reading the
/// whole `rider.edf` back — 67 MB for Rider+. The viewer reloads on every loadout change, so
/// without this a rider wearing a kit that leaves one slot bare re-reads the model each time
/// you touch a dropdown.
///
/// Only textures the viewer could actually draw are decoded. Skin renders as flat colour and
/// the `w_` planes render as nothing, so inflating and re-encoding them is time spent on
/// pixels no one will ever see — and on a rider body that decode costs more than parsing the
/// mesh does.
fn body_textures(src: &BodySource, profile: &str) -> Option<Vec<paint::PaintTexture>> {
    static C: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Vec<paint::PaintTexture>>>,
    > = std::sync::OnceLock::new();
    let cache = C.get_or_init(Default::default);
    let key = src.cache_key(profile);
    if let Some(t) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return Some(t);
    }
    let drawn = |name: &str| !matches!(body_slot(Some(name)).as_str(), "hide" | "face");
    let texs = paint::extract_edf_textures_where(&src.read(profile)?, drawn);
    if let Ok(mut c) = cache.lock() {
        c.insert(key, texs.clone());
    }
    Some(texs)
}

fn load_rider_body(
    cfg: &config::AppConfig,
    profile: &str,
    mut textures: Vec<paint::PaintTexture>,
) -> Option<RiderPart> {
    let profile = rider_profile_or_stock(profile);
    let src = rider_body_source(cfg, profile)?;
    let nodes = rider_body_nodes(&src, profile)?;

    // Whatever the mesh asks for that no paint supplies, the model itself supplies: the
    // supermoto rider ships no `.pnt` at all and wears its baked textures, and a custom
    // model paints its own extra pieces into the mesh. Reading the file back costs real
    // time on a 60 MB body, so only a name actually missing pays for it.
    let supplied: std::collections::HashSet<String> =
        textures.iter().map(|t| t.name.to_ascii_lowercase()).collect();
    let wanted: std::collections::HashSet<String> = nodes
        .iter()
        .flat_map(|n| n.submeshes.iter().filter_map(|s| s.texture.as_deref()))
        .map(|t| t.to_ascii_lowercase())
        // `hide` draws nothing and `face` is bare skin — neither wants a texture.
        .filter(|t| t != "hide" && t != "face" && !supplied.contains(t))
        .collect();
    if !wanted.is_empty() {
        match body_textures(&src, profile) {
            Some(own) => textures.extend(
                own.into_iter().filter(|t| wanted.contains(&t.name.to_ascii_lowercase())),
            ),
            None => log::warn!("[rider] body '{profile}' could not be re-read for {wanted:?}"),
        }
    }

    log::info!(
        "[rider] body '{profile}' loaded from {src:?}: {} nodes, tex={:?}",
        nodes.len(),
        textures.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
    );
    Some(RiderPart {
        part: "body".into(),
        nodes,
        textures,
    })
}

/// Stand a rider body up.
///
/// Rider meshes don't agree on which axis is up. The stock motocross rider is authored Y-up;
/// the supermoto rider and Rider+ are Z-up and arrive lying on their back. The viewer anchors
/// every piece of gear to a fraction of the body's height, so a body on its side doesn't just
/// look wrong — it measures a quarter of a metre tall instead of a metre and a bit, and the
/// helmet and boots scale down to specks and sink into the torso.
///
/// A rider is a standing figure: its longest axis is its height. Where that's Z, the mesh is
/// authored in the other convention and takes that convention's one fixed rotation. Where
/// it's already Y, leave the mesh alone — guessing at a body that's already upright is how
/// the stock rider would get broken to fix a custom one.
///
/// The rotation is a half turn about Y on top of the quarter turn about X. Standing the body
/// up alone leaves it facing backwards, which the name and number planes give away: they sit
/// on a rider's back, and on the stock motocross rider — authored upright, so correct by
/// construction — they sit behind its centre. On the Z-up meshes a bare quarter turn puts
/// them in front. Both halves are needed together: `y = -z, z = -y` on its own mirrors the
/// mesh rather than turning it, which would swap the rider's left and right hands.
fn stand_body_upright(nodes: &mut [edf::EdfNode]) {
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for n in nodes.iter() {
        for v in n.positions.chunks_exact(3) {
            for a in 0..3 {
                lo[a] = lo[a].min(v[a]);
                hi[a] = hi[a].max(v[a]);
            }
        }
    }
    let ext = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    if ext[2] <= ext[1] || ext[2] <= ext[0] {
        return;
    }
    for n in nodes.iter_mut() {
        for v in n.positions.chunks_exact_mut(3).chain(n.normals.chunks_exact_mut(3)) {
            let (x, y, z) = (v[0], v[1], v[2]);
            v[0] = -x;
            // Negated, not taken as-is: these meshes lie head-away, with the head at the
            // most negative Z, so this is what puts it at the top.
            v[1] = -z;
            v[2] = -y;
        }
    }
    log::info!("[rider] body was authored Z-up ({ext:?}); stood it upright");
}

/// Bind each body submesh to the texture the mesh itself says it wears.
///
/// A material index is not a slot: it counts into the model's own texture list, and that
/// list is written in the exporter's order. `default_mx` happens to put the suit first and
/// the gloves second; `default_sm` puts the face second and its gloves third; Rider+ puts
/// the gloves first and the suit last. So a fixed index→slot map is one model memorised —
/// it already swaps face and gloves on the supermoto rider, and on a custom model it smears
/// the glove texture across the whole body. Read the name the model was drawn against
/// instead, the same reading the bike and gear viewers take — through each node's own
/// material table, since an id counts into the table of the part that owns it and means
/// nothing outside it (see `bind_textures`).
fn bind_body_submeshes(nodes: &mut [edf::EdfNode], mesh: &[u8]) {
    let colors = edf::color_textures(mesh);
    if colors.is_empty() {
        // A mesh whose texture table doesn't parse tells us nothing; the stock layout is
        // still right for the model the app has always shown.
        return tag_body_materials(nodes);
    }
    bind_body_to_colors(nodes, &colors);
}

/// The binding itself, split out so a test can drive it without a mesh blob: reading a
/// material id through the wrong node's table is the failure worth pinning down, and it
/// needs two nodes whose tables disagree, not a parseable `.edf`.
fn bind_body_to_colors(nodes: &mut [edf::EdfNode], colors: &[edf::EmbeddedTexture]) {
    for node in nodes.iter_mut() {
        // Disjoint field borrows: the node's table is read while its submeshes are written.
        let materials = &node.materials;
        for sm in node.submeshes.iter_mut() {
            let emb = sm
                .mat
                .and_then(|m| materials.get(m as usize).copied().flatten())
                .and_then(|slot| colors.get(slot))
                .map(|t| t.name.as_str());
            sm.texture = Some(body_slot(emb));
        }
    }
}

/// The viewer slot an embedded texture name belongs to. The `w_` planes are decals the game
/// composites a rider's name and number onto and carry no look of their own, and skin must
/// never wear the kit. Everything else keeps its own name, so a paint replaces it by name
/// and a piece the paint doesn't cover falls back to the model's own texture.
fn body_slot(name: Option<&str>) -> String {
    let Some(n) = name else { return "rider".into() };
    let l = n.to_ascii_lowercase();
    if l.starts_with("w_") {
        return "hide".into();
    }
    if l.contains("face") {
        return "face".into();
    }
    l
}

fn tag_body_materials(nodes: &mut [edf::EdfNode]) {
    for n in nodes.iter_mut() {
        for sm in n.submeshes.iter_mut() {
            sm.texture = Some(
                match sm.mat {
                    Some(1) => "gloves",
                    Some(2) => "face",
                    Some(3) | Some(4) => "hide",
                    _ => "rider",
                }
                .into(),
            );
        }
    }
}

/// Rider bodies and helmets are small next to a bike, and a session cycles through a
/// handful of them, so this can hold more entries than the bike cache does.
const MESH_CACHE_CAP: usize = 12;

fn pkz_mesh_cache() -> &'static std::sync::Mutex<lru::Lru<Vec<edf::EdfNode>>> {
    static C: std::sync::OnceLock<std::sync::Mutex<lru::Lru<Vec<edf::EdfNode>>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(lru::Lru::new(MESH_CACHE_CAP)))
}

fn keep_lod0(nodes: &mut Vec<edf::EdfNode>) {
    let mut seen = std::collections::HashSet::new();
    nodes.retain(|n| n.name.is_empty() || seen.insert(n.name.clone()));
}

fn cached_mesh(key: &str) -> Option<Vec<edf::EdfNode>> {
    // `mut` because a hit marks the entry as the warm one — see `lru::Lru`.
    pkz_mesh_cache().lock().ok().and_then(|mut c| c.get(key).cloned())
}

/// Parse a mesh into viewer space and memoise it. `prepare` runs once, on the parse that
/// populates the cache, for work that depends on the file rather than on the loadout.
fn mesh_from_bytes(
    key: String,
    data: &[u8],
    // Gear is read with [`edf::parse_gear`] — see there for why the two readings differ.
    gear: bool,
    prepare: impl FnOnce(&mut Vec<edf::EdfNode>, &[u8]),
) -> Option<Vec<edf::EdfNode>> {
    let mut nodes = if gear { edf::parse_gear(data) } else { edf::parse(data) };
    edf::to_right_handed(&mut nodes);
    keep_lod0(&mut nodes);
    if nodes.is_empty() {
        return None;
    }
    prepare(&mut nodes, data);
    if let Ok(mut c) = pkz_mesh_cache().lock() {
        c.insert(key, nodes.clone());
    }
    Some(nodes)
}

/// A gear mesh out of a game archive, memoised. Gear-only: the two readings would otherwise
/// share a cache entry, and whichever asked first would settle how the other saw the file.
fn load_pkz_mesh(pkz: &std::path::Path, entry: &str) -> Option<Vec<edf::EdfNode>> {
    let key = format!("{}:{}#gear", bike_cache_key(&pkz.to_string_lossy()), entry);
    if let Some(n) = cached_mesh(&key) {
        return Some(n);
    }
    mesh_from_bytes(key, &read_pkz_entry(pkz, entry)?, true, |_, _| {})
}

/// The stock rider profiles the game itself ships. They're the fallback for a custom model
/// that brings a mesh but none of the kits meant to be worn on it.
const STOCK_RIDER_PROFILES: [&str; 2] = ["default_mx", "default_sm"];

fn rider_profile_or_stock(profile: &str) -> &str {
    if profile.is_empty() { STOCK_RIDER_PROFILES[0] } else { profile }
}

/// Where a rider profile's body mesh lives.
///
/// A rider model is a whole new `rider.edf`, not a texture — Rider+ and its variants install
/// as folders under `mods/rider/riders`. The game's own archive is the last place to look,
/// not the only one: reading only `rider.pkz` left a picked custom profile rendering no body
/// at all, just gear floating where the rider should be.
#[derive(Debug, Clone)]
enum BodySource {
    /// `mods/rider/riders/<profile>/rider.edf`, installed loose — the shape every rider
    /// model on mxb-mods ships.
    Loose(std::path::PathBuf),
    /// A profile packed as `<profile>.pkz`, or the game's own `rider.pkz`.
    Packed(std::path::PathBuf),
}

impl BodySource {
    fn cache_key(&self, profile: &str) -> String {
        match self {
            Self::Loose(p) => bike_cache_key(&p.to_string_lossy()),
            Self::Packed(p) => format!("{}:{profile}", bike_cache_key(&p.to_string_lossy())),
        }
    }

    /// The mesh bytes. A packed profile is read at the entry the game uses, then — for a
    /// repack that flattened the folder tree — by any `rider.edf` in the archive.
    fn read(&self, profile: &str) -> Option<Vec<u8>> {
        match self {
            Self::Loose(p) => std::fs::read(p).ok(),
            Self::Packed(p) => read_pkz_entry(p, &format!("rider/riders/{profile}/rider.edf"))
                .or_else(|| read_pkz_basename(p, "rider.edf")),
        }
    }
}

/// One named file out of an archive wherever it sits in the tree, for repacks that don't
/// keep the game's layout. Only the wanted entry is inflated.
fn read_pkz_basename(pkz: &std::path::Path, base: &str) -> Option<Vec<u8>> {
    let want = |n: &str| {
        n.replace('\\', "/")
            .rsplit('/')
            .next()
            .is_some_and(|b| b.eq_ignore_ascii_case(base))
    };
    pkz::read_selected(pkz, want).ok()?.into_iter().next().map(|(_, d)| d)
}

fn rider_body_source(cfg: &config::AppConfig, profile: &str) -> Option<BodySource> {
    let riders = library::mods_subdir(&cfg.mods_path, "mods/rider/riders");
    let loose = riders.join(profile).join("rider.edf");
    if loose.is_file() {
        return Some(BodySource::Loose(loose));
    }
    let packed = riders.join(format!("{profile}.pkz"));
    if packed.is_file() {
        return Some(BodySource::Packed(packed));
    }
    resolve_game_pkz(cfg, "rider.pkz").map(BodySource::Packed)
}

/// The body mesh, submeshes already bound to their textures. Binding depends on the model
/// and not on the loadout, so it happens on the parse that fills the cache — a paint change
/// must not re-read a 60 MB body.
fn rider_body_nodes(src: &BodySource, profile: &str) -> Option<Vec<edf::EdfNode>> {
    let key = src.cache_key(profile);
    if let Some(n) = cached_mesh(&key) {
        return Some(n);
    }
    mesh_from_bytes(key, &src.read(profile)?, false, |nodes, data| {
        bind_body_submeshes(nodes, data);
        stand_body_upright(nodes);
    })
}

fn load_rider_body_nodes(cfg: &config::AppConfig, profile: &str) -> Option<Vec<edf::EdfNode>> {
    let profile = rider_profile_or_stock(profile);
    rider_body_nodes(&rider_body_source(cfg, profile)?, profile)
}

fn resolve_game_pkz(cfg: &config::AppConfig, name: &str) -> Option<std::path::PathBuf> {
    let gp = cfg.game_path.trim();
    if !gp.is_empty() {
        let p = std::path::Path::new(gp).join(name);
        if p.exists() {
            return Some(p);
        }
    }
    let p = std::path::Path::new(&cfg.mods_path).join(name);
    if p.exists() {
        return Some(p);
    }
    // Last resort for configs that predate game-path auto-detection: scan Steam now.
    let detected = config::detect_game_path(cfg.game())?;
    let p = std::path::Path::new(&detected).join(name);
    p.exists().then_some(p)
}

fn read_pkz_entry(pkz: &std::path::Path, entry: &str) -> Option<Vec<u8>> {
    let matches = |name: &str| name.replace('\\', "/").eq_ignore_ascii_case(entry);
    if pkz::is_plain_zip(pkz) {
        let file = std::fs::File::open(pkz).ok()?;
        let mut zip = zip::ZipArchive::new(file).ok()?;
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).ok()?;
            if matches(f.name()) {
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut f, &mut buf).ok()?;
                return Some(buf);
            }
        }
        return None;
    }
    pkz::read_all(pkz)
        .ok()?
        .into_iter()
        .find(|(n, _)| matches(n))
        .map(|(_, d)| d)
}

#[tauri::command]
async fn load_gear_model(
    path: String,
    part: String,
    paint: Option<String>,
    goggles: Option<String>,
    // Show the mesh's own textures instead of a `.pnt` — the stock look. Separate flags
    // because a helmet's goggles are picked independently of its shell.
    stock: Option<bool>,
    stock_goggles: Option<bool>,
) -> Result<RiderPart, String> {
    tauri::async_runtime::spawn_blocking(move || {
        load_gear_model_blocking(
            path,
            part,
            paint,
            goggles,
            stock.unwrap_or(false),
            stock_goggles.unwrap_or(false),
            // A library preview shows one mod on its own — nothing outside it to gather.
            Vec::new(),
        )
    })
    .await
    .map_err(|e| format!("load_gear_model task failed: {e}"))?
}

#[tauri::command]
async fn load_stock_gear_model(
    app: tauri::AppHandle,
    part: String,
    paint_path: Option<String>,
) -> Result<RiderPart, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = config::load(&app).map_err(|e| format!("{e:#}"))?;
        let spec = GEAR
            .iter()
            .find(|g| g.part == part)
            .ok_or_else(|| format!("no stock model for gear slot '{part}'"))?;
        let pkz = resolve_game_pkz(&cfg, "rider.pkz")
            .ok_or_else(|| "game path not set or rider.pkz not found".to_string())?;
        let folder = format!("rider/{}/{}", spec.pkz_kind, spec.default_name);
        // The entry is kept, not just the nodes: with no paint to show, the mesh's own
        // textures are the look, and they're read back out of the file it came from.
        let named = format!("{folder}/{}", spec.mesh);
        let (entry, nodes) = load_pkz_mesh(&pkz, &named)
            .map(|n| (named, n))
            .or_else(|| {
                let alt = stock_gear_entry(&pkz, &folder)?;
                let n = load_pkz_mesh(&pkz, &alt)?;
                Some((alt, n))
            })
            .ok_or_else(|| format!("stock {part} mesh not found in rider.pkz"))?;
        let textures = match paint_path.filter(|s| !s.is_empty()) {
            Some(p) => std::fs::read(&p)
                .ok()
                .and_then(|d| paint::decode_any(&d).ok())
                .map(|pnt| pnt.into_par_iter().map(paint::into_texture).collect())
                .unwrap_or_default(),
            // Nothing to preview → the stock look, which is what the mesh carries. Not the
            // first `.pnt` in the folder: that's a paint like any other, and picking it here
            // is what showed the stock helmet bronze everywhere else. Companion maps are
            // skipped — the viewer never draws a normal or roughness sheet, so inflating
            // one is time spent on pixels no one sees.
            None => read_pkz_entry(&pkz, &entry)
                .map(|d| paint::extract_edf_textures_where(&d, |n| !is_companion_map(n)))
                .unwrap_or_default(),
        };
        Ok(RiderPart {
            part: spec.part.into(),
            nodes,
            textures,
        })
    })
    .await
    .map_err(|e| format!("load_stock_gear_model task failed: {e}"))?
}

#[derive(serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct GearPaints {
    paints: Vec<String>,
    goggles: Vec<String>,
    /// The mesh carries its own shell / goggle texture, so a "Stock" entry is worth
    /// offering next to the packed paints. Preview-only — never a loadout value, since…44682 tokens truncated…  if let Err(e) = overlay::register(handle, &cfg) {
                    log::warn!("overlay hotkey not registered: {e}");
                }
            } else {
                log::info!("no MX Bikes folder found — showing first-run setup");
            }
            // Watch for a cheat attached to the game. Started unconditionally rather than
            // from the Play button, and outside the `load_or_detect` arm above: the game is
            // just as often launched from Steam, and a machine with no config yet still has
            // a process list worth reading. The watcher re-reads the setting every pass, so
            // it costs an idle poll and nothing else when it is turned off.
            integritywatch::start(handle);
            shop_session::load_session(handle);
            shop_catalog_session::load(handle);
            mxb_session::load(handle);
            imgcache::start_maintenance(handle);
            memwatch::start();
            // Only registers the result listener and stashes the handle — the hidden window
            // isn't built until something is actually refused.
            mxb_fetch::init(handle);
            // Same again for the shop's signed-in half. Nothing opens until the purchases tab
            // is actually used, so a user who never signs in never pays for the window.
            shop_fetch::init(handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            // The overlay is a HUD over a running game, so clicking back into the game
            // has to put it away — an unfocused webview stops repainting and leaves an
            // empty frame over the game that still eats clicks.
            if let WindowEvent::Focused(false) = event {
                if window.label() == overlay::LABEL {
                    overlay::on_focus_lost(window.app_handle());
                    return;
                }
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Closing the overlay (Alt+F4, its own button) parks it rather than
                // destroying it, so the next hotkey press doesn't rebuild the webview.
                if window.label() == overlay::LABEL {
                    api.prevent_close();
                    let _ = overlay::hide(window.app_handle());
                    return;
                }
                // Everything else — the clearance check, the shop login — closes for real.
                if !parks_in_tray(window.label()) {
                    return;
                }
                let cfg = config::load(window.app_handle()).unwrap_or_default();
                // Never on Linux: the tray runs through libayatana-appindicator, which
                // doesn't deliver click events to Tauri and isn't present at all on a
                // stock GNOME desktop. Hiding there can strand the window with no way
                // back, so closing closes.
                let tray_can_restore = cfg!(not(target_os = "linux"));
                if cfg.run_in_background && tray_can_restore && !cfg!(debug_assertions) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        // Wrapped rather than passed straight in: `ipc_allowed` closes the hole that giving
        // the hidden mxb-mods.com window a capability would otherwise open. See its doc.
        .invoke_handler({
            // The macro is generic over the runtime; naming `Wry` here is what lets the
            // wrapper below infer what it is wrapping.
            let handler: fn(tauri::ipc::Invoke<tauri::Wry>) -> bool = tauri::generate_handler![
            is_configured,
            get_config,
            create_config,
            bike_preview_available,
            app_platform,
            search_mods,
            get_mod_detail,
            get_mod_ratings,
            get_installed_mods,
            scan_library,
            get_pkz_meta_cached,
            get_pkz_meta,
            get_pkz_preview,
            read_track_info,
            load_track_terrain,
            load_track_overview,
            diagnose_track,
            unpack_paint,
            texture_bytes,
            watch_paint_files,
            unpack_pkz,
            load_bike_model,
            preview_model_swap,
            load_rider_model,
            load_rider_body_model,
            load_gear_model,
            load_stock_gear_model,
            list_gear_paints,
            list_installed_gear_paints,
            paint_studio_load,
            paint_studio_pixels,
            paint_studio_stage,
            paint_studio_target,
            paint_studio_save,
            paint_studio_extract,
            paint_studio_hints,
            scan_rider_targets,
            scan_gear_repairs,
            repair_gear,
            scan_bike_targets,
            scan_model_swaps,
            apply_model_swap,
            scan_sound_swaps,
            apply_sound_swap,
            bind_sound,
            unbind_sound,
            reshade_status,
            set_reshade_path,
            apply_reshade_preset,
            delete_reshade_preset,
            detect_loose_swaps,
            register_loose_swaps,
            detect_orphaned_setup,
            repair_orphaned_setup,
            add_to_library,
            cancel_install,
            import_file,
            plan_drop,
            repreview_drop,
            commit_drop,
            cancel_drop,
            move_mod,
            uninstall_mod,
            reveal_in_explorer,
            logs_info,
            open_logs_folder,
            export_logs,
            set_game_path,
            set_wine_runner,
            wine_host_info,
            set_mods_path,
            set_intro_seen,
            set_seen_version,
            set_profiles_path,
            detect_game_path,
            count_profiles_in,
            get_mods_root,
            set_run_in_background,
            set_launch_at_startup,
            set_auto_run_frostmod,
            set_instant_refresh,
            overlay_toggle,
            overlay_hide,
            overlay_open_main,
            overlay_state,
            set_overlay_enabled,
            set_overlay_hotkey,
            voice_devices,
            set_voice_enabled,
            set_voice_input_device,
            set_voice_output_device,
            set_voice_ptt_hotkey,
            set_voice_levels,
            set_voice_toggle_to_talk,
            voice_meter_start,
            voice_meter_stop,
            voice_test_output,
            set_watch_mods_reload,
            integrity_status,
            integrity_scan_now,
            set_integrity_watch,
            set_integrity_report,
            integrity_server_reports,
            frostmod_reload,
            frostmod_running,
            garage_scan_bikes,
            garage_swap_bike,
            frostmod_status,
            frostmod_install,
            frostmod_install_runtime,
            frostmod_repair_runtimes,
            runtime_downloads,
            frostmod_start,
            frostmod_stop,
            launch_game,
            join_server,
            experimental_state,
            set_experimental,
            enroll_account,
            set_guid,
            publish_paints,
            sync_paints,
            list_servers,
            save_servers,
            cp_servers,
            server_status,
            server_tracks,
            server_probe,
            publish_server,
            unpublish_server,
            provision_server,
            fleet_state,
            cloud_servers,
            destroy_cloud_server,
            parse_pairing,
            server_action,
            server_set_config,
            game_running,
            shop_login,
            shop_status,
            shop_logout,
            shop_my_downloads,
            shop_match_catalog,
            shop_install,
            shop_installed_map,
            record_download,
            download_history,
            forget_download,
            clear_download_history,
            shop_catalog_available,
            shop_catalog_status,
            shop_catalog_categories,
            shop_catalog_search,
            shop_catalog_detail,
            shop_catalog_refresh,
            presets_list_profiles,
            presets_list_bikes,
            presets_read_loadout,
            presets_slots,
            list_games,
            set_active_game,
            presets_apply,
            presets_list,
            presets_save,
            presets_delete,
            presets_export,
            presets_decode,
            presets_import,
            preset_bundle_stats,
            preset_bundle_create,
            preset_bundle_import,
            file_share_plan,
            file_share_create,
            file_share_preview,
            file_share_import,
            mods_state_scan,
            mods_state_plan,
            mods_state_set,
            mods_state_delete,
            mods_state_apply,
                mods_state_restore_all
            ];
            move |invoke: tauri::ipc::Invoke<tauri::Wry>| {
                let (label, command) = (
                    invoke.message.webview().label().to_string(),
                    invoke.message.command().to_string(),
                );
                if !ipc_allowed(&label, &command) {
                    log::warn!("refused IPC '{command}' from the '{label}' window");
                    invoke.resolver.reject("not permitted from this window");
                    return true;
                }
                handler(invoke)
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod webview_env_tests {
    use super::*;
    use std::collections::HashMap;

    fn defaults(env: GraphicsEnv) -> HashMap<&'static str, &'static str> {
        webview_env_defaults(env).into_iter().collect()
    }

    /// SteamOS, both modes: Desktop is Plasma Wayland and gamescope is a compositor of its
    /// own, and each runs an XWayland the app can land on instead.
    const STEAMOS: GraphicsEnv = GraphicsEnv {
        wayland: true,
        x_server: true,
        safe_mode: false,
    };

    /// The cheap fix for the silent no-paint fault, and it costs a machine nothing that
    /// works already — so it goes on everywhere rather than being guessed at.
    #[test]
    fn the_shared_memory_renderer_is_always_the_default() {
        for env in [
            GraphicsEnv::default(),
            STEAMOS,
            GraphicsEnv { wayland: false, ..STEAMOS },
            GraphicsEnv { safe_mode: true, ..STEAMOS },
        ] {
            assert_eq!(
                defaults(env).get("WEBKIT_DISABLE_DMABUF_RENDERER"),
                Some(&"1"),
                "{env:?} should have fallen back to shared memory",
            );
        }
    }

    /// Nothing here forces a backend of its own accord any more. The EGL abort this used to
    /// answer for was the AppImage's bundled libwayland, taken out of the bundle itself; an
    /// AppImage is on XWayland regardless, because its AppRun hook says so before this
    /// process starts.
    #[test]
    fn a_wayland_session_is_left_on_its_own_backend() {
        assert_eq!(defaults(STEAMOS).get("GDK_BACKEND"), None);
    }

    /// Without an X server to fall back to, forcing the backend trades a white screen for
    /// no window at all.
    #[test]
    fn wayland_with_no_x_server_is_left_alone() {
        let no_xwayland = GraphicsEnv { x_server: false, ..STEAMOS };
        assert_eq!(defaults(no_xwayland).get("GDK_BACKEND"), None);

        let safe = GraphicsEnv { safe_mode: true, ..no_xwayland };
        assert_eq!(defaults(safe).get("GDK_BACKEND"), None);
    }

    /// GTK already picks X11 there; saying so again would only be noise in the log — and
    /// safe mode has nothing to move the session *to*.
    #[test]
    fn a_plain_x11_session_needs_no_override() {
        let x11_only = GraphicsEnv { wayland: false, ..STEAMOS };
        assert_eq!(defaults(x11_only).get("GDK_BACKEND"), None);

        let safe = GraphicsEnv { safe_mode: true, ..x11_only };
        assert_eq!(defaults(safe).get("GDK_BACKEND"), None);
    }

    /// The escape hatch to hand someone whose screen is still white: every knob at once.
    #[test]
    fn safe_mode_takes_the_gpu_out_of_it() {
        let vars = defaults(GraphicsEnv { safe_mode: true, ..STEAMOS });
        assert_eq!(vars.get("GDK_BACKEND"), Some(&"x11"));
        assert_eq!(vars.get("WEBKIT_DISABLE_COMPOSITING_MODE"), Some(&"1"));
        assert_eq!(vars.get("LIBGL_ALWAYS_SOFTWARE"), Some(&"1"));
    }

    /// Nothing is imposed on a desktop that was never broken.
    #[test]
    fn an_ordinary_session_gets_only_the_renderer_default() {
        let vars = defaults(GraphicsEnv::default());
        assert_eq!(vars.len(), 1);
        assert!(vars.contains_key("WEBKIT_DISABLE_DMABUF_RENDERER"));
    }
}

#[cfg(test)]
mod release_version_tests {
    use super::*;

    /// The bug this exists for: a build cut from `v0.8.0-beta.1` packages itself as `0.8.0`,
    /// so the Beta badge — which keys off a pre-release suffix — never fired on any beta.
    #[test]
    fn a_release_tag_carries_its_prerelease_suffix() {
        assert_eq!(
            pick_release_version(Some("v0.8.0-beta.1"), "0.8.0".into()),
            "0.8.0-beta.1",
        );
    }

    /// Local builds, and the `workflow_dispatch` runs that pass an empty tag.
    #[test]
    fn no_tag_leaves_the_packaged_version_alone() {
        assert_eq!(pick_release_version(None, "0.8.0".into()), "0.8.0");
        assert_eq!(pick_release_version(Some("  "), "0.8.0".into()), "0.8.0");
    }

    /// `github.ref_name` is a *branch* outside a tag push. Believing it would put "main" in
    /// the About box, which reads as a broken build rather than a misconfigured workflow.
    #[test]
    fn a_tag_that_isnt_a_version_is_ignored() {
        for junk in ["main", "chore/harden-release-binary", "vNext"] {
            assert_eq!(
                pick_release_version(Some(junk), "0.8.0".into()),
                "0.8.0",
                "{junk} should not have been believed",
            );
        }
    }

    /// The `v` is a tag convention, not part of the version — the UI adds its own.
    #[test]
    fn the_tags_v_prefix_is_optional() {
        assert_eq!(pick_release_version(Some("0.9.0"), "0.8.0".into()), "0.9.0");
        assert_eq!(pick_release_version(Some("v0.9.0"), "0.8.0".into()), "0.9.0");
    }
}

#[cfg(test)]
mod autostart_tests {
    use super::*;

    #[test]
    fn the_setting_is_honoured_when_the_binding_is_current() {
        assert_eq!(autostart_action(true, false, false), Autostart::Enable);
        assert_eq!(autostart_action(true, true, false), Autostart::Leave);
        assert_eq!(autostart_action(false, true, false), Autostart::Disable);
        assert_eq!(autostart_action(false, false, false), Autostart::Leave);
    }

    /// The rename bug: the entry exists, so nothing looks wrong, but it names a binary that
    /// is gone. Without this the app quietly stops starting at login for everyone upgrading.
    #[test]
    fn an_entry_written_for_the_old_binary_is_rewritten() {
        assert_eq!(autostart_action(true, true, true), Autostart::Rebind);
    }

    /// Whoever turned it off gets it off, however old their entry is — a stale binding is a
    /// reason to rewrite the entry, never to bring one back.
    #[test]
    fn a_stale_binding_never_revives_a_disabled_login_item() {
        assert_eq!(autostart_action(false, true, true), Autostart::Disable);
        assert_eq!(autostart_action(false, false, true), Autostart::Leave);
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    /// The regression a tester's log caught: the clearance window was being parked in the
    /// tray on close, which left its label registered, so every handshake after the first
    /// failed to build a window and Retry silently did nothing for the rest of the session.
    ///
    /// Only the main window may park. Every transient window has to close for real.
    #[test]
    fn only_the_main_window_parks_in_the_tray() {
        assert!(parks_in_tray(MAIN_WINDOW));

        for transient in [
            mxb_fetch::WINDOW,
            shop_fetch::WINDOW,
            SHOP_LOGIN_WINDOW,
            overlay::LABEL, // handled earlier by its own branch, but never by this one
        ] {
            assert!(
                !parks_in_tray(transient),
                "{transient} must be destroyed on close, not hidden — a stranded label \
                 makes it unopenable for the life of the process"
            );
        }
    }

    /// The security boundary. Both fetch windows run a *remote* origin — mxb-mods.com's own
    /// page in one, mxbikes-shop.com's signed-in page in the other — and their capabilities
    /// grant IPC, which `generate_handler!` commands are not gated by. So each gets exactly one
    /// call and nothing else; if this test ever goes green on a second command, script on those
    /// sites can drive that command.
    ///
    /// The shop window is the sharper case of the two: it is signed in as the user, and
    /// `shop_install` writes files.
    #[test]
    fn the_remote_fetch_windows_may_only_emit_their_result() {
        for remote in [mxb_fetch::WINDOW, shop_fetch::WINDOW] {
            assert!(ipc_allowed(remote, "plugin:event|emit"), "{remote}");

            for forbidden in [
                "create_config",
                "install_mod",
                "mods_state_delete",
                "get_config",
                "shop_install",
                "shop_logout",
                "commit_drop",
                "record_download",
                "clear_download_history",
                "plugin:shell|open",
                "plugin:dialog|open",
                "plugin:event|listen",
                "plugin:process|restart",
            ] {
                assert!(
                    !ipc_allowed(remote, forbidden),
                    "script on {remote}'s remote origin must not be able to call {forbidden}"
                );
            }
        }
    }

    /// ...and the guard must not touch anything else. The app's own windows keep whatever
    /// their capability files grant.
    #[test]
    fn the_apps_own_windows_are_unaffected_by_the_guard() {
        for label in [MAIN_WINDOW, overlay::LABEL, SHOP_LOGIN_WINDOW] {
            for command in ["create_config", "install_mod", "plugin:event|emit"] {
                assert!(ipc_allowed(label, command), "{label} / {command}");
            }
        }
    }
}

#[cfg(test)]
mod gear_bind_tests {
    use super::{bind_gear_submeshes, edf, GearSide};

    fn submesh(name: &str) -> edf::Submesh {
        edf::Submesh {
            name: name.into(),
            tri_start: 0,
            tri_count: 1,
            texture: None,
            uv_tile: None,
            mat: None,
        }
    }

    fn node(name: &str, subs: &[&str]) -> edf::EdfNode {
        edf::EdfNode {
            name: name.into(),
            positions: Vec::new(),
            uvs: Vec::new(),
            normals: Vec::new(),
            indices: Vec::new(),
            submeshes: subs.iter().map(|s| submesh(s)).collect(),
            texture: None,
            placed: false,
            materials: Vec::new(),
        }
    }

    fn side(names: &[&str]) -> GearSide {
        GearSide::new(names.iter().map(|s| s.to_string()).collect())
    }

    fn bound(nodes: &[edf::EdfNode]) -> Vec<Option<String>> {
        nodes
            .iter()
            .flat_map(|n| {
                if n.submeshes.is_empty() {
                    vec![n.texture.clone()]
                } else {
                    n.submeshes.iter().map(|s| s.texture.clone()).collect()
                }
            })
            .collect()
    }

    // Without the mesh's materials to read, the names are all there is to go on.
    #[test]
    fn a_named_goggle_submesh_wears_the_goggle_paint() {
        let mut nodes = vec![node("helmet", &["shell", "goggle", "lens"])];
        bind_gear_submeshes(&mut nodes, None, &side(&["hjc"]), &side(&["smoke"]), &[]);
        assert_eq!(
            bound(&nodes),
            [Some("hjc".into()), Some("smoke".into()), Some("smoke".into())],
        );
    }

    // The goggles of many helmets are a node of their own, with no submesh table at all —
    // the case that had every goggle paint land on the shell instead.
    #[test]
    fn a_goggle_node_without_submeshes_wears_the_goggle_paint() {
        let mut nodes = vec![node("helmet", &[]), node("goggles", &[])];
        bind_gear_submeshes(&mut nodes, None, &side(&["hjc"]), &side(&["smoke"]), &[]);
        assert_eq!(bound(&nodes), [Some("hjc".into()), Some("smoke".into())]);
    }

    // A goggle group inside a goggle node needn't repeat the word.
    #[test]
    fn a_submesh_inherits_its_node() {
        let mut nodes = vec![node("goggles", &["strap", "glass"])];
        bind_gear_submeshes(&mut nodes, None, &side(&["hjc"]), &side(&["smoke"]), &[]);
        assert_eq!(bound(&nodes), [Some("smoke".into()), Some("smoke".into())]);
    }

    // A helmet that draws its goggles into the shell atlas still shows them.
    #[test]
    fn unpainted_goggles_fall_back_to_the_shell() {
        let mut nodes = vec![node("helmet", &["shell", "goggle"])];
        bind_gear_submeshes(&mut nodes, None, &side(&["hjc"]), &GearSide::default(), &[]);
        assert_eq!(bound(&nodes), [Some("hjc".into()), Some("hjc".into())]);
    }

    // The shell never borrows the goggle paint the other way round — that's the smear.
    #[test]
    fn an_unpainted_shell_stays_bare() {
        let mut nodes = vec![node("helmet", &["shell", "goggle"])];
        bind_gear_submeshes(&mut nodes, None, &GearSide::default(), &side(&["smoke"]), &[]);
        assert_eq!(bound(&nodes), [None, Some("smoke".into())]);
    }

    // A paint replaces the mesh's textures by name, so where it supplies the exact name a
    // piece asks for, that beats the side's primary.
    #[test]
    fn a_side_supplies_the_name_the_mesh_asks_for() {
        let s = side(&["shell_n", "shell", "strap"]);
        assert_eq!(s.primary.as_deref(), Some("shell")); // never the companion map
        assert_eq!(s.supplies("STRAP").as_deref(), Some("strap")); // case-insensitive
        assert_eq!(s.supplies("visor"), None);
    }

    // A paint baked out of Substance names its maps the exporter's way, and one of those
    // taken for the look leaves the piece wearing a normal map.
    #[test]
    fn an_exporter_named_map_is_never_the_primary() {
        let s = side(&["Vest_Normal", "Vest_BaseColor"]);
        assert_eq!(s.primary.as_deref(), Some("Vest_BaseColor"));
    }
}

/// A packed gear item and a folder of the same name are one item, not two — the case the
/// paint studio creates every time it installs a paint for a `.pkz` helmet.
#[cfg(test)]
mod gear_source_tests {
    use super::{gear_entry_key, read_gear_files};

    #[test]
    fn one_entry_however_it_is_spelled() {
        assert_eq!(gear_entry_key("helmets/Foo/paints/red.pnt"), gear_entry_key("paints/red.pnt"));
        assert_eq!(gear_entry_key("helmets/Foo/helmet.edf"), gear_entry_key("helmet.edf"));
        assert_ne!(gear_entry_key("paints/red.pnt"), gear_entry_key("goggles/red.pnt"));
    }

    #[test]
    fn a_folder_of_paints_beside_a_pkz_reads_as_both() {
        let root = std::env::temp_dir().join(format!("frost-gear-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("Helmet");
        std::fs::create_dir_all(dir.join("paints")).unwrap();
        std::fs::write(dir.join("paints").join("mine.pnt"), b"PNT\0mine").unwrap();
        // Not a real archive — `pkz::read_all` returning nothing is enough to prove the
        // folder's paint isn't lost, and a genuine `.pkz` is exercised by the pkz tests.
        std::fs::write(root.join("Helmet.pkz"), b"not really an archive").unwrap();

        for from in [dir.clone(), root.join("Helmet.pkz")] {
            let files = read_gear_files(&from).unwrap_or_default();
            assert!(
                files.iter().any(|(n, _)| n.ends_with("mine.pnt")),
                "the loose paint has to be visible whichever side the caller resolved ({from:?})"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod mesh_texture_tests {
    use super::{mesh_texture_names, paint_hints};

    /// The smallest thing `edf::embedded_textures` reads as a texture record: a
    /// null-terminated name, then the fields it validates by shape at a fixed offset.
    fn mesh_naming(texture: &str) -> Vec<u8> {
        const W_FROM_NAME: usize = 100;
        let mut b = vec![0u8; W_FROM_NAME];
        b[..texture.len()].copy_from_slice(texture.as_bytes());
        b.extend_from_slice(&64u32.to_le_bytes()); // width, from the fixed size set
        b.extend_from_slice(&64u32.to_le_bytes()); // height
        b.extend_from_slice(&[0u8; 16]); // digest
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&12u32.to_le_bytes()); // payload, counting the pad
        b.extend_from_slice(&[0u8; 8]); // pad
        b.extend_from_slice(&[1u8; 4]); // payload
        b
    }

    #[test]
    fn a_models_own_textures_come_from_its_mesh() {
        let root = std::env::temp_dir().join(format!("frost-mesh-tex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("MyBike");
        std::fs::create_dir_all(&dir).unwrap();
        // No paint here at all — the mesh is the only thing that can name `plastics`.
        std::fs::write(dir.join("model.edf"), mesh_naming("plastics")).unwrap();
        assert_eq!(mesh_texture_names(&dir), vec!["plastics".to_string()]);

        std::fs::create_dir_all(root.join("Bare")).unwrap();
        assert!(mesh_texture_names(&root.join("Bare")).is_empty(), "no mesh, nothing to say");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A model's own textures are offered where a paint for it goes, and nowhere else: the
    /// goggles beside it are a different file, painted from a different sheet.
    #[test]
    fn only_the_models_own_paints_folder_is_offered_the_mesh() {
        let root = std::env::temp_dir().join(format!("frost-mesh-hints-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let helmet = root.join("Helmet");
        std::fs::create_dir_all(helmet.join("paints")).unwrap();
        std::fs::create_dir_all(helmet.join("goggles")).unwrap();
        std::fs::write(helmet.join("helmet.edf"), mesh_naming("shell")).unwrap();

        assert_eq!(paint_hints(&helmet.join("paints")), vec!["shell".to_string()]);
        assert!(paint_hints(&helmet.join("goggles")).is_empty(), "the shell is not a goggle sheet");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole hint list for a real destination:
    /// `MXB_PAINT_DEST='…/mods/bikes/MX1OEM_2023_Husqvarna_FC_450/paints' \
    ///   cargo test paint_hints_from_env -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn paint_hints_from_env() {
        let Ok(dir) = std::env::var("MXB_PAINT_DEST") else {
            eprintln!("set MXB_PAINT_DEST to run");
            return;
        };
        let t = std::time::Instant::now();
        let names = paint_hints(std::path::Path::new(&dir));
        eprintln!("{dir} expects {names:?} ({:?})", t.elapsed());
        assert!(!names.is_empty(), "a destination with a model behind it expects something");
    }
}

#[cfg(test)]
mod gear_scene_tests {
    use super::gear_scenes;

    fn files(entries: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        entries.iter().map(|(n, d)| (n.to_string(), d.as_bytes().to_vec())).collect()
    }

    // A protection folder names its mesh for the piece, not the slot, so the loader has to
    // ask rather than guess: `neckbrace.edf` sits beside the shadow mesh it must not pick.
    #[test]
    fn a_mod_names_its_own_mesh() {
        let f = files(&[
            ("gfx.cfg", "neckbrace\n{\n\tmodel = neckbrace.hrc\n}\n"),
            ("neckbrace.hrc", "level0\n{\n\tscene = neckbrace.edf\n\tswitch = 0\n}\n"),
        ]);
        assert_eq!(gear_scenes(&f), ["neckbrace.edf"]);
    }

    // The block in `gfx.cfg` is named for the piece, and authors don't agree on the word —
    // `armour` and `neckbrace` both turn up on protection mods that ship `protection.edf`.
    #[test]
    fn the_block_name_doesnt_matter() {
        let f = files(&[
            ("gfx.cfg", "armour\n{\n\tmodel = protection.hrc\n}\n"),
            ("protection.hrc", "level0\n{\n\tscene = protection.edf\n}\n"),
        ]);
        assert_eq!(gear_scenes(&f), ["protection.edf"]);
    }

    // The stock `full` protection: two pieces worn together, each its own mesh. Taking one
    // block's mesh dressed the rider in half the item.
    #[test]
    fn a_two_piece_set_draws_both_pieces() {
        let f = files(&[
            (
                "gfx.cfg",
                "neckbrace\n{\n\tmodel = neckbrace.hrc\n}\n\narmour\n{\n\tmodel = armour.hrc\n}\n",
            ),
            ("armour.hrc", "level0\n{\n\tscene = armour.edf\n}\n"),
            ("neckbrace.hrc", "level0\n{\n\tscene = neckbrace.edf\n}\n"),
        ]);
        // Sorted by block name, so the same folder always draws in the same order.
        assert_eq!(gear_scenes(&f), ["armour.edf", "neckbrace.edf"]);
    }

    // Boots declare a left and a right, both pointing at the one mesh that holds both feet.
    // A repeat is one piece, not two — and the nested `model { file = … }` is the boots'
    // own spelling, which nothing else uses.
    #[test]
    fn two_blocks_naming_one_mesh_are_one_piece() {
        let f = files(&[
            (
                "gfx.cfg",
                "left\n{\n\tmodel\n\t{\n\t\tfile = left_boot.hrc\n\t}\n}\nright\n{\n\tmodel\n\t{\n\t\tfile = right_boot.hrc\n\t}\n}\n",
            ),
            ("left_boot.hrc", "level0\n{\n\tscene = boots.edf\n}\n"),
            ("right_boot.hrc", "level0\n{\n\tscene = boots.edf\n\tname = righta\n}\n"),
        ]);
        assert_eq!(gear_scenes(&f), ["boots.edf"]);
    }

    // A helmet names its mesh at the top level and its first-person mesh in `cockpit`. The
    // cockpit one is never drawn on the model, and picking it up put a headless shell on the
    // rider.
    #[test]
    fn the_cockpit_mesh_is_not_the_item() {
        let f = files(&[
            ("gfx.cfg", "model = helmet.hrc\nshadow = helmet_s.edf\n\ncockpit\n{\n\tmodel = c_helmet.edf\n}\n"),
            ("helmet.hrc", "level0\n{\n\tscene = helmet.edf\n}\n"),
        ]);
        assert_eq!(gear_scenes(&f), ["helmet.edf"]);
    }

    // Most gear ships nothing to read, and that's not an error — the loader falls back to
    // scanning the folder for a mesh.
    #[test]
    fn a_mod_that_says_nothing_answers_nothing() {
        assert!(gear_scenes(&files(&[("protection.edf", "EDF\0")])).is_empty());
    }
}

#[cfg(test)]
mod gear_paint_merge_tests {
    use super::GearPaints;

    fn paints(names: &[&str]) -> GearPaints {
        GearPaints {
            paints: names.iter().map(|s| s.to_string()).collect(),
            ..GearPaints::default()
        }
    }

    // The whole point: a model installed as a `.pkz` *and* as a folder of extra paints
    // offers both sets. Taking the first source is what showed one and hid the other.
    #[test]
    fn sources_add_up() {
        let mut all = paints(&["Black", "Red"]);
        all.absorb(paints(&["Purple White", "RDS Leopard"]));
        all.sort();
        assert_eq!(all.paints, ["Black", "Purple White", "RDS Leopard", "Red"]);
    }

    // A paint pack ships the same file names as the mod it was made for, so the same look
    // arrives twice. Twice in a dropdown is a bug, not a choice.
    #[test]
    fn a_repeat_is_the_same_look_twice() {
        let mut all = paints(&["Black", "Red"]);
        all.absorb(paints(&["black", "Flo"]));
        all.sort();
        assert_eq!(all.paints, ["Black", "Flo", "Red"]);
    }

    // Each source sorts its own names; merged, they have to sort as one list rather than
    // as one source appended to the next.
    #[test]
    fn the_merged_list_sorts_as_one() {
        let mut all = paints(&["Alpha", "Zulu"]);
        all.absorb(paints(&["Bravo"]));
        all.sort();
        assert_eq!(all.paints, ["Alpha", "Bravo", "Zulu"]);
    }

    // A "Stock" entry is worth offering as soon as any source's mesh carries its own look.
    #[test]
    fn stock_carries_across() {
        let mut all = GearPaints::default();
        all.absorb(GearPaints { has_stock: true, ..GearPaints::default() });
        assert!(all.has_stock);
        all.absorb(GearPaints::default());
        assert!(all.has_stock, "a later source without one doesn't take it away");
    }
}

/// Whether a side has a stock look at all — the question behind both the library's "Stock"
/// entry and what an empty paint slot resolves to on the rider. They read the same function,
/// so these cases pin down both at once.
#[cfg(test)]
mod stock_side_tests {
    use super::mesh_supplies_side;

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // The ordinary helmet: the mesh carries the sheet its paints replace, so there is a stock
    // look to show and an empty slot means it.
    #[test]
    fn the_mesh_carrying_the_painted_sheet_is_a_stock_look() {
        assert!(mesh_supplies_side(&v(&["helmet", "visor"]), &v(&["helmet"]), false));
    }

    // Case is the mod author's business, not ours — a `.pnt` replaces by name regardless.
    #[test]
    fn the_match_ignores_case() {
        assert!(mesh_supplies_side(&v(&["Helmet"]), &v(&["helmet"]), false));
    }

    // The Bell Moto 10: it embeds a tear-off film and a goggle, but the shell sheet comes from
    // its paints. Calling that a stock look drew the helmet near-blank — and now it would also
    // make an empty slot render bare, which is worse than the first-paint fallback it keeps.
    #[test]
    fn a_mesh_that_leaves_the_shell_to_its_paints_has_no_stock_look() {
        assert!(!mesh_supplies_side(&v(&["tearoff", "Racecraft"]), &v(&["airoh_shell"]), false));
    }

    // Normal and roughness maps are never the look. A mesh carrying only companions carries
    // nothing to show.
    #[test]
    fn companion_maps_dont_count_as_a_look() {
        assert!(!mesh_supplies_side(&v(&["boots_n", "boots_r"]), &v(&[]), false));
        assert!(!mesh_supplies_side(&v(&["helmet_n"]), &v(&["helmet_n"]), false));
    }

    // Nothing painted on a side at all — the stock protection slot — so whatever the mesh
    // carries for that side is the only look there is.
    #[test]
    fn with_no_paints_the_mesh_is_the_only_look() {
        assert!(mesh_supplies_side(&v(&["armor"]), &v(&[]), false));
    }

    // ...and the sides don't borrow from each other: an embedded goggle is not a shell look.
    #[test]
    fn an_unpainted_side_only_counts_its_own_sheets() {
        let embedded = v(&["goggles"]);
        assert!(mesh_supplies_side(&embedded, &v(&[]), true), "the goggle side has one");
        assert!(!mesh_supplies_side(&embedded, &v(&[]), false), "the shell does not");
    }
}

#[cfg(test)]
mod deep_link_tests {
    use super::enroll_code_from_link;

    #[test]
    fn reads_the_code_out_of_an_enroll_link() {
        assert_eq!(enroll_code_from_link("mxb://enroll?code=ABC-123").as_deref(), Some("ABC-123"));
        // A launcher that adds a trailing slash to the host must still work.
        assert_eq!(enroll_code_from_link("mxb://enroll/?code=xyz_9").as_deref(), Some("xyz_9"));
    }

    #[test]
    fn accepts_the_british_spelling_too() {
        // Whoever writes the invite link is a person, and the two spellings are a trap.
        assert_eq!(enroll_code_from_link("mxb://enroll?code=A1").as_deref(), Some("A1"));
    }

    #[test]
    fn finds_the_code_among_other_parameters() {
        assert_eq!(enroll_code_from_link("mxb://enroll?ref=discord&code=A1").as_deref(), Some("A1"));
    }

    #[test]
    fn ignores_links_that_are_not_the_enroll_route() {
        // Any page the player visits can open one of these, so anything unrecognised has
        // to be dropped rather than interpreted.
        for bad in [
            "mxb://install?code=A1",
            "mxb://enroll",
            "https://example.com/enroll?code=A1",
            "mxb://",
            "",
        ] {
            assert!(enroll_code_from_link(bad).is_none(), "{bad:?} must be ignored");
        }
    }

    #[test]
    fn refuses_a_code_that_is_not_a_plain_token() {
        // These would each go straight into the enroll field; none is a real invite code.
        for bad in [
            "mxb://enroll?code=",
            "mxb://enroll?code=a b",
            "mxb://enroll?code=../../etc",
            "mxb://enroll?code=%2E%2E",
            "mxb://enroll?code=<script>",
        ] {
            assert!(enroll_code_from_link(bad).is_none(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn refuses_an_absurdly_long_code() {
        let long = format!("mxb://enroll?code={}", "a".repeat(200));
        assert!(enroll_code_from_link(&long).is_none());
    }
}

#[cfg(test)]
mod viewer_tests {
    use std::path::{Path, PathBuf};

    fn copy_tree(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for e in std::fs::read_dir(src).unwrap().flatten() {
            let (from, to) = (e.path(), dst.join(e.file_name()));
            if from.is_dir() {
                copy_tree(&from, &to);
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }
    }

    /// How the viewer sees a bike, as a comparable shape: which parts resolved and what
    /// each submesh is bound to. Node order is fixed by `GFX_PARTS`, so this is stable.
    fn shape(m: &super::BikeModel) -> Vec<(String, Vec<(String, Option<String>)>)> {
        m.nodes
            .iter()
            .map(|n| {
                (
                    n.name.clone(),
                    n.submeshes.iter().map(|s| (s.name.clone(), s.texture.clone())).collect(),
                )
            })
            .collect()
    }

    /// The contract behind the Locker's 3D preview: what it shows is what applying the
    /// swap would give you. Run against a **real** bike, because the in-memory overlay has
    /// to pick up the `.hrc`s, `.geom` and textures the root keeps while the mesh comes
    /// from the variant folder — a synthetic bike can't exercise that chain.
    ///
    /// MXB_REAL_BIKES=~/Projects/PiBoSo/"MX Bikes" \
    ///   cargo test preview_matches_the_applied_swap -- --ignored --nocapture
    #[test]
    #[ignore]
    fn preview_matches_the_applied_swap() {
        let Ok(src_root) = std::env::var("MXB_REAL_BIKES") else {
            eprintln!("set MXB_REAL_BIKES to the MX Bikes folder to run");
            return;
        };
        let src_bikes = Path::new(&src_root).join("mods").join("bikes");
        let Some((bike, src_dir)) = std::fs::read_dir(&src_bikes)
            .expect("read bikes")
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
            .find(|(_, p)| p.is_dir() && crate::bikefiles::dir_has_mesh(p))
        else {
            eprintln!("no extracted bike with a mesh found");
            return;
        };
        eprintln!("using real bike: {bike}");

        let root: PathBuf =
            std::env::temp_dir().join(format!("frost-preview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mp = root.to_str().unwrap();
        let dst = crate::library::mods_subdir(mp, "mods/bikes").join(&bike);
        copy_tree(&src_dir, &dst);

        // A realistic swap set: the bike's own mesh under a variant name, nothing else.
        // The preview must find everything else at the root.
        let mesh = std::fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .find(|f| crate::bikefiles::is_mesh(f))
            .expect("mesh");
        let variant = dst.join("FrostMod Models").join("Factory");
        std::fs::create_dir_all(&variant).unwrap();
        std::fs::copy(dst.join(&mesh), variant.join(&mesh)).unwrap();

        let set = super::modelswap::preview_set(mp, &bike, "Factory").expect("preview set");
        eprintln!("keeps {:?} + brings {:?}", set.root_keep, set.variant_files);
        let files = super::gather_preview_files(&set).expect("preview files");
        let previewed = super::build_bike_model(
            "preview",
            "preview-test".into(),
            files,
            super::installed_paints(&set.bike_dir),
            std::time::Instant::now(),
        )
        .expect("preview builds");
        assert!(!previewed.nodes.is_empty(), "the preview drew nothing");

        super::modelswap::apply_model_swap(mp, &bike, "Factory").expect("swap applies");
        let applied = super::load_bike_model_blocking(dst.to_string_lossy().to_string())
            .expect("the swapped bike loads");

        assert_eq!(shape(&previewed), shape(&applied), "preview differs from the real swap");
        assert_eq!(
            previewed.paints.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
            applied.paints.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
            "the preview offers different paints",
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Stock parks every loose override, so there'd be nothing to draw if the packed model
    /// didn't come through underneath. Needs a real `.pkz` — that's the whole mechanism.
    ///
    /// MXB_REAL_BIKES=~/Projects/PiBoSo/"MX Bikes" \
    ///   cargo test preview_of_stock_shows_the_packed_model -- --ignored --nocapture
    #[test]
    #[ignore]
    fn preview_of_stock_shows_the_packed_model() {
        let Ok(src_root) = std::env::var("MXB_REAL_BIKES") else {
            eprintln!("set MXB_REAL_BIKES to the MX Bikes folder to run");
            return;
        };
        let src_bikes = Path::new(&src_root).join("mods").join("bikes");
        // A bike that is both extracted *and* packed — the loose files hide a packed model.
        let Some((bike, src_dir)) = std::fs::read_dir(&src_bikes)
            .expect("read bikes")
            .flatten()
            .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
            .find(|(_, p)| {
                p.is_dir()
                    && crate::bikefiles::dir_has_mesh(p)
                    && p.with_extension("pkz").exists()
            })
        else {
            eprintln!("no bike with both a loose mesh and a .pkz found");
            return;
        };
        eprintln!("using real bike: {bike}");

        let root: PathBuf =
            std::env::temp_dir().join(format!("frost-preview-stock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mp = root.to_str().unwrap();
        let bikes = crate::library::mods_subdir(mp, "mods/bikes");
        copy_tree(&src_dir, &bikes.join(&bike));
        std::fs::copy(src_dir.with_extension("pkz"), bikes.join(format!("{bike}.pkz"))).unwrap();

        let set = super::modelswap::preview_set(mp, &bike, "Stock").expect("preview set");
        eprintln!("keeps {:?}", set.root_keep);
        assert!(
            !set.root_keep.iter().any(|f| crate::bikefiles::is_mesh(f)),
            "Stock must park every loose mesh",
        );
        let files = super::gather_preview_files(&set).expect("preview files");
        let m = super::build_bike_model(
            "stock preview",
            "stock-preview-test".into(),
            files,
            super::installed_paints(&set.bike_dir),
            std::time::Instant::now(),
        )
        .expect("stock preview builds");
        assert!(!m.nodes.is_empty(), "the packed model didn't come through");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn tex(name: &str, token: &str) -> crate::paint::PaintTexture {
        crate::paint::PaintTexture {
            name: name.into(),
            width: 4,
            height: 4,
            token: token.into(),
        }
    }

    /// Evicting a bike has to free every blob it put in the store.
    ///
    /// The one that gets away is a model texture every paint overrides: it is folded into
    /// none of them, so walking the paints alone never names it and its pixels outlive the
    /// bike by the whole session. Cheap to leak, since it's the biggest sheets — `plastics`
    /// on a bike whose paints all replace it — that leak.
    #[test]
    fn eviction_frees_the_models_own_textures_too() {
        let model = super::BikeModel {
            nodes: Vec::new(),
            paints: vec![super::BikePaint {
                name: "Red".into(),
                path: None,
                // Its own `plastics`, so the model's never reaches it.
                textures: vec![tex("plastics", "t-paint"), tex("wheel", "t-shared")],
                changes_preview: true,
            }],
            base: vec![tex("plastics", "t-own"), tex("wheel", "t-shared")],
            assembled: true,
        };
        let tokens = model.tokens();
        assert!(tokens.contains(&"t-own".to_string()), "the overridden one is still released");
        assert!(tokens.contains(&"t-paint".to_string()));
        // Named twice over — the paints borrowed it — and that's fine: `release` removes by
        // key, so the second pass over a token is a no-op rather than a double free.
        assert_eq!(tokens.iter().filter(|t| *t == "t-shared").count(), 2);
    }

    /// A paint installed loose beside a bike has to come back with the file it lives in —
    /// that path is the only thing the viewer can watch, and without it a re-saved livery
    /// goes unnoticed until the dialog is closed and re-opened.
    #[test]
    fn an_installed_paint_names_the_file_it_came_from() {
        let root = std::env::temp_dir().join(format!("frost-installed-paints-{}", std::process::id()));
        let paints = root.join("KTM 450").join("paints");
        std::fs::create_dir_all(&paints).expect("make the bike's paints folder");
        std::fs::write(paints.join("Frost.pnt"), b"not really a paint").expect("write a paint");
        // Whatever else is parked in there is not a paint and must not be offered as one.
        std::fs::write(paints.join("notes.txt"), b"x").expect("write the noise");

        let found = super::installed_paints(&root.join("KTM 450"));
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(found.len(), 1, "only the .pnt counts");
        let (name, path, bytes) = &found[0];
        assert_eq!(name, "Frost.pnt");
        assert_eq!(std::path::Path::new(path), paints.join("Frost.pnt"));
        assert_eq!(bytes, b"not really a paint");
    }

    /// A paint re-saved inside one timestamp tick still misses the cache.
    ///
    /// The viewer's live reload re-decodes a file the moment it changes, which can be twice
    /// in the same second — and a filesystem with coarse timestamps (FAT32 rounds to two)
    /// would hand back the previous decode, i.e. the painter's *last* attempt. The mtime is
    /// pinned here so the size is the only thing left to tell the two apart.
    #[test]
    fn a_paint_resaved_within_a_timestamp_tick_is_not_served_stale() {
        let dir = std::env::temp_dir().join(format!("frost-cache-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("make the folder");
        let paint = dir.join("Frost.pnt");
        let pinned = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let stamp = |bytes: &[u8]| {
            std::fs::write(&paint, bytes).expect("save the paint");
            let f = std::fs::File::options().write(true).open(&paint).expect("reopen");
            f.set_times(std::fs::FileTimes::new().set_modified(pinned))
                .expect("pin the mtime");
            super::bike_cache_key(&paint.to_string_lossy())
        };

        let first = stamp(b"the first attempt");
        let second = stamp(b"the second attempt, recompressed");
        let identical = stamp(b"the second attempt, recompressed");
        let _ = std::fs::remove_dir_all(&dir);

        assert_ne!(first, second, "a re-saved paint must miss its cached decode");
        assert_eq!(identical, second, "and an unchanged one must still hit it");
    }

    /// The same, reached through the `.pkz` beside the folder — how a packaged bike is
    /// installed, and the path a painter's own liveries actually sit next to.
    #[test]
    fn paints_are_found_beside_a_packaged_bike_too() {
        let root = std::env::temp_dir().join(format!("frost-packaged-paints-{}", std::process::id()));
        let paints = root.join("KTM 450").join("paints");
        std::fs::create_dir_all(&paints).expect("make the bike's paints folder");
        std::fs::write(paints.join("Frost.pnt"), b"paint").expect("write a paint");

        // The source the viewer is given is the archive, not the folder next to it.
        let found = super::installed_paints(&root.join("KTM 450.pkz"));
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(found.len(), 1);
        assert_eq!(std::path::Path::new(&found[0].1), paints.join("Frost.pnt"));
    }

    #[test]
    #[ignore]
    fn bike_model_from_pkz() {
        let Ok(path) = std::env::var("MXB_REAL_PKZ") else {
            eprintln!("set MXB_REAL_PKZ to run");
            return;
        };
        let m = super::load_bike_model_blocking(path).expect("load bike");
        for n in &m.nodes {
            // Where the part ended up. Printed because placement is the half of this that a
            // texture listing can't show: a part bound to the right sheet and hung in the
            // wrong place reads as correct here otherwise.
            let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
            for v in n.positions.chunks_exact(3) {
                for k in 0..3 {
                    lo[k] = lo[k].min(v[k]);
                    hi[k] = hi[k].max(v[k]);
                }
            }
            eprintln!(
                "node '{}' placed={} x[{:.3},{:.3}] y[{:.3},{:.3}] z[{:.3},{:.3}]",
                n.name, n.placed, lo[0], hi[0], lo[1], hi[1], lo[2], hi[2],
            );
            for s in &n.submeshes {
                eprintln!(
                    "   {:<16} -> {:<12} tile={:?}",
                    s.name,
                    s.texture.as_deref().unwrap_or("(none)"),
                    s.uv_tile
                );
            }
        }
        for p in &m.paints {
            let mut names: Vec<&str> = p.textures.iter().map(|t| t.name.as_str()).collect();
            names.sort_unstable();
            eprintln!(
                "paint '{}' changes_preview={}: {}",
                p.name,
                p.changes_preview,
                names.join(", ")
            );
        }
        let mut own: Vec<&str> = m.base.iter().map(|t| t.name.as_str()).collect();
        own.sort_unstable();
        eprintln!("the model's own textures: {}", own.join(", "));
        assert!(!m.nodes.is_empty(), "decoded the mesh");
        let have: std::collections::HashSet<String> = m.paints[0]
            .textures
            .iter()
            .map(|t| t.name.to_ascii_lowercase())
            .collect();
        for n in &m.nodes {
            for s in &n.submeshes {
                if let Some(t) = &s.texture {
                    assert!(have.contains(&t.to_ascii_lowercase()), "'{t}' is available");
                }
            }
        }
        // The Designer's stock underlay reads exactly this list, and it is wanted most where no
        // `.pnt` can answer: an OEM bike's stock paint replaces the wheels and the chain, so its
        // `plastics` is only ever embedded in the mesh. Not asserted by that name — a mod bike
        // calls its sheets whatever it likes, and a bike whose paints supply everything has a
        // shorter list than this one — only that the list survived being folded into the paints
        // above, which is the way it would be lost.
        assert!(!m.base.is_empty(), "the model's own textures outlive the fold into the paints");
    }


    #[test]
    #[ignore]
    fn gear_model_from_pkz() {
        let Ok(path) = std::env::var("MXB_REAL_GEAR") else {
            eprintln!("set MXB_REAL_GEAR to run");
            return;
        };
        let files = super::read_gear_files(std::path::Path::new(&path)).expect("read gear");
        let paints: Vec<String> = files
            .iter()
            .filter_map(|(n, _)| super::gear_folder_paint_name(n, "paints"))
            .collect();
        let goggles: Vec<String> = files
            .iter()
            .filter_map(|(n, _)| super::gear_folder_paint_name(n, "goggles"))
            .collect();
        eprintln!("paints ({}): {:?}", paints.len(), &paints[..paints.len().min(4)]);
        eprintln!("goggles ({}): {:?}", goggles.len(), &goggles[..goggles.len().min(4)]);

        // What the mesh itself draws each piece from — the reading the binder goes by, and
        // the one worth eyeballing when a helmet's goggles come out wearing the shell.
        if let Some((_, d)) = files.iter().find(|(n, _)| super::is_visible_gear_mesh(n)) {
            // The slots as the loader counts them: what the mesh embeds, plus the sheets it
            // leaves to a `.pnt`, in the order the model names them.
            let declared = super::paint_texture_names(
                files
                    .iter()
                    .filter(|(n, _)| {
                        super::gear_folder_paint_name(n, "paints").is_some()
                            || super::gear_folder_paint_name(n, "goggles").is_some()
                    })
                    .map(|(_, d)| d.as_slice()),
            );
            let colors = super::edf::declared_colors(d, &declared);
            eprintln!("mesh colour textures: {colors:?}");
            // Per piece, the texture the model was drawn against. This is the evidence the
            // binder decides on, so when a piece ends up wearing the wrong side it says
            // whether the reading was wrong or the choice made from it was.
            for n in &super::edf::parse(d) {
                eprintln!("mats    {:<28} {:?}", n.name, n.materials);
                for sm in &n.submeshes {
                    let emb = sm
                        .mat
                        .and_then(|m| n.materials.get(m as usize).copied().flatten())
                        .and_then(|slot| colors.get(slot))
                        .map(String::as_str);
                    // The triangle count tells the pieces apart when their names don't: a
                    // helmet shell dwarfs its lens, and both dwarf a tear-off film.
                    eprintln!(
                        "drawn   {:<28} mat={:?} tris={:<6} -> {}",
                        format!("{}/{}", n.name, sm.name),
                        sm.mat,
                        sm.tri_count,
                        emb.unwrap_or("(none)"),
                    );
                }
            }
        }
        // Where each mesh sits in its own frame, against the bounds the file states for
        // itself. Protection is authored in the rider's own space, so these say whether a
        // piece is a chest-wide vest or a thin chain — the difference a one-size fit erases —
        // and disagreeing with the header is how a placement that ran twice shows up.
        let mut scenes: Vec<&Vec<u8>> =
            super::gear_scenes(&files).iter().filter_map(|s| super::gear_file(&files, s)).collect();
        if scenes.is_empty() {
            scenes.extend(files.iter().find(|(n, _)| super::is_visible_gear_mesh(n)).map(|(_, d)| d));
        }
        for d in scenes {
            let mut nodes = super::edf::parse_gear(d);
            super::edf::to_right_handed(&mut nodes);
            let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
            for n in &nodes {
                let (mut nlo, mut nhi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
                for v in n.positions.chunks_exact(3) {
                    for k in 0..3 {
                        nlo[k] = nlo[k].min(v[k]);
                        nhi[k] = nhi[k].max(v[k]);
                    }
                }
                eprintln!(
                    "bounds  {:<16} x[{:.3},{:.3}] y[{:.3},{:.3}] z[{:.3},{:.3}]",
                    n.name, nlo[0], nhi[0], nlo[1], nhi[1], nlo[2], nhi[2],
                );
                for k in 0..3 {
                    lo[k] = lo[k].min(nlo[k]);
                    hi[k] = hi[k].max(nhi[k]);
                }
            }
            let Some((hlo, hhi)) = super::edf::header_aabb(d) else { continue };
            // The header is written in authored space, so it takes the same X flip the
            // vertices got — and the flip swaps which end is the minimum.
            let (hlo, hhi) = ([-hhi[0], hlo[1], hlo[2]], [-hlo[0], hhi[1], hhi[2]]);
            eprintln!(
                "header                   x[{:.3},{:.3}] y[{:.3},{:.3}] z[{:.3},{:.3}]",
                hlo[0], hhi[0], hlo[1], hhi[1], hlo[2], hhi[2],
            );
            // Only the highest LOD is kept and a dropped one can reach a millimetre further,
            // so this is a "same place, same size" check, not an equality.
            for k in 0..3 {
                let slack = 0.02 + 0.1 * (hhi[k] - hlo[k]);
                assert!(
                    (lo[k] - hlo[k]).abs() <= slack && (hi[k] - hhi[k]).abs() <= slack,
                    "axis {k}: mesh [{:.3},{:.3}] is not where the file says it is \
                     ([{:.3},{:.3}]) — placement",
                    lo[k], hi[k], hlo[k], hhi[k],
                );
            }
        }
        // The names each side's first paint supplies — where each can actually land.
        let supplied = |folder: &str| -> std::collections::HashSet<String> {
            files
                .iter()
                .find(|(n, _)| super::gear_folder_paint_name(n, folder).is_some())
                .and_then(|(_, d)| super::paint::decode_any(d).ok())
                .map(|p| p.iter().map(|t| t.name.to_ascii_lowercase()).collect())
                .unwrap_or_default()
        };

        // The slot this item is worn in. Gear behaves differently per slot — protection has
        // no goggle side, and is the slot where a paintless mod is the norm rather than the
        // exception — so the run is worth naming honestly.
        let slot = std::env::var("MXB_REAL_GEAR_PART").unwrap_or_else(|_| "helmet".into());
        let part = super::load_gear_model_blocking(path.clone(), slot.clone(), None, None, false, false, Vec::new())
            .expect("load gear");
        // MXB_DUMP_OBJ=<file> writes the loaded geometry in viewer-input space, so a
        // silhouette can be drawn outside the test — the only way to settle which way a
        // gear frame points without the game in front of you.
        if let Ok(out) = std::env::var("MXB_DUMP_OBJ") {
            let mut s = String::new();
            for n in &part.nodes {
                s.push_str(&format!("o {}\n", n.name));
                for v in n.positions.chunks_exact(3) {
                    s.push_str(&format!("v {} {} {}\n", v[0], v[1], v[2]));
                }
            }
            std::fs::write(&out, s).expect("write obj");
            eprintln!("wrote {out}");
        }
        let have: std::collections::HashSet<String> =
            part.textures.iter().map(|t| t.name.to_ascii_lowercase()).collect();
        // An item with no paint and nothing baked into its mesh has no look to wear — the
        // Minecraft pickaxe on mxb-mods ships exactly that. Bare grey is the honest answer
        // there, and the only case where an unbound piece isn't a bug.
        let has_look = !paints.is_empty() || !have.is_empty();
        // Which texture each piece ended up wearing, by node so a goggle node that carries
        // no submeshes of its own shows up too.
        let mut worn: Vec<(String, String)> = Vec::new();
        let mut bare = 0usize;
        for n in &part.nodes {
            let pieces: Vec<(String, &Option<String>)> = if n.submeshes.is_empty() {
                vec![(n.name.clone(), &n.texture)]
            } else {
                n.submeshes
                    .iter()
                    .map(|s| (format!("{}/{}", n.name, s.name), &s.texture))
                    .collect()
            };
            for (label, tex) in pieces {
                match tex {
                    Some(t) => {
                        eprintln!("worn    {label:<28} -> {t}");
                        worn.push((label, t.clone()));
                    }
                    None => {
                        eprintln!("worn    {label:<28} -> (bare)");
                        bare += 1;
                    }
                }
            }
        }
        assert!(
            !has_look || bare == 0,
            "{bare} piece(s) left bare though the item ships a look",
        );
        for (_, t) in &worn {
            assert!(have.contains(&t.to_ascii_lowercase()), "'{t}' is shipped");
        }
        let (shell_names, goggle_names) = (supplied("paints"), supplied("goggles"));
        eprintln!("shell paint supplies {shell_names:?}, goggle paint {goggle_names:?}");
        // A helmet that ships goggle paints has somewhere for them to land — asked by name,
        // not by what a piece is called, since that's exactly what a mesh needn't spell out.
        // Skipped where the two sides share an atlas: then "whose texture is this" has no
        // answer to check.
        if !goggle_names.is_empty() && goggle_names.is_disjoint(&shell_names) {
            assert!(
                worn.iter().any(|(_, t)| goggle_names.contains(&t.to_ascii_lowercase())),
                "a piece wears the goggle paint: {worn:?}",
            );
            assert!(
                worn.iter().any(|(_, t)| !goggle_names.contains(&t.to_ascii_lowercase())),
                "the shell keeps its own texture: {worn:?}",
            );
        }

        // Stock: the same mesh wearing the textures embedded in it, not a packed `.pnt`.
        let listed = super::gear_paints_at(std::path::Path::new(&path)).expect("list paints");
        eprintln!("has_stock={} goggles={}", listed.has_stock, listed.has_stock_goggles);
        if !listed.has_stock {
            eprintln!("this piece embeds no textures — no stock entry to check");
            return;
        }
        let stock =
            super::load_gear_model_blocking(
                path,
                slot,
                None,
                None,
                true,
                listed.has_stock_goggles,
                Vec::new(),
            )
            .expect("load stock gear");
        let embedded: std::collections::HashSet<String> =
            stock.textures.iter().map(|t| t.name.to_ascii_lowercase()).collect();
        assert!(
            !paints.iter().any(|p| embedded.contains(&p.to_ascii_lowercase())),
            "a stock preview decodes no packed paint",
        );
        let mut bound = 0;
        for n in &stock.nodes {
            // A one-piece item has no submesh table at all and wears its texture on the node
            // — `bind_gear_submeshes` says so explicitly. Counting only submeshes read that
            // as "nothing was bound" on exactly the meshes worth checking.
            let pieces: Vec<(&str, Option<&String>)> = if n.submeshes.is_empty() {
                vec![(n.name.as_str(), n.texture.as_ref())]
            } else {
                n.submeshes.iter().map(|s| (s.name.as_str(), s.texture.as_ref())).collect()
            };
            for (name, tex) in pieces {
                let t = tex.expect("stock piece bound to a texture");
                eprintln!("stock piece {name:<10} -> {t}");
                assert!(embedded.contains(&t.to_ascii_lowercase()), "'{t}' is embedded in the mesh");
                bound += 1;
            }
        }
        assert!(bound > 0, "stock bound at least one piece");

        // Mixed: stock shell, painted goggles. A paint reuses the mesh's texture names, so
        // this is where the two sets would collide and the viewer would pick at random.
        if let Some(g) = goggles.first() {
            let mixed = super::load_gear_model_blocking(
                std::env::var("MXB_REAL_GEAR").unwrap(),
                "helmet".into(),
                None,
                Some(g.clone()),
                true,
                false,
                Vec::new(),
            )
            .expect("load mixed gear");
            let mut names: Vec<String> =
                mixed.textures.iter().map(|t| t.name.to_ascii_lowercase()).collect();
            names.sort_unstable();
            let total = names.len();
            names.dedup();
            assert_eq!(names.len(), total, "one texture per name, so binding is unambiguous");
            for n in &mixed.nodes {
                for s in &n.submeshes {
                    let t = s.texture.as_ref().expect("mixed submesh bound to a texture");
                    assert!(names.contains(&t.to_ascii_lowercase()), "'{t}' is available");
                }
            }
        }
    }

    /// The picker's paint list against a real install: whatever any single source can
    /// supply for a model, the merged list offers.
    ///
    /// `MXB_MODS=~/Documents/PiBoSo/MX\ Bikes MXB_GEAR_PART=boots \
    ///   MXB_GEAR_MODEL='Fox Instinct 2.0 by Aeffertz' \
    ///   cargo test gear_paints_merge_every_source -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gear_paints_merge_every_source() {
        let Ok(mods) = std::env::var("MXB_MODS") else {
            eprintln!("set MXB_MODS to run");
            return;
        };
        let part = std::env::var("MXB_GEAR_PART").unwrap_or_else(|_| "boots".into());
        let Ok(model) = std::env::var("MXB_GEAR_MODEL") else {
            eprintln!("set MXB_GEAR_MODEL to run");
            return;
        };
        let cfg = crate::config::AppConfig { mods_path: mods.clone(), ..Default::default() };
        let spec = super::GEAR.iter().find(|g| g.part == part).expect("a gear slot");

        let merged = super::gear_paints_for(&cfg, spec, &model);
        eprintln!("merged paints ({}): {:?}", merged.paints.len(), merged.paints);
        eprintln!("merged goggles ({}): {:?}", merged.goggles.len(), merged.goggles);

        let has = |set: &[String], want: &str| set.iter().any(|n| n.eq_ignore_ascii_case(want));

        // Every source, asked on its own. Each one's paints have to survive the merge —
        // taking the first source is exactly what dropped the others.
        let rider = std::path::Path::new(&mods).join("mods").join("rider");
        let stem = model.trim_end_matches(".pkz");
        let mut sources = 0;
        for src in super::gear_sources(&rider, spec, stem) {
            if !src.exists() {
                continue;
            }
            sources += 1;
            let alone = super::gear_paints_at(&src).expect("list one source");
            eprintln!("  {src:?}: {:?}", alone.paints);
            for p in &alone.paints {
                assert!(has(&merged.paints, p), "'{p}' from {src:?} survives the merge");
            }
            for g in &alone.goggles {
                assert!(has(&merged.goggles, g), "goggle '{g}' from {src:?} survives the merge");
            }
        }

        // The game's own copy, which is all a stock name has and which nothing listed before.
        if let Some(pkz) = super::resolve_game_pkz(&cfg, "rider.pkz") {
            let folder = format!("rider/{}/{}", spec.pkz_kind, stem);
            let stock = super::pkz_paint_names(&pkz, &folder, "paints");
            eprintln!("  rider.pkz {folder}: {stock:?}");
            if !stock.is_empty() {
                sources += 1;
            }
            for p in &stock {
                assert!(has(&merged.paints, p), "stock '{p}' survives the merge");
            }
        }
        assert!(sources > 0, "'{model}' resolved to at least one source");
        // Nothing is offered twice — a paint pack installed beside the mod it was made for
        // ships the same names, and the same look twice is not two choices.
        let mut seen: Vec<String> = merged.paints.iter().map(|s| s.to_lowercase()).collect();
        let total = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), total, "no paint is offered twice");
    }

    #[test]
    fn body_slot_reads_the_name_not_the_index() {
        // The `w_` planes are decals the game composites a name and number onto.
        for decal in ["w_name", "w_number", "w_plate", "W_Name"] {
            assert_eq!(super::body_slot(Some(decal)), "hide");
        }
        // Skin, whatever the model calls it, must not wear the kit.
        for skin in ["face_parts", "face", "Face_Parts", "rider_face"] {
            assert_eq!(super::body_slot(Some(skin)), "face");
        }
        // Everything else keeps its own name, so a paint replaces it by name and a piece no
        // paint covers falls back to the model's own texture.
        for (name, slot) in [
            ("rider", "rider"),
            ("gloves", "gloves"),
            ("rider_sm", "rider_sm"),
            ("glovessm", "glovessm"),
            ("Braces", "braces"),
        ] {
            assert_eq!(super::body_slot(Some(name)), slot);
        }
        // A material that names no texture still has to render as something.
        assert_eq!(super::body_slot(None), "rider");
    }

    /// A material id is LOCAL to the node that owns it, so the same id must resolve to
    /// different textures in different parts of one mesh.
    ///
    /// This is the invariant the per-part material-table fix established, and the one the
    /// rider binder lost: it was still resolving ids through a whole-model reading, which
    /// is what put one part's texture on another's geometry.
    #[test]
    fn a_body_part_resolves_its_material_through_its_own_table() {
        fn tex(name: &str) -> super::edf::EmbeddedTexture {
            super::edf::EmbeddedTexture {
                name: name.into(),
                width: 4,
                height: 4,
                data_off: 0,
                data_len: 0,
            }
        }
        fn sm(mat: u32) -> super::edf::Submesh {
            super::edf::Submesh {
                name: "range".into(),
                tri_start: 0,
                tri_count: 1,
                texture: None,
                uv_tile: None,
                mat: Some(mat),
            }
        }
        fn node(materials: Vec<Option<usize>>, mats: &[u32]) -> super::edf::EdfNode {
            super::edf::EdfNode {
                name: "part".into(),
                positions: Vec::new(),
                uvs: Vec::new(),
                normals: Vec::new(),
                indices: Vec::new(),
                submeshes: mats.iter().map(|m| sm(*m)).collect(),
                texture: None,
                placed: false,
                materials,
            }
        }

        let colors = [tex("rider"), tex("gloves"), tex("w_number")];
        // Both parts draw on local id 0 — and mean different textures by it.
        let mut nodes = vec![
            node(vec![Some(0)], &[0]),           // body  -> rider
            node(vec![Some(1)], &[0]),           // hands -> gloves
            node(vec![Some(2), None], &[0, 1]),  // plate -> hidden decal, then untextured
        ];
        super::bind_body_to_colors(&mut nodes, &colors);

        assert_eq!(nodes[0].submeshes[0].texture.as_deref(), Some("rider"));
        assert_eq!(
            nodes[1].submeshes[0].texture.as_deref(),
            Some("gloves"),
            "id 0 read through the first node's table would smear the suit onto the hands"
        );
        assert_eq!(nodes[2].submeshes[0].texture.as_deref(), Some("hide"));
        // An untextured material, and an id past the end of the table, both still render.
        assert_eq!(nodes[2].submeshes[1].texture.as_deref(), Some("rider"));
    }

    /// The bug this replaced: material indices count into the model's own texture list, and
    /// no two rider models write that list in the same order.
    ///
    /// `MXB_REAL_BODY=<rider.edf>[,<rider.edf>…] cargo test rider_body_binding -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn rider_body_binding_from_env() {
        let Ok(paths) = std::env::var("MXB_REAL_BODY") else {
            eprintln!("set MXB_REAL_BODY to run");
            return;
        };
        for path in paths.split(',').filter(|p| !p.is_empty()) {
            let bytes = std::fs::read(path).expect("read rider.edf");
            let order: Vec<String> =
                crate::edf::color_textures(&bytes).iter().map(|t| t.name.clone()).collect();
            let mut nodes = crate::edf::parse(&bytes);
            crate::edf::to_right_handed(&mut nodes);
            super::keep_lod0(&mut nodes);
            super::bind_body_submeshes(&mut nodes, &bytes);

            super::stand_body_upright(&mut nodes);

            let bounds = |ns: &[crate::edf::EdfNode], only_face: bool| {
                let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                for n in ns {
                    for sm in &n.submeshes {
                        if only_face && sm.texture.as_deref() != Some("face") {
                            continue;
                        }
                        for i in &n.indices[sm.tri_start as usize * 3
                            ..(sm.tri_start + sm.tri_count) as usize * 3]
                        {
                            let v = &n.positions[*i as usize * 3..*i as usize * 3 + 3];
                            for a in 0..3 {
                                lo[a] = lo[a].min(v[a]);
                                hi[a] = hi[a].max(v[a]);
                            }
                        }
                    }
                }
                (lo, hi)
            };
            let (lo, hi) = bounds(&nodes, false);
            let (h, d) = (hi[1] - lo[1], hi[2] - lo[2]);
            eprintln!("  upright bounds x={:.3} y={h:.3} z={d:.3}", hi[0] - lo[0]);

            // A rider is a standing figure, so height is its longest axis. The viewer scales
            // and anchors every piece of gear off this — a body on its side buries the
            // helmet and boots in the torso at a fifth of their size.
            assert!(h > hi[0] - lo[0] && h > d, "the body stands up");
            // And it stands the right way up. Height alone can't tell a rider from one
            // hanging upside down, so check the skin: the head is the highest thing on a
            // rider. Its top, not its bottom — Rider+'s skin texture also covers the bare
            // wrists of its rolled-sleeve variants, which reach well down the body.
            let (_, fhi) = bounds(&nodes, true);
            eprintln!("  skin tops out at {:.3} of {:.3}", fhi[1], hi[1]);

            // Which way does it face? Report the Z centroid of each slot relative to the
            // body's own centre, plus the head alone (the top eighth of the skin, so
            // Rider+'s bare wrists don't drag the number toward the bars).
            let cz = (lo[2] + hi[2]) / 2.0;
            let mut per: std::collections::BTreeMap<String, (f64, f64, usize)> = Default::default();
            for n in &nodes {
                for sm in &n.submeshes {
                    let slot = sm.texture.clone().unwrap_or_default();
                    for i in &n.indices
                        [sm.tri_start as usize * 3..(sm.tri_start + sm.tri_count) as usize * 3]
                    {
                        let v = &n.positions[*i as usize * 3..*i as usize * 3 + 3];
                        let e = per.entry(slot.clone()).or_default();
                        e.0 += (v[2] - cz) as f64;
                        e.2 += 1;
                        if slot == "face" && v[1] > hi[1] - 0.125 * h {
                            let hd = per.entry("face(head)".into()).or_default();
                            hd.0 += (v[2] - cz) as f64;
                            hd.2 += 1;
                        }
                    }
                }
            }
            for (slot, (sum, _, n)) in &per {
                eprintln!("  {slot:>12} z-centroid {:+.4} ({n} verts)", sum / *n as f64);
            }
            let centroid = |slot: &str| per.get(slot).map(|(s, _, n)| s / *n as f64);

            // And it faces the right way. The viewer nudges the helmet and boots forward in
            // +Z, so a rider turned around wears its gear through its own back.
            //
            // The name and number planes are the tell: they go on a rider's back. Where the
            // model has none, fall back to the head, which leans forward over the bars —
            // a weaker signal, so it only decides when the strong one is absent.
            match centroid("hide") {
                Some(back) => assert!(back < 0.0, "the name and number sit on the back ({back:+.4})"),
                None => {
                    let head = centroid("face(head)").expect("a rider has a head");
                    assert!(head > 0.0, "the head leans forward ({head:+.4})");
                }
            }
            assert!(
                fhi[1] > lo[1] + 0.9 * h,
                "the head is at the top (skin tops at {:.3}, body {:.3}..{:.3})",
                fhi[1],
                lo[1],
                hi[1],
            );

            let mut slots: Vec<String> = nodes
                .iter()
                .flat_map(|n| n.submeshes.iter().filter_map(|s| s.texture.clone()))
                .collect();
            slots.sort_unstable();
            slots.dedup();
            eprintln!("{path}\n  blob order: {order:?}\n  bound slots: {slots:?}");

            assert!(!slots.is_empty(), "every body binds its submeshes to something");
            // Whatever the model calls its suit and gloves, the binding must name the
            // textures the model actually carries — never a slot borrowed from another model.
            for s in &slots {
                assert!(
                    s == "hide"
                        || s == "face"
                        || order.iter().any(|t| t.eq_ignore_ascii_case(s)),
                    "'{s}' is a texture this model carries",
                );
            }
            // Skin is its own slot on every rider model shipped so far; catching its loss
            // is what tells us a mesh stopped being read and an index map crept back in.
            assert!(slots.iter().any(|s| s == "face"), "the face binds to bare skin");
        }
    }

    /// The whole rider-body path against a real install: a custom model resolves out of
    /// `mods/rider/riders`, binds, and wears a kit that only the stock profile owns.
    ///
    /// `MXB_MODS=~/Documents/PiBoSo/MX\ Bikes MXB_PROFILE=Rider+ cargo test rider_body_end_to_end -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn rider_body_end_to_end() {
        let Ok(mods) = std::env::var("MXB_MODS") else {
            eprintln!("set MXB_MODS to run");
            return;
        };
        let profile = std::env::var("MXB_PROFILE").unwrap_or_else(|_| "Rider+".into());
        let cfg = crate::config::AppConfig { mods_path: mods.clone(), ..Default::default() };
        let base = std::path::Path::new(&mods).join("mods").join("rider");

        // A model nobody can pick is a model nobody can wear. The scan reports what's on
        // disk; the two stock riders live in `rider.pkz` and the picker adds them itself.
        let targets = crate::library::scan_rider_targets(&mods);
        eprintln!("profiles: {:?}", targets.profiles);
        assert!(
            targets.profiles.iter().any(|p| *p == profile)
                || super::STOCK_RIDER_PROFILES.contains(&profile.as_str()),
            "'{profile}' is offered",
        );

        let src = super::rider_body_source(&cfg, &profile).expect("a body source");
        eprintln!("{profile}: {src:?}");

        {
            let t = std::time::Instant::now();
            let data = src.read(&profile).expect("read mesh");
            eprintln!("  read {} MB in {:?}", data.len() / 1_000_000, t.elapsed());
            let t = std::time::Instant::now();
            let mut n = crate::edf::parse(&data);
            eprintln!("  parse {} nodes in {:?}", n.len(), t.elapsed());
            let t = std::time::Instant::now();
            crate::edf::to_right_handed(&mut n);
            super::keep_lod0(&mut n);
            eprintln!("  handedness + lod0 in {:?}", t.elapsed());
            let t = std::time::Instant::now();
            super::bind_body_submeshes(&mut n, &data);
            eprintln!("  bind in {:?}", t.elapsed());
            let t = std::time::Instant::now();
            super::stand_body_upright(&mut n);
            eprintln!("  stand in {:?}", t.elapsed());
            let t = std::time::Instant::now();
            let texs = super::body_textures(&src, &profile).expect("textures");
            eprintln!(
                "  extract {:?} in {:?}",
                texs.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
                t.elapsed(),
            );
        }
        // A profile folder that carries a mesh is a model, and beats the game archive. One
        // that only carries paints — which is what installing a kit under `default_mx`
        // leaves behind — is not, and must still fall through to the stock body.
        let installed = base.join("riders").join(&profile).join("rider.edf").is_file();
        assert_eq!(
            matches!(src, super::BodySource::Loose(_)),
            installed,
            "an installed mesh wins, a paints-only folder doesn't",
        );

        let part = super::load_rider_body(&cfg, &profile, Vec::new()).expect("a body part");
        let slots: std::collections::BTreeSet<&str> = part
            .nodes
            .iter()
            .flat_map(|n| n.submeshes.iter().filter_map(|s| s.texture.as_deref()))
            .collect();
        let texs: std::collections::BTreeSet<String> =
            part.textures.iter().map(|t| t.name.to_ascii_lowercase()).collect();
        eprintln!("  slots={slots:?}\n  textures={texs:?}");
        // With no paint chosen, every slot that draws something is dressed by the model.
        for s in slots.iter().filter(|s| **s != "hide" && **s != "face") {
            assert!(texs.contains(*s), "'{s}' is supplied by the mesh when no paint is");
        }

        // Rider+ ships `paints` empty on purpose — the kit still has to resolve.
        if let Some(kit) = std::env::var("MXB_KIT").ok().filter(|k| !k.is_empty()) {
            let found = super::read_rider_paint_file(&cfg, &base, &profile, "paints", &kit);
            assert!(found.is_some(), "kit '{kit}' resolves for '{profile}'");
            eprintln!("  kit '{kit}' resolved ({} bytes)", found.unwrap().len());
        }
    }

    #[test]
    #[ignore]
    fn lod0_dedup_from_env() {
        let Ok(path) = std::env::var("MXB_REAL_EDF") else {
            eprintln!("set MXB_REAL_EDF to run");
            return;
        };
        let bytes = std::fs::read(&path).expect("read edf");
        let mut nodes = crate::edf::parse(&bytes);
        let before = nodes.len();
        super::keep_lod0(&mut nodes);
        for n in &nodes {
            eprintln!("kept node '{}' tris={}", n.name, n.indices.len() / 3);
        }
        let mut names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "no duplicate node names survive");
        eprintln!("{before} nodes -> {} after LOD dedup", nodes.len());
    }

    /// Write a bike's raw `.edf` meshes out so the binary layout can be studied directly.
    ///
    /// `MXB_REAL_PKZ=<bike.pkz> MXB_EDF_OUT=<dir> \
    ///   cargo test extract_bike_edfs -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn extract_bike_edfs() {
        let (Ok(path), Ok(out)) = (std::env::var("MXB_REAL_PKZ"), std::env::var("MXB_EDF_OUT"))
        else {
            eprintln!("set MXB_REAL_PKZ and MXB_EDF_OUT to run");
            return;
        };
        let stem = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        std::fs::create_dir_all(&out).expect("create out dir");
        let files = super::gather_bike_files(std::path::Path::new(&path)).expect("gather");
        for (name, data) in &files {
            let bn = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
            if !bn.ends_with(".edf") {
                continue;
            }
            let dst = std::path::Path::new(&out).join(format!("{stem}__{bn}"));
            std::fs::write(&dst, data).expect("write edf");
            println!("wrote {} ({} bytes)", dst.display(), data.len());
        }
    }

    /// Dump, as one JSON object, everything that decides which texture each part of a
    /// bike wears: the mesh's colour list, what each reading of a material index claims,
    /// which reading the geometry settled on, and what the viewer finally bound.
    ///
    /// `MXB_REAL_PKZ='…/mods/bikes/MX2OEM_2023_Kawasaki_KX250.pkz' \
    ///   cargo test audit_bike_bindings -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn audit_bike_bindings() {
        let Ok(path) = std::env::var("MXB_REAL_PKZ") else {
            eprintln!("set MXB_REAL_PKZ to run");
            return;
        };
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let bike = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        // What the viewer renders today.
        let model = super::load_bike_model_blocking(path.clone()).expect("load bike");
        // Keyed on the triangle range too: one group name can appear twice in a node,
        // once per material, and those two are exactly the interesting case.
        let bound: std::collections::HashMap<(String, String, u32), Option<String>> = model
            .nodes
            .iter()
            .flat_map(|n| {
                n.submeshes.iter().map(move |s| {
                    (
                        (n.name.to_ascii_lowercase(), s.name.to_ascii_lowercase(), s.tri_start),
                        s.texture.clone(),
                    )
                })
            })
            .collect();

        // The same meshes again, raw, so both readings of every material can be shown
        // side by side with the fit that chose between them.
        let files = super::gather_bike_files(std::path::Path::new(&path)).expect("gather");
        let mut out = String::new();
        out.push_str(&format!("{{\"bike\":\"{}\",\"meshes\":[", esc(&bike)));
        let mut first_mesh = true;
        for (name, data) in &files {
            let bn = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
            if !bn.ends_with(".edf") {
                continue;
            }
            let nodes = crate::edf::parse_with_levels(data, &[]);
            if nodes.is_empty() {
                continue;
            }
            let color = crate::edf::color_textures(data);
            if color.is_empty() {
                continue; // a shadow mesh carries no colour textures and is never rendered
            }
            let colors: Vec<String> =
                color.iter().map(|t| format!("\"{}\"", esc(&t.name))).collect();
            if !first_mesh {
                out.push(',');
            }
            first_mesh = false;
            out.push_str(&format!(
                "{{\"file\":\"{}\",\"colors\":[{}],\"nodes\":[",
                esc(&bn),
                colors.join(",")
            ));
            for (i, n) in nodes.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // The node's OWN material table: local id -> colour texture.
                let table: Vec<String> = n
                    .materials
                    .iter()
                    .map(|slot| match slot.and_then(|s| color.get(s)) {
                        Some(t) => format!("\"{}\"", esc(&t.name)),
                        None => "null".into(),
                    })
                    .collect();
                out.push_str(&format!(
                    "{{\"node\":\"{}\",\"materials\":[{}],\"submeshes\":[",
                    esc(&n.name),
                    table.join(",")
                ));
                for (j, s) in n.submeshes.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    let key =
                        (n.name.to_ascii_lowercase(), s.name.to_ascii_lowercase(), s.tri_start);
                    let rendered = bound
                        .get(&key)
                        .cloned()
                        .flatten()
                        .map(|t| format!("\"{}\"", esc(&t)))
                        .unwrap_or_else(|| "null".into());
                    out.push_str(&format!(
                        "{{\"group\":\"{}\",\"mat\":{},\"tris\":{},\"rendered\":{}}}",
                        esc(&s.name),
                        s.mat.map(|m| m.to_string()).unwrap_or_else(|| "null".into()),
                        s.tri_count,
                        rendered,
                    ));
                }
                out.push_str("]}");
            }
            out.push_str("]}");
        }
        let paints: Vec<String> = model
            .paints
            .iter()
            .map(|p| {
                let mut t: Vec<String> =
                    p.textures.iter().map(|t| format!("\"{}\"", esc(&t.name))).collect();
                t.sort_unstable();
                format!("{{\"paint\":\"{}\",\"textures\":[{}]}}", esc(&p.name), t.join(","))
            })
            .collect();
        out.push_str(&format!("],\"paints\":[{}]}}", paints.join(",")));
        println!("AUDIT {out}");
    }

    /// The average colour of a stored texture, and how far off neutral it is.
    ///
    /// Neutrality rather than "is it white" on purpose: it holds whichever way round the
    /// channels are stored, so this can't pass or fail for the wrong reason if that ever
    /// moves. A white or grey sheet has its three channels together; a painted one doesn't.
    fn avg_and_spread(tex: &crate::paint::PaintTexture) -> ([u32; 3], u32) {
        let px = crate::texstore::get(&tex.token).expect("token resolves");
        let mut sum = [0u64; 3];
        let mut n = 0u64;
        for p in px.chunks_exact(4) {
            if p[3] < 128 {
                continue; // fully transparent regions are not the look
            }
            for k in 0..3 {
                sum[k] += p[k] as u64;
            }
            n += 1;
        }
        assert!(n > 0, "'{}' has no opaque pixels", tex.name);
        let avg = [(sum[0] / n) as u32, (sum[1] / n) as u32, (sum[2] / n) as u32];
        (avg, avg.iter().max().unwrap() - avg.iter().min().unwrap())
    }

    /// The reported bug, pinned to the archive it came from: the rider tab drew the stock
    /// helmet bronze while the library's "Stock" entry drew it white.
    ///
    /// Both halves matter and they pull in opposite directions — an empty name must reach no
    /// paint at all, while a *stale* name must still reach one, or gear whose paint was
    /// renamed comes out bare grey. A single assertion would let the fix overshoot.
    #[test]
    #[ignore]
    fn an_unnamed_stock_paint_is_the_mesh_not_the_first_pnt() {
        let Ok(pkz) = std::env::var("MXB_RIDER_PKZ") else {
            eprintln!("set MXB_RIDER_PKZ to the game's rider.pkz to run");
            return;
        };
        let pkz = std::path::Path::new(&pkz);
        let folder = "rider/helmets/default";

        // No name → no paint, so the caller falls through to the mesh's own textures.
        assert!(
            super::load_pkz_paint(pkz, folder, "paints", "").is_empty(),
            "an empty slot must not resolve to a paint",
        );
        // A name that misses → still a paint, so a renamed livery shows textured.
        assert!(
            !super::load_pkz_paint(pkz, folder, "paints", "no_such_paint_ea7b").is_empty(),
            "a stale name must still fall back to a paint",
        );

        // And what the two answers actually look like. The mesh's own sheet is the white one
        // the library shows; the paint that used to stand in for it is not.
        let mesh = super::read_pkz_entry(pkz, &format!("{folder}/helmet.edf")).expect("helmet.edf");
        let stock = crate::paint::extract_edf_textures_where(&mesh, |n| n.eq_ignore_ascii_case("helmet"));
        let stock = stock.first().expect("the mesh carries its own 'helmet' sheet");
        let (stock_avg, stock_spread) = avg_and_spread(stock);

        let first = super::load_pkz_paint(pkz, folder, "paints", "black_yellow");
        let first = first.iter().find(|t| t.name.eq_ignore_ascii_case("helmet"));
        let first = first.expect("black_yellow paints 'helmet'");
        let (first_avg, first_spread) = avg_and_spread(first);

        eprintln!("stock mesh sheet  avg={stock_avg:?} spread={stock_spread}");
        eprintln!("black_yellow.pnt  avg={first_avg:?} spread={first_spread}");
        assert!(stock_spread < 12, "the stock helmet is neutral (white/grey), got {stock_avg:?}");
        assert!(
            first_spread > 40,
            "black_yellow is a colour, not a neutral — if this fails the fixture archive \
             changed and the test above proves less than it looks like ({first_avg:?})",
        );
    }

    /// The invariant behind the fix, for whatever gear is on this machine: an empty paint
    /// slot renders exactly what the library's "Stock" entry renders — and where there is no
    /// stock look to render, it still renders *something* rather than going bare.
    ///
    /// Stated as an equality between the two code paths rather than as an expected texture
    /// name, because the point is that they agree, not what they agree on.
    #[test]
    #[ignore]
    fn an_empty_slot_renders_what_the_library_calls_stock() {
        let Ok(path) = std::env::var("MXB_REAL_GEAR") else {
            eprintln!("set MXB_REAL_GEAR to an installed gear folder/.pkz to run");
            return;
        };
        let slot = std::env::var("MXB_REAL_GEAR_PART").unwrap_or_else(|_| "helmet".into());
        let names = |p: super::RiderPart| {
            let mut v: Vec<String> = p.textures.iter().map(|t| t.name.to_ascii_lowercase()).collect();
            v.sort_unstable();
            v
        };
        let load = |paint: Option<&str>, stock: bool| {
            names(
                super::load_gear_model_blocking(
                    path.clone(),
                    slot.clone(),
                    paint.map(str::to_string),
                    None,
                    stock,
                    false,
                    Vec::new(),
                )
                .expect("load gear"),
            )
        };

        let offers_stock = super::gear_paints_at(std::path::Path::new(&path))
            .expect("read gear paints")
            .has_stock;
        // The rider tab's empty slot, and the library's picker on "Stock".
        let empty = load(Some(""), false);
        eprintln!("has_stock={offers_stock} empty slot -> {empty:?}");
        assert!(!empty.is_empty(), "an empty slot must never render untextured");
        if offers_stock {
            assert_eq!(empty, load(None, true), "empty slot != the library's Stock");
        } else {
            // No stock look to fall back on, so the first-paint fallback still stands —
            // forcing stock here is what would draw the Bell Moto 10 in a near-blank film.
            assert_eq!(empty, load(None, false), "kept the first-paint fallback");
        }
    }

    /// The whole round trip for a package a shop install buried: unloadable where it sits,
    /// loadable once the repair has raised it. Run against a real locked helmet, because the
    /// point is that the file coming up is the same file the viewer then decodes.
    ///
    /// `MXB_REAL_GEAR` must name a `.pkz`. Linked rather than copied, so the player's own
    /// 42 MB helmet is neither duplicated nor moved — the repair renames the link.
    #[test]
    #[ignore]
    fn a_buried_package_loads_once_the_repair_has_raised_it() {
        let Ok(path) = std::env::var("MXB_REAL_GEAR") else {
            eprintln!("set MXB_REAL_GEAR to an installed gear .pkz to run");
            return;
        };
        let src = std::path::Path::new(&path);
        assert!(src.is_file(), "MXB_REAL_GEAR must be a .pkz file for this one");
        let name = src.file_name().unwrap().to_string_lossy().into_owned();

        let root = std::env::temp_dir().join(format!("frost-buried-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let helmets = root.join("mods/rider/helmets");
        let buried = helmets.join("shop-44");
        std::fs::create_dir_all(&buried).unwrap();
        std::fs::hard_link(src, buried.join(&name))
            .or_else(|_| std::fs::copy(src, buried.join(&name)).map(|_| ()))
            .expect("stage the package");

        // Where the install put it: the folder is what the picker offers, and it has no mesh.
        let buried_load = super::load_gear_model_blocking(
            buried.to_string_lossy().into_owned(),
            "helmet".into(),
            None,
            None,
            false,
            false,
            Vec::new(),
        );
        let err = buried_load.err().expect("a buried package must not load");
        assert!(err.contains("no gear mesh found"), "unexpected error: {err}");

        let plans = crate::gearrepair::plan(root.to_str().unwrap());
        assert_eq!(plans.len(), 1, "the burial is found");
        let moved = crate::gearrepair::apply_one(root.to_str().unwrap(), &plans[0].id).unwrap();
        assert_eq!(moved, 1);

        let raised = helmets.join(&name);
        assert!(raised.is_file(), "the package is in the area root");
        let part = super::load_gear_model_blocking(
            raised.to_string_lossy().into_owned(),
            "helmet".into(),
            None,
            None,
            false,
            false,
            Vec::new(),
        )
        .expect("load the raised package");
        eprintln!(
            "{name} -> {} node(s), textures {:?}",
            part.nodes.len(),
            part.textures.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert!(!part.nodes.is_empty(), "it draws something");
        assert!(!part.textures.is_empty(), "and it is textured");
        let _ = std::fs::remove_dir_all(&root);
    }
}
