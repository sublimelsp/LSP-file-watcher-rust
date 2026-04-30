use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, BufWriter, Write as _};
use std::mem;
use std::path;
use std::process::exit;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use notify::Watcher as _;
use serde::Deserialize;
use walkdir::WalkDir;

fn parent_died() -> ! {
    eprintln!("parent process died");
    exit(1);
}

#[cfg(target_os = "linux")]
fn parent_process_watchdog() -> ! {
    let ppid = unsafe { libc::getppid() };
    if ppid <= 1 {
        parent_died();
    }

    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, ppid, 0) };
    if pidfd < 0 {
        parent_died();
    }

    #[allow(clippy::cast_possible_truncation)]
    let mut fds = [libc::pollfd {
        fd: pidfd as i32,
        events: libc::POLLIN,
        revents: 0,
    }];

    loop {
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
        if n > 0 {
            parent_died();
        }
        if n < 0 {
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno != libc::EINTR {
                parent_died();
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn parent_process_watchdog() -> ! {
    use libc::{kevent, kqueue, EVFILT_PROC, EV_ADD, EV_ONESHOT, NOTE_EXIT};
    use std::ptr::{null, null_mut};

    let ppid = unsafe { libc::getppid() };
    if ppid <= 1 {
        parent_died();
    }

    let kq = unsafe { kqueue() };
    if kq < 0 {
        parent_died();
    }

    #[allow(clippy::cast_sign_loss)]
    let change = kevent {
        ident: ppid as usize,
        filter: EVFILT_PROC,
        flags: EV_ADD | EV_ONESHOT,
        fflags: NOTE_EXIT,
        data: 0,
        udata: null_mut(),
    };

    // Registration returns ESRCH if the parent already died.
    let ret = unsafe { kevent(kq, &raw const change, 1, null_mut(), 0, null()) };
    if ret < 0 {
        parent_died();
    }

    let mut event = unsafe { mem::zeroed::<kevent>() };
    loop {
        let n = unsafe { kevent(kq, null(), 0, &raw mut event, 1, null()) };
        if n > 0 {
            parent_died();
        }
        if n < 0 {
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno != libc::EINTR {
                parent_died();
            }
        }
    }
}

#[cfg(windows)]
fn parent_process_watchdog() -> ! {
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, WaitForSingleObject, INFINITE, PROCESS_ACCESS_RIGHTS,
    };

    let mut info = [0_usize; 6];
    let mut r_len = 0;
    let current_process = unsafe { GetCurrentProcess() };
    assert!(unsafe {
        NtQueryInformationProcess(
            current_process,
            PROCESSINFOCLASS(0),
            info.as_mut_ptr().cast(),
            (size_of::<usize>() * 6) as _,
            &raw mut r_len,
        )
    }
    .is_ok());
    assert_eq!(r_len as usize, size_of::<usize>() * 6);

    let ppid = info[5] as u32;
    let Ok(pph) = (unsafe { OpenProcess(PROCESS_ACCESS_RIGHTS(0x0010_0000), false, ppid) }) else {
        parent_died();
    };

    let _ = unsafe { WaitForSingleObject(pph, INFINITE) };
    parent_died();
}

#[cfg(target_os = "linux")]
fn enter_efficiency_mode() {
    let param: libc::sched_param = unsafe { mem::zeroed() };
    let _ = unsafe { libc::sched_setscheduler(0, libc::SCHED_BATCH, &raw const param) };
}

#[cfg(windows)]
fn enter_efficiency_mode() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    };

    let info = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    };
    let current_process = unsafe { GetCurrentProcess() };
    let _ = unsafe {
        SetProcessInformation(
            current_process,
            ProcessPowerThrottling,
            (&raw const info).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as _,
        )
    };
}

#[cfg(target_os = "macos")]
fn enter_efficiency_mode() {
    let _ = unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG) };
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EventType {
    Create,
    Change,
    Delete,
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create => "create",
            Self::Change => "change",
            Self::Delete => "delete",
        }
        .fmt(f)
    }
}

struct Report {
    uid: usize,
    event: EventType,
    path: Utf8PathBuf,
    timestamp: Instant,
}

