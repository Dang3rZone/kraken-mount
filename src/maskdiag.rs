//! TEST ONLY (#96): sceAppInstUtilAppInstallAll with /user/app masked to the staged titles.
//! On while /data/krakenmount/mask-test.on exists. The scan pass's install call then runs with
//! /user/app showing only its titles and the tile (KRKN00001); app.db's tbl_contentinfo is counted
//! per title id before and after, and every title that lost its rows is logged as DROPPED.
//! Never MNT_FORCE: an unmount is retried, then left mounted and logged. At start, a mask left by
//! an earlier copy is removed before anything else touches /user/app.
use std::collections::BTreeMap;
use std::ffi::{c_char, CStr, CString};
use std::path::Path;
use std::time::{Duration, Instant};

const FLAG: &str = "/data/krakenmount/mask-test.on";
/// The masked view: one empty folder per visible title.
const STAGE: &str = "/data/krakenmount/mask-stage";
/// A second view of the real /user/app, taken before it is masked, to show the visible titles from.
const REAL: &str = "/data/krakenmount/mask-real";
const APP: &str = "/user/app";
const APP_DB: &str = "/system_data/priv/mms/app.db";
const TILE: &str = "KRKN00001";
/// Copies of app.db (and its -wal/-shm, when there) before and after each install call.
const COPIES: &str = "/data/krakenmount/appdb-copies";

extern "C" {
    fn km_sqlite_exec1(path: *const c_char, sql: *const c_char, arg: *const c_char, err: *mut c_char, errlen: usize) -> i32;
}

pub fn enabled() -> bool {
    Path::new(FLAG).exists()
}

fn mounted_on(path: &str) -> Option<String> {
    let c = CString::new(path).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let on = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) }.to_string_lossy().into_owned();
    let from = unsafe { CStr::from_ptr(st.f_mntfromname.as_ptr()) }.to_string_lossy().into_owned();
    (on == path).then_some(from)
}

fn nullfs(from: &str, to: &str) -> std::io::Result<()> {
    crate::sys::nmount(&[("fstype", "nullfs"), ("from", from), ("fspath", to)], 0)
}

/// The masked /user/app itself read-only: anything written there during the call fails instead
/// of landing in the stage folder. The titles' own folders, mounted on top, stay writable.
fn nullfs_ro(from: &str, to: &str) -> std::io::Result<()> {
    crate::sys::nmount(&[("fstype", "nullfs"), ("from", from), ("fspath", to)], libc::MNT_RDONLY)
}

/// Copies app.db (+ -wal, -shm) to COPIES/<n>-<what>-<when>.db*, n counting calls since start.
fn copy_db(n: u32, what: &str, when: &str) {
    if let Err(e) = std::fs::create_dir_all(COPIES) {
        log!("TEST copy: mkdir {COPIES}: {e}");
        return;
    }
    for ext in ["", "-wal", "-shm"] {
        let src = format!("{APP_DB}{ext}");
        if !Path::new(&src).exists() {
            continue;
        }
        let dst = format!("{COPIES}/{n}-{what}-{when}.db{ext}");
        match std::fs::copy(&src, &dst) {
            Ok(b) => log!("TEST copy: {dst} ({b} bytes)"),
            Err(e) => log!("TEST copy: {src} -> {dst}: {e}"),
        }
    }
}