#[derive(Debug, Deserialize)]
struct Register {
    cwd: String,
    events: Vec<EventType>,
    ignores: Vec<String>,
    patterns: Vec<String>,
    uid: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Request {
    Register(Register),
    Unregister(usize),
}

fn is_glob_str(s: &str) -> bool {
    s.bytes().next() == Some(b'!') || s.bytes().any(|ch| matches!(ch, b'*' | b'?' | b'{' | b'['))
}

#[derive(Copy, Clone)]
enum PathKind {
    Missing,
    Dir,
    File,
}

fn classify(path: &Utf8Path) -> Option<PathKind> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(PathKind::Missing),
        Err(e) => {
            eprintln!("watcher: stat error for {path}: {e}");
            None
        }
        Ok(m) if m.is_dir() => Some(PathKind::Dir),
        Ok(_) => Some(PathKind::File),
    }
}

fn normalize_pattern(cwd: &Utf8Path, pat: &str) -> Utf8PathBuf {
    #[cfg(windows)]
    let pat = pat.replace('/', "\\");

    let p = Utf8Path::new(&pat);
    if p.is_absolute() {
        p.to_owned()
    } else {
        // absolute_utf8 normalises `.` and `..` without touching glob metacharacters
        let joined = cwd.join(p);
        camino::absolute_utf8(&joined).unwrap_or(joined)
    }
}

fn build_globset(patterns: impl IntoIterator<Item = impl AsRef<str>>) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = GlobBuilder::new(p.as_ref()).literal_separator(true).build() {
            builder.add(g);
        } else {
            eprintln!("invalid glob pattern: {}", p.as_ref());
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap())
}

struct PatternMatcher {
    literal_files: HashSet<Utf8PathBuf>,
    literal_dirs: Vec<Utf8PathBuf>,
    glob_set: GlobSet,
    ignore_set: GlobSet,
}

impl PatternMatcher {
    fn new(cwd: &Utf8Path, patterns: &[String], ignores: &[String]) -> Self {
        let mut literal_files = HashSet::new();
        let mut literal_dirs: Vec<Utf8PathBuf> = Vec::new();
        let mut glob_patterns: Vec<String> = Vec::new();
        let mut ignore_patterns: Vec<String> = Vec::new();

        for pat in patterns {
            let abs = normalize_pattern(cwd, pat);
            if is_glob_str(pat) {
                glob_patterns.push(abs.into_string());
            } else if abs.is_dir() {
                literal_dirs.push(abs);
            } else {
                literal_files.insert(abs);
            }
        }

        for ign in ignores {
            let abs = normalize_pattern(cwd, ign);
            let mut abs_str = abs.into_string();
            if !is_glob_str(ign) {
                // literal ignore: also add path/** to exclude children
                let mut with_children = abs_str.clone();
                with_children.extend([path::MAIN_SEPARATOR, '*', '*']);
                mem::swap(&mut abs_str, &mut with_children);
                ignore_patterns.push(with_children);
            }
            ignore_patterns.push(abs_str);
        }

        Self {
            literal_files,
            literal_dirs,
            glob_set: build_globset(&glob_patterns),
            ignore_set: build_globset(&ignore_patterns),
        }
    }

    fn matches(&self, path: &Utf8Path) -> bool {
        self.literal_files.contains(path)
            || self.literal_dirs.iter().any(|d| path.starts_with(d))
            || self.glob_set.is_match(path)
    }

    fn is_ignored(&self, path: &Utf8Path) -> bool {
        if self.ignore_set.is_match(path) {
            return true;
        }
        // Mirror chokidar's DOT_RE: /\..*\.(sw[px])$|~$|\.subl.*\.tmp/
        // tested against the full path string.
        let s = path.as_str();
        if s.ends_with('~') {
            return true;
        }
        for suffix in [".swp", ".swx", ".swpx"] {
            if let Some(stripped) = s.strip_suffix(suffix) {
                // \..*\.swp$ requires a dot before the trailing suffix.
                if stripped.contains('.') {
                    return true;
                }
            }
        }
        if let Some(idx) = s.find(".subl") {
            if s[idx + ".subl".len()..].contains(".tmp") {
                return true;
            }
        }
        false
    }

    fn is_emittable(&self, path: &Utf8Path) -> bool {
        self.matches(path) && !self.is_ignored(path)
    }
}

const CHANGE_THROTTLE_MS: u64 = 50;
const REMOVE_THROTTLE_MS: u64 = 100;
const ATOMIC_WINDOW_MS: u64 = 100;

struct WatcherState {
    uid: usize,
    cwd: Utf8PathBuf,
    events: Vec<EventType>,
    matcher: PatternMatcher,
    // Every emittable file we currently know about.
    tracked_files: HashSet<Utf8PathBuf>,
    // Tracked directory → set of child basenames (dirs and emittable files).
    tracked_dirs: HashMap<Utf8PathBuf, HashSet<String>>,
    // Per-path last-emit timestamps for throttling.
    last_change: HashMap<Utf8PathBuf, Instant>,
    last_remove: HashMap<Utf8PathBuf, Instant>,
    // Deferred delete events (atomic-write detection).
    // Value = original event time (deadline = value + ATOMIC_WINDOW_MS).
    pending_unlinks: HashMap<Utf8PathBuf, Instant>,
}

impl WatcherState {
    fn new(uid: usize, cwd: Utf8PathBuf, events: Vec<EventType>, matcher: PatternMatcher) -> Self {
        Self {
            uid,
            cwd,
            events,
            matcher,
            tracked_files: HashSet::new(),
            tracked_dirs: HashMap::new(),
            last_change: HashMap::new(),
            last_remove: HashMap::new(),
            pending_unlinks: HashMap::new(),
        }
    }