/// Unmounts `path` while it is a mount, without MNT_FORCE: up to 40 tries 250 ms apart.
fn unmount_retry(path: &str) -> bool {
    let t = Instant::now();
    for i in 0..40 {
        if mounted_on(path).is_none() {
            return true;
        }
        match crate::sys::unmount(path, 0) {
            Ok(()) => {
                log!("TEST mask: unmounted {path} (try {}, {} ms)", i + 1, t.elapsed().as_millis());
                return true;
            }
            Err(e) if i == 0 || i == 39 => log!("TEST mask: unmount {path} (try {}): {e}", i + 1),
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    log!("TEST mask: {path} STILL MOUNTED after 10 s, left as it is (no MNT_FORCE)");
    false
}

fn unmask(titles: &[String]) -> bool {
    let mut ok = true;
    for t in titles {
        ok &= unmount_retry(&format!("{APP}/{t}"));
    }
    ok &= unmount_retry(APP);
    ok & unmount_retry(REAL)
}

/// At start, before anything touches /user/app: a mask left by an earlier copy (payload killed).
pub fn cleanup_leftover() {
    if mounted_on(APP).as_deref() != Some(STAGE) {
        if mounted_on(REAL).is_some() {
            unmount_retry(REAL);
        }
        return;
    }
    let titles: Vec<String> = std::fs::read_dir(STAGE)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    log!("TEST mask: /user/app was still masked by an earlier copy ({titles:?}); removing it");
    let ok = unmask(&titles);
    log!("TEST mask: leftover removed: {ok}; /user/app now shows {} folder(s)", count_app());
}

fn count_app() -> usize {
    std::fs::read_dir(APP).map(|d| d.flatten().count()).unwrap_or(0)
}

fn rows(tid: &str) -> i32 {
    const SQL: &str = "SELECT COUNT(*) FROM tbl_contentinfo WHERE titleId = ?1";
    let (Ok(p), Ok(s), Ok(t)) = (CString::new(APP_DB), CString::new(SQL), CString::new(tid)) else { return -1 };
    let mut msg = [0 as c_char; 256];
    unsafe { km_sqlite_exec1(p.as_ptr(), s.as_ptr(), t.as_ptr(), msg.as_mut_ptr(), msg.len()) }
}

fn is_tid(b: &[u8]) -> bool {
    b.len() == 9 && b[..4].iter().all(u8::is_ascii_uppercase) && b[4..].iter().all(u8::is_ascii_digit)
}

/// Rows per title id in tbl_contentinfo, for every id in app.db's bytes, /user/app and /user/appmeta.
fn snapshot() -> BTreeMap<String, i32> {
    let mut ids: Vec<String> = Vec::new();
    if let Ok(db) = std::fs::read(APP_DB) {
        ids.extend(db.windows(9).filter(|w| is_tid(w)).map(|w| String::from_utf8_lossy(w).into_owned()));
    }
    for dir in [APP, "/user/appmeta"] {
        if let Ok(d) = std::fs::read_dir(dir) {
            ids.extend(d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| is_tid(n.as_bytes())));
        }
    }
    ids.sort();
    ids.dedup();
    ids.into_iter().map(|t| { let n = rows(&t); (t, n) }).collect()
}

fn diff(before: &BTreeMap<String, i32>, after: &BTreeMap<String, i32>, what: &str) {
    let (b, a): (i32, i32) = (before.values().filter(|n| **n > 0).sum(), after.values().filter(|n| **n > 0).sum());
    log!("TEST {what}: tbl_contentinfo rows {b} -> {a} over {} title id(s)", before.len().max(after.len()));
    for (t, n) in before {
        let m = after.get(t).copied().unwrap_or(0);
        if *n > 0 && m <= 0 {
            log!("TEST {what}: DROPPED {t} ({n} row(s) -> {m})");
        } else if *n != m {
            log!("TEST {what}: {t} rows {n} -> {m}");
        }
    }
    for (t, m) in after {
        if *m > 0 && !before.contains_key(t) {
            log!("TEST {what}: new {t} ({m} row(s))");
        }
    }
}

/// Logs what /user/app holds: how many folders, and every one that is not a game (PPSA/CUSA).
fn describe_app() {
    let names: Vec<String> = std::fs::read_dir(APP)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let other: Vec<&String> = names.iter().filter(|n| !n.starts_with("PPSA") && !n.starts_with("CUSA")).collect();
    log!("TEST mask: /user/app holds {} folder(s); not PPSA/CUSA: {other:?}", names.len());
}

/// Runs `call` (the InstallAll) with /user/app masked to `titles` + the tile when the test flag is
/// on, plain otherwise; logs the app.db diff either way. Returns call's result.
pub fn around<T>(titles: &[String], call: impl FnOnce() -> T) -> T {
    let before = snapshot();
    describe_app();
    let mut shown: Vec<String> = titles.to_vec();
    if !shown.iter().any(|t| t == TILE) {
        shown.push(TILE.to_string());
    }
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let masked = enabled() && mask(&shown);
    let what = if masked { "masked" } else { "plain" };
    copy_db(n, what, "before");
    let t = Instant::now();
    let r = call();
    let ms = t.elapsed().as_millis();
    log!("TEST {what}: InstallAll for {titles:?} took {ms} ms");
    if masked {
        let ok = unmask(&shown);
        log!("TEST masked: unmask ok={ok}; /user/app now shows {} folder(s)", count_app());
    }
    copy_db(n, what, "after");
    diff(&before, &snapshot(), what);
    r
}

/// Masks /user/app to `titles`; false (nothing left mounted) when a step fails.
fn mask(titles: &[String]) -> bool {
    if mounted_on(APP).is_some() {
        log!("TEST mask: /user/app is already a mount ({:?}), not masking", mounted_on(APP));
        return false;
    }
    let _ = std::fs::remove_dir_all(STAGE);
    for d in [STAGE, REAL] {
        if let Err(e) = std::fs::create_dir_all(d) {
            log!("TEST mask: mkdir {d}: {e}");
            return false;
        }
    }
    let shown: Vec<&String> = titles.iter().filter(|t| Path::new(APP).join(t.as_str()).is_dir()).collect();
    for t in &shown {
        if let Err(e) = std::fs::create_dir(Path::new(STAGE).join(t.as_str())) {
            log!("TEST mask: mkdir stage {t}: {e}");
            return false;
        }
    }
    if let Err(e) = nullfs(APP, REAL) {
        log!("TEST mask: nullfs {APP} -> {REAL}: {e}");
        return false;
    }
    if let Err(e) = nullfs_ro(STAGE, APP) {
        log!("TEST mask: nullfs {STAGE} -> {APP}: {e}");
        unmount_retry(REAL);
        return false;
    }
    for (i, t) in shown.iter().enumerate() {
        if let Err(e) = nullfs(&format!("{REAL}/{t}"), &format!("{APP}/{t}")) {
            log!("TEST mask: nullfs {REAL}/{t} -> {APP}/{t}: {e}");
            let done: Vec<String> = shown[..i].iter().map(|s| s.to_string()).collect();
            unmask(&done);
            return false;
        }
    }
    let probe = std::fs::create_dir(format!("{APP}/ZZZZ00000")).map(|_| "writable!").unwrap_or_else(|_| "read-only");
    log!("TEST mask: /user/app shows only {shown:?} ({} folder(s) visible), {probe}", count_app());
    true
}

/// GET /api/test/mask-kill/<TID>: masks /user/app to that title and the tile, then ends the payload
/// without unmounting, like a payload killed mid-call. The next start must remove it.
pub fn kill_mid_mask(tid: &str) -> String {
    let shown = vec![tid.to_string(), TILE.to_string()];
    if !mask(&shown) {
        return "could not mask, see kraken.log".to_string();
    }
    log!("TEST mask: exiting now with /user/app masked; send Kraken Mount again to check the cleanup");
    std::process::exit(0);
}