    fn initial_scan(&mut self) {
        // Disjoint field borrows: the walker's filter_entry closure captures
        // `cwd` and `matcher` immutably while the loop body mutates the two
        // tracking maps — keeps `self.cwd` accessible without cloning.
        let Self {
            cwd,
            matcher,
            tracked_dirs,
            tracked_files,
            ..
        } = self;
        let walker = WalkDir::new(cwd.as_std_path()).follow_links(false);
        for entry in walker.into_iter().filter_entry(|e| {
            Utf8Path::from_path(e.path()).is_none_or(|p| p == *cwd || !matcher.is_ignored(p))
        }) {
            let Ok(entry) = entry else { continue };
            let Some(path) = Utf8Path::from_path(entry.path()).map(Utf8Path::to_path_buf) else {
                continue;
            };
            let file_type = entry.file_type();

            if file_type.is_dir() {
                if &path != cwd {
                    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                        tracked_dirs
                            .entry(parent.to_path_buf())
                            .or_default()
                            .insert(name.to_owned());
                    }
                }
                tracked_dirs.entry(path).or_default();
            } else if (file_type.is_file() || file_type.is_symlink()) && matcher.is_emittable(&path)
            {
                if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                    tracked_dirs
                        .entry(parent.to_path_buf())
                        .or_default()
                        .insert(name.to_owned());
                }
                tracked_files.insert(path);
            }
        }
    }

    fn dispatch(
        &mut self,
        path: &Utf8Path,
        kind: PathKind,
        queue: &mut Vec<Report>,
        acquired: &mut Vec<Utf8PathBuf>,
        released: &mut Vec<Utf8PathBuf>,
    ) {
        if !path.starts_with(&self.cwd) {
            return;
        }
        match kind {
            PathKind::Missing => self.handle_deletion(path.to_path_buf(), released),
            PathKind::Dir => self.handle_directory(path.to_path_buf(), queue, acquired, released),
            PathKind::File => self.handle_file(path.to_path_buf(), queue),
        }
    }

    fn handle_deletion(&mut self, path: Utf8PathBuf, released: &mut Vec<Utf8PathBuf>) {
        let parent = match path.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        };
        let name = match path.file_name() {
            Some(n) => n.to_owned(),
            None => return,
        };

        let was_tracked_file = self.tracked_files.contains(&path);
        let was_tracked_dir = self.tracked_dirs.contains_key(&path);

        if !was_tracked_file && !was_tracked_dir {
            return;
        }

        let now = Instant::now();

        // 100 ms remove throttle
        if let Some(&last) = self.last_remove.get(&path) {
            if now.duration_since(last) < Duration::from_millis(REMOVE_THROTTLE_MS) {
                return;
            }
        }
        self.last_remove.insert(path.clone(), now);

        if was_tracked_dir {
            let children = self.tracked_dirs.remove(&path).unwrap_or_default();
            for child in &children {
                self.handle_deletion(path.join(child), released);
            }
        }

        if was_tracked_file {
            self.tracked_files.remove(&path);
        }

        // Remove from parent's child set.
        if let Some(parent_set) = self.tracked_dirs.get_mut(&parent) {
            parent_set.remove(&name);
        }

        let defer_unlink = was_tracked_file && self.events.contains(&EventType::Delete);
        match (was_tracked_dir, defer_unlink) {
            (true, true) => {
                released.push(path.clone());
                // Defer: wait for a possible same-path Add within the atomic window.
                self.pending_unlinks.insert(path, now);
            }
            (true, false) => released.push(path),
            (false, true) => {
                self.pending_unlinks.insert(path, now);
            }
            (false, false) => {}
        }
    }

    fn handle_directory(
        &mut self,
        path: Utf8PathBuf,
        queue: &mut Vec<Report>,
        acquired: &mut Vec<Utf8PathBuf>,
        released: &mut Vec<Utf8PathBuf>,
    ) {
        // Take the previous child set out of self so we can mutate `self`
        // freely below without re-borrowing it for the diff.
        let prev_opt = self.tracked_dirs.remove(&path);
        let already_tracked = prev_opt.is_some();
        let mut prev = prev_opt.unwrap_or_default();

        if path != self.cwd {
            if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                self.tracked_dirs
                    .entry(parent.to_path_buf())
                    .or_default()
                    .insert(name.to_owned());
            }
        }

        // Snapshot what's currently on disk (filtered).
        let current: HashSet<String> = match fs::read_dir(&path) {
            Err(_) => {
                // Restore the previous tracking so we don't lose state on a transient stat failure.
                self.tracked_dirs.insert(path, prev);
                return;
            }
            Ok(rd) => rd
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let cp = Utf8PathBuf::from_path_buf(entry.path()).ok()?;
                    let name = entry.file_name().into_string().ok()?;
                    let ft = entry.file_type().ok()?;
                    if ft.is_dir() {
                        if self.matcher.is_ignored(&cp) {
                            None
                        } else {
                            Some(name)
                        }
                    } else {
                        self.matcher.is_emittable(&cp).then_some(name)
                    }
                })
                .collect(),
        };

        // Partition by moving Strings instead of cloning:
        //   - in current and prev   → intersection (still tracked)
        //   - in current, not prev  → new_entries  (dispatch as new)
        //   - in prev, not current  → gone        (left in `prev`, then deleted)
        let mut new_entries: Vec<String> = Vec::new();
        let mut intersection: HashSet<String> = HashSet::with_capacity(prev.len());
        for name in current {
            if prev.remove(&name) {
                intersection.insert(name);
            } else {
                new_entries.push(name);
            }
        }

        for name in new_entries {
            let child = path.join(&name);
            if let Some(kind) = classify(&child) {
                self.dispatch(&child, kind, queue, acquired, released);
            }
        }
        for name in prev {
            self.handle_deletion(path.join(&name), released);
        }

        // Track new directories so the hub can install a watch on them.
        if !already_tracked {
            acquired.push(path.clone());
        }
        // Dispatch repopulated tracked_dirs[path] with the new entries; merge the
        // surviving intersection back in so the entry mirrors on-disk state.
        self.tracked_dirs
            .entry(path)
            .or_default()
            .extend(intersection);
    }

    fn handle_file(&mut self, path: Utf8PathBuf, queue: &mut Vec<Report>) {
        if !self.matcher.is_emittable(&path) {
            return;
        }

        let parent = match path.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        };
        let name = match path.file_name() {
            Some(n) => n.to_owned(),
            None => return,
        };

        let was_tracked = self.tracked_files.contains(&path);
        let now = Instant::now();

        if self.pending_unlinks.remove(&path).is_some() {
            // Atomic write: an unlink was pending for this path and an add arrived.
            // Emit change instead of (delete + create).
            self.tracked_dirs.entry(parent).or_default().insert(name);

            if self.events.contains(&EventType::Change) && self.allow_change(&path, now) {
                self.tracked_files.insert(path.clone());
                queue.push(Report {
                    uid: self.uid,
                    event: EventType::Change,
                    path,
                    timestamp: now,
                });
            } else {
                self.tracked_files.insert(path);
            }
        } else if was_tracked {
            // Known file — emit change with 50 ms throttle.
            if self.events.contains(&EventType::Change) && self.allow_change(&path, now) {
                queue.push(Report {
                    uid: self.uid,
                    event: EventType::Change,
                    path,
                    timestamp: now,
                });
            }
        } else {
            // New file — emit create.
            self.tracked_dirs.entry(parent).or_default().insert(name);

            if self.events.contains(&EventType::Create) {
                self.tracked_files.insert(path.clone());
                queue.push(Report {
                    uid: self.uid,
                    event: EventType::Create,
                    path,
                    timestamp: now,
                });
            } else {
                self.tracked_files.insert(path);
            }
        }
    }

    fn allow_change(&mut self, path: &Utf8Path, now: Instant) -> bool {
        if let Some(&last) = self.last_change.get(path) {
            if now.duration_since(last) < Duration::from_millis(CHANGE_THROTTLE_MS) {
                return false;
            }
        }
        self.last_change.insert(path.to_path_buf(), now);
        true
    }

    /// Materialise deferred deletes whose atomic window has passed.
    /// Returns the earliest remaining deadline (if any) so the caller can
    /// schedule the next wake-up. Also evicts expired throttle entries.
    fn flush_pending(&mut self, now: Instant, queue: &mut Vec<Report>) -> Option<Instant> {
        let window = Duration::from_millis(ATOMIC_WINDOW_MS);
        let change_window = Duration::from_millis(CHANGE_THROTTLE_MS);
        let remove_window = Duration::from_millis(REMOVE_THROTTLE_MS);

        self.last_change
            .retain(|_, &mut t| now.saturating_duration_since(t) < change_window);
        self.last_remove
            .retain(|_, &mut t| now.saturating_duration_since(t) < remove_window);

        let mut earliest: Option<Instant> = None;
        let pending = mem::take(&mut self.pending_unlinks);
        for (path, orig) in pending {
            let deadline = orig + window;
            if now >= deadline {
                queue.push(Report {
                    uid: self.uid,
                    event: EventType::Delete,
                    path,
                    timestamp: orig,
                });
            } else {
                earliest = Some(earliest.map_or(deadline, |e| e.min(deadline)));
                self.pending_unlinks.insert(path, orig);
            }
        }

        earliest
    }
}

// FSEvents on macOS is inherently recursive and incurs a full stream
// stop/restart per `watch()` call (see notify::fsevent::watch_inner). Acquiring
// one watch per directory under cwd would mean N stream restarts for an
// initial scan, taking ~1 s each on real projects. Instead we install a single
// recursive watch on each registration's cwd and rely on PatternMatcher /
// WatcherState to filter sub-tree events in user space (mirroring chokidar 3).
//
// On Linux/Windows the inotify/ReadDirectoryChangesW backends do not support
// recursive natively, so notify simulates it by watching each directory
// individually. With overlapping registrations (common in LSP), per-dir
// refcounting via Hub avoids redundant kernel-level watches on the same inode.
#[cfg(target_os = "macos")]
const WATCH_MODE: notify::RecursiveMode = notify::RecursiveMode::Recursive;
#[cfg(not(target_os = "macos"))]
const WATCH_MODE: notify::RecursiveMode = notify::RecursiveMode::NonRecursive;

// Owns the single RecommendedWatcher and refcounts OS-level watches.
//
// On Linux/Windows the keys are every directory inside each registration's
// tracked subtree. On macOS the keys are the registration cwds (one recursive
// watch per cwd, deduplicated across overlapping registrations).
struct Hub {
    watcher: notify::RecommendedWatcher,
    refs: HashMap<Utf8PathBuf, usize>,
}

impl Hub {
    fn acquire(&mut self, path: &Utf8Path) {
        let n = self.refs.entry(path.to_path_buf()).or_insert(0);
        *n += 1;
        if *n == 1 {
            if let Err(e) = self.watcher.watch(path.as_std_path(), WATCH_MODE) {
                eprintln!("watch({path}) failed: {e:?}");
            }
        }
    }

    fn release(&mut self, path: &Utf8Path) {
        if let Some(n) = self.refs.get_mut(path) {
            *n -= 1;
            if *n == 0 {
                self.refs.remove(path);
                let _ = self.watcher.unwatch(path.as_std_path());
            }
        }
    }
}

type Queue = &'static Mutex<Vec<Report>>;
type States = &'static Mutex<HashMap<usize, Arc<Mutex<WatcherState>>>>;
type HubHandle = &'static Mutex<Hub>;

fn register_watcher(reg: Register, hub: HubHandle, states: States) {
    let uid = reg.uid;
    let cwd = camino::absolute_utf8(&reg.cwd).unwrap_or_else(|_| Utf8PathBuf::from(&reg.cwd));
    let matcher = PatternMatcher::new(&cwd, &reg.patterns, &reg.ignores);

    let mut state = WatcherState::new(uid, cwd, reg.events, matcher);
    state.initial_scan();

    // macOS: a single recursive FSEvents watch on cwd covers the whole subtree.
    // Other OSes: one non-recursive watch per directory discovered in the scan.
    #[cfg(target_os = "macos")]
    {
        hub.lock().unwrap().acquire(&state.cwd);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let dirs: Vec<Utf8PathBuf> = state.tracked_dirs.keys().cloned().collect();
        let mut h = hub.lock().unwrap();
        for dir in dirs {
            h.acquire(&dir);
        }
    }

    let state = Arc::new(Mutex::new(state));
    states.lock().unwrap().insert(uid, state);
}

fn dispatch_worker(
    rx: mpsc::Receiver<notify::Event>,
    #[allow(unused_variables)] hub: HubHandle,
    queue: Queue,
    states: States,
) {
    while let Ok(event) = rx.recv() {
        // Stat each path once; reuse across all watchers.
        let path_kinds: Vec<(Utf8PathBuf, PathKind)> = event
            .paths
            .into_iter()
            .filter_map(|p| Utf8PathBuf::from_path_buf(p).ok())
            .filter_map(|p| classify(&p).map(|k| (p, k)))
            .collect();

        let state_arcs: Vec<Arc<Mutex<WatcherState>>> = {
            let map = states.lock().unwrap();
            map.values().cloned().collect()
        };

        let mut reports: Vec<Report> = Vec::new();
        let mut acquired: Vec<Utf8PathBuf> = Vec::new();
        let mut released: Vec<Utf8PathBuf> = Vec::new();

        for state_arc in state_arcs {
            let mut s = state_arc.lock().unwrap();
            for (path, kind) in &path_kinds {
                s.dispatch(path, *kind, &mut reports, &mut acquired, &mut released);
            }
        }

        // The recursive macOS watch already covers any new sub-directory, so
        // the deltas WatcherState produces are simply discarded there.
        #[cfg(not(target_os = "macos"))]
        if !acquired.is_empty() || !released.is_empty() {
            let mut h = hub.lock().unwrap();
            for dir in acquired {
                h.acquire(&dir);
            }
            for dir in released {
                h.release(&dir);
            }
        }

        if !reports.is_empty() {
            queue.lock().unwrap().append(&mut reports);
        }
    }
}

fn handle_reports(queue: Queue, states: States) -> ! {
    let debounce = Duration::from_millis(400);
    let min_sleep = Duration::from_millis(10);
    let mut to_sleep = debounce;

    loop {
        thread::sleep(to_sleep);
        to_sleep = debounce;
        let now = Instant::now();

        // Flush pending unlinks whose atomic window has expired.
        let mut flush_buf: Vec<Report> = Vec::new();
        let mut earliest_deadline: Option<Instant> = None;
        {
            if let Ok(map) = states.try_lock() {
                for state_arc in map.values() {
                    if let Ok(mut s) = state_arc.try_lock() {
                        if let Some(ed) = s.flush_pending(now, &mut flush_buf) {
                            earliest_deadline = Some(earliest_deadline.map_or(ed, |e| e.min(ed)));
                        }
                    }
                }
            }
        }
        if !flush_buf.is_empty() {
            queue.lock().unwrap().append(&mut flush_buf);
        }

        // Trim to_sleep so we don't overshoot an upcoming deadline.
        if let Some(deadline) = earliest_deadline {
            let time_to = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(min_sleep);
            to_sleep = to_sleep.min(time_to.max(min_sleep));
        }

        // Drain and emit if the debounce period has passed since the last report.
        let q = if let Ok(mut g) = queue.try_lock() {
            let Some(last) = g.last() else { continue };
            let handle_at = last.timestamp + debounce;
            if let Some(remaining) = handle_at.checked_duration_since(Instant::now()) {
                to_sleep = to_sleep.min(remaining);
                continue;
            }
            mem::take(&mut *g)
        } else {
            continue;
        };

        // Collapse overlapping events for the same (uid, path), preserving order.
        let mut path_states: HashMap<(usize, Utf8PathBuf), (usize, EventType)> = HashMap::new();
        for (i, report) in q.into_iter().enumerate() {
            let Report {
                uid, event, path, ..
            } = report;
            match path_states.entry((uid, path)) {
                Entry::Vacant(e) => {
                    e.insert((i, event));
                }
                Entry::Occupied(mut e) => {
                    let stored = &mut e.get_mut().1;
                    let merged = match (*stored, event) {
                        (EventType::Delete, EventType::Create) => Some(EventType::Change),
                        (EventType::Create, EventType::Delete) => None,
                        (EventType::Create, EventType::Change) => Some(EventType::Create),
                        _ => Some(event),
                    };
                    match merged {
                        Some(ev) => *stored = ev,
                        None => {
                            e.remove();
                        }
                    }
                }
            }
        }

        let mut ordered: Vec<_> = path_states
            .into_iter()
            .map(|((uid, path), (idx, event))| (idx, uid, event, path))
            .collect();
        ordered.sort_unstable_by_key(|e| e.0);

        let mut stdout = BufWriter::new(io::stdout().lock());
        for (_, uid, event, path) in ordered {
            let _ = writeln!(stdout, "{uid}:{event}:{path}");
        }
        let _ = writeln!(stdout, "<flush>");
    }
}

fn main() {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        enter_efficiency_mode();
        drop(thread::spawn(parent_process_watchdog));
    }

    let queue: Queue = Box::leak(Box::new(Mutex::new(Vec::new())));
    let states: States = Box::leak(Box::new(Mutex::new(HashMap::new())));

    let (event_tx, event_rx) = mpsc::channel::<notify::Event>();
    let watcher =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(e) => {
                let _ = event_tx.send(e);
            }
            Err(e) => eprintln!("watcher error: {e:?}"),
        })
        .expect("failed to create watcher");

    let hub: HubHandle = Box::leak(Box::new(Mutex::new(Hub {
        watcher,
        refs: HashMap::new(),
    })));

    drop(thread::spawn(move || {
        dispatch_worker(event_rx, hub, queue, states);
    }));
    drop(thread::spawn(move || handle_reports(queue, states)));

    for line in io::stdin().lines() {
        let line = line.expect("failed to read from stdin");
        let request: Request = serde_json::from_str(&line).expect("failed to parse input");

        match request {
            Request::Register(reg) => {
                let uid = reg.uid;
                let already = states.lock().unwrap().contains_key(&uid);
                if already {
                    eprintln!("watcher with ID {uid} already exists");
                } else {
                    register_watcher(reg, hub, states);
                }
            }
            Request::Unregister(uid) => {
                let state = states.lock().unwrap().remove(&uid);
                match state {
                    None => eprintln!("watcher with ID {uid} not found"),
                    Some(arc) => {
                        #[cfg(target_os = "macos")]
                        {
                            let cwd = arc.lock().unwrap().cwd.clone();
                            hub.lock().unwrap().release(&cwd);
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            let dirs: Vec<Utf8PathBuf> =
                                arc.lock().unwrap().tracked_dirs.keys().cloned().collect();
                            let mut h = hub.lock().unwrap();
                            for dir in dirs {
                                h.release(&dir);
                            }
                        }
                    }
                }
            }
        }
    }

    exit(0);
}
