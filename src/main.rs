use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufWriter, Write};
use std::mem;
use std::path::{self, Path, PathBuf};
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use notify::Watcher;
use serde::Deserialize;
use walkdir::WalkDir;

#[cfg(any(target_os = "linux", windows))]
fn parent_died() -> ! {
    eprintln!("parent process died");
    exit(1);
}

#[cfg(target_os = "linux")]
fn parent_process_watchdog() -> ! {
    use rustix::event::{poll, PollFd, PollFlags};
    use rustix::io::Errno;
    use rustix::process::{getppid, pidfd_open, PidfdFlags};

    let Some(ppid) = getppid() else {
        parent_died();
    };

    let Ok(ppid_fd) = pidfd_open(ppid, PidfdFlags::empty()) else {
        parent_died();
    };

    let mut fds = [PollFd::new(&ppid_fd, PollFlags::IN)];

    loop {
        match poll(&mut fds, -1) {
            Ok(_) => parent_died(),
            Err(Errno::INTR) => continue,
            Err(e) => panic!("poll failed: {e:?}"),
        }
    }
}

#[cfg(windows)]
fn parent_process_watchdog() -> ! {
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, WaitForSingleObject, INFINITE, PROCESS_ACCESS_RIGHTS,
    };

    let mut info = [0usize; 6];
    let mut r_len = 0;
    assert!(unsafe {
        NtQueryInformationProcess(
            GetCurrentProcess(),
            PROCESSINFOCLASS(0),
            info.as_mut_ptr() as _,
            (size_of::<usize>() * 6) as _,
            &raw mut r_len,
        )
    }
    .is_ok());
    assert_eq!(r_len as usize, size_of::<usize>() * 6);

    let ppid = info[5] as u32;
    let Ok(pph) = (unsafe { OpenProcess(PROCESS_ACCESS_RIGHTS(0x00100000), false, ppid) }) else {
        parent_died();
    };

    let _ = unsafe { WaitForSingleObject(pph, INFINITE) };
    parent_died();
}

#[cfg(target_os = "linux")]
fn enter_efficiency_mode() {
    use rustix::process::{sched_setscheduler, SchedParam, SchedPolicy};

    let _ = sched_setscheduler(None, SchedPolicy::Batch, &SchedParam::default());
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
    let _ = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &raw const info as _,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as _,
        )
    };
}

#[cfg(not(any(target_os = "linux", windows)))]
fn enter_efficiency_mode() {}

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
            EventType::Create => "create",
            EventType::Change => "change",
            EventType::Delete => "delete",
        }
        .fmt(f)
    }
}

struct Report {
    uid: usize,
    event: EventType,
    path: PathBuf,
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
    s.contains('*') || s.contains('?') || s.contains('{') || s.contains('[') || s.starts_with('!')
}

fn normalize_pattern(cwd: &Path, pat: &str) -> PathBuf {
    #[cfg(windows)]
    let pat = pat.replace('/', "\\");
    #[cfg(not(windows))]
    let pat: &str = pat;

    let p = Path::new(pat);
    if p.is_absolute() {
        PathBuf::from(pat)
    } else {
        // path::absolute normalises `.` and `..` without touching glob metacharacters
        path::absolute(cwd.join(pat)).unwrap_or_else(|_| cwd.join(pat))
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
    literal_files: HashSet<PathBuf>,
    literal_dirs: Vec<PathBuf>,
    glob_set: GlobSet,
    ignore_set: GlobSet,
}

impl PatternMatcher {
    fn new(cwd: &Path, patterns: &[String], ignores: &[String]) -> Self {
        let mut literal_files = HashSet::new();
        let mut literal_dirs: Vec<PathBuf> = Vec::new();
        let mut glob_patterns: Vec<String> = Vec::new();
        let mut ignore_patterns: Vec<String> = Vec::new();

        for pat in patterns {
            let abs = normalize_pattern(cwd, pat);
            if is_glob_str(pat) {
                glob_patterns.push(abs.to_string_lossy().into_owned());
            } else {
                if abs.is_dir() {
                    literal_dirs.push(abs);
                } else {
                    literal_files.insert(abs);
                }
            }
        }

        for ign in ignores {
            let abs = normalize_pattern(cwd, ign);
            let abs_str = abs.to_string_lossy().into_owned();
            if is_glob_str(ign) {
                ignore_patterns.push(abs_str);
            } else {
                // literal ignore: also add path/** to exclude children
                ignore_patterns.push(abs_str.clone());
                ignore_patterns.push(format!(
                    "{}{sep}**",
                    abs_str,
                    sep = std::path::MAIN_SEPARATOR
                ));
            }
        }

        Self {
            literal_files,
            literal_dirs,
            glob_set: build_globset(&glob_patterns),
            ignore_set: build_globset(&ignore_patterns),
        }
    }

    fn matches(&self, path: &Path) -> bool {
        self.literal_files.contains(path)
            || self.literal_dirs.iter().any(|d| path.starts_with(d))
            || self.glob_set.is_match(path)
    }

    fn is_ignored(&self, path: &Path) -> bool {
        if self.ignore_set.is_match(path) {
            return true;
        }
        // Editor temp-file filter (mirrors chokidar's atomic DOT_RE):
        //   vim swap:    .*.swp / .*.swx / .*.swpx
        //   generic bak: *~
        //   sublime tmp: .subl*.tmp
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.ends_with('~') {
                return true;
            }
            if name.starts_with('.') {
                if name.ends_with(".swp") || name.ends_with(".swx") || name.ends_with(".swpx") {
                    return true;
                }
                if name.starts_with(".subl") && name.ends_with(".tmp") {
                    return true;
                }
            }
        }
        false
    }

    fn is_emittable(&self, path: &Path) -> bool {
        self.matches(path) && !self.is_ignored(path)
    }
}

const CHANGE_THROTTLE_MS: u64 = 50;
const REMOVE_THROTTLE_MS: u64 = 100;
const ATOMIC_WINDOW_MS: u64 = 100;

struct WatcherState {
    uid: usize,
    cwd: PathBuf,
    events: Vec<EventType>,
    matcher: PatternMatcher,
    // Every emittable file we currently know about.
    tracked_files: HashSet<PathBuf>,
    // Tracked directory → set of child basenames (dirs and emittable files).
    tracked_dirs: HashMap<PathBuf, HashSet<OsString>>,
    // Per-path last-emit timestamps for throttling.
    last_change: HashMap<PathBuf, Instant>,
    last_remove: HashMap<PathBuf, Instant>,
    // Deferred delete events (atomic-write detection).
    // Value = original event time (deadline = value + ATOMIC_WINDOW_MS).
    pending_unlinks: HashMap<PathBuf, Instant>,
}

impl WatcherState {
    fn new(uid: usize, cwd: PathBuf, events: Vec<EventType>, matcher: PatternMatcher) -> Self {
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
        let cwd = self.cwd.clone();
        let walker = WalkDir::new(&cwd).follow_links(false);
        for entry in walker
            .into_iter()
            .filter_entry(|e| e.path() == cwd || !self.matcher.is_ignored(e.path()))
        {
            let Ok(entry) = entry else { continue };
            let path = entry.path().to_path_buf();
            let file_type = entry.file_type();

            if file_type.is_dir() {
                self.tracked_dirs.entry(path.clone()).or_default();
                if path != cwd {
                    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                        self.tracked_dirs
                            .entry(parent.to_path_buf())
                            .or_default()
                            .insert(name.to_os_string());
                    }
                }
            } else if file_type.is_file() || file_type.is_symlink() {
                if self.matcher.is_emittable(&path) {
                    self.tracked_files.insert(path.clone());
                    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                        self.tracked_dirs
                            .entry(parent.to_path_buf())
                            .or_default()
                            .insert(name.to_os_string());
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, path: &Path, queue: &mut Vec<Report>) {
        if !path.starts_with(&self.cwd) {
            return;
        }

        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.handle_deletion(path.to_path_buf(), queue);
            }
            Err(e) => {
                eprintln!("watcher: stat error for {}: {e}", path.display());
            }
            Ok(meta) => {
                if meta.is_dir() {
                    self.handle_directory(path.to_path_buf(), queue);
                } else {
                    self.handle_file(path.to_path_buf(), queue);
                }
            }
        }
    }

    fn handle_deletion(&mut self, path: PathBuf, queue: &mut Vec<Report>) {
        let parent = match path.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        };
        let name = match path.file_name() {
            Some(n) => n.to_os_string(),
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
            // Collect children *before* modifying tracked_dirs so the borrow ends.
            let children: Vec<OsString> = self
                .tracked_dirs
                .get(&path)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            self.tracked_dirs.remove(&path);
            for child in children {
                self.handle_deletion(path.join(&child), queue);
            }
        }

        if was_tracked_file {
            self.tracked_files.remove(&path);
            if self.events.contains(&EventType::Delete) {
                // Defer: wait for a possible same-path Add within the atomic window.
                self.pending_unlinks.insert(path.clone(), now);
            }
        }

        // Remove from parent's child set.
        if let Some(parent_set) = self.tracked_dirs.get_mut(&parent) {
            parent_set.remove(&name);
        }
    }

    fn handle_directory(&mut self, path: PathBuf, queue: &mut Vec<Report>) {
        // Ensure the directory itself is tracked.
        self.tracked_dirs.entry(path.clone()).or_default();
        if path != self.cwd {
            if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                self.tracked_dirs
                    .entry(parent.to_path_buf())
                    .or_default()
                    .insert(name.to_os_string());
            }
        }

        // Snapshot what's currently on disk (filtered).
        let current: HashSet<OsString> = match std::fs::read_dir(&path) {
            Err(_) => return,
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter_map(|entry| {
                    let cp = entry.path();
                    let ft = entry.file_type().ok()?;
                    if ft.is_dir() {
                        if self.matcher.is_ignored(&cp) {
                            None
                        } else {
                            Some(entry.file_name())
                        }
                    } else {
                        if self.matcher.is_emittable(&cp) {
                            Some(entry.file_name())
                        } else {
                            None
                        }
                    }
                })
                .collect(),
        };

        // Snapshot the previously-tracked children.
        let tracked: HashSet<OsString> = self.tracked_dirs.get(&path).cloned().unwrap_or_default();

        // New entries (on disk, not yet tracked) → dispatch each.
        let new_entries: Vec<OsString> = current.difference(&tracked).cloned().collect();
        // Gone entries (tracked, not on disk) → deletion path.
        let gone_entries: Vec<OsString> = tracked.difference(&current).cloned().collect();

        for name in new_entries {
            self.dispatch(&path.join(&name), queue);
        }
        for name in gone_entries {
            self.handle_deletion(path.join(&name), queue);
        }
    }

    fn handle_file(&mut self, path: PathBuf, queue: &mut Vec<Report>) {
        if !self.matcher.is_emittable(&path) {
            return;
        }

        let parent = match path.parent() {
            Some(p) => p.to_path_buf(),
            None => return,
        };
        let name = match path.file_name() {
            Some(n) => n.to_os_string(),
            None => return,
        };

        let was_tracked = self.tracked_files.contains(&path);
        let now = Instant::now();

        if self.pending_unlinks.contains_key(&path) {
            // Atomic write: an unlink was pending for this path and an add arrived.
            // Emit change instead of (delete + create).
            self.pending_unlinks.remove(&path);
            self.tracked_files.insert(path.clone());
            self.tracked_dirs.entry(parent).or_default().insert(name);

            if self.events.contains(&EventType::Change) && self.allow_change(&path, now) {
                queue.push(Report {
                    uid: self.uid,
                    event: EventType::Change,
                    path,
                    timestamp: now,
                });
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
            self.tracked_files.insert(path.clone());
            self.tracked_dirs.entry(parent).or_default().insert(name);

            if self.events.contains(&EventType::Create) {
                queue.push(Report {
                    uid: self.uid,
                    event: EventType::Create,
                    path,
                    timestamp: now,
                });
            }
        }
    }

    fn allow_change(&mut self, path: &Path, now: Instant) -> bool {
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
    /// schedule the next wake-up.
    fn flush_pending(&mut self, now: Instant, queue: &mut Vec<Report>) -> Option<Instant> {
        let window = Duration::from_millis(ATOMIC_WINDOW_MS);
        let mut earliest: Option<Instant> = None;
        let mut to_flush: Vec<(PathBuf, Instant)> = Vec::new();

        for (path, &orig) in &self.pending_unlinks {
            let deadline = orig + window;
            if now >= deadline {
                to_flush.push((path.clone(), orig));
            } else {
                earliest = Some(match earliest {
                    None => deadline,
                    Some(e) => e.min(deadline),
                });
            }
        }

        for (path, orig) in to_flush {
            self.pending_unlinks.remove(&path);
            queue.push(Report {
                uid: self.uid,
                event: EventType::Delete,
                path,
                timestamp: orig,
            });
        }

        earliest
    }
}

type Queue = &'static Mutex<Vec<Report>>;
type States = &'static Mutex<BTreeMap<usize, Arc<Mutex<WatcherState>>>>;

fn create_watcher(
    reg: Register,
    queue: Queue,
    states: States,
) -> notify::Result<notify::RecommendedWatcher> {
    let uid = reg.uid;
    let cwd = path::absolute(&reg.cwd).unwrap_or_else(|_| PathBuf::from(&reg.cwd));
    let matcher = PatternMatcher::new(&cwd, &reg.patterns, &reg.ignores);

    let state = Arc::new(Mutex::new(WatcherState::new(
        uid,
        cwd.clone(),
        reg.events,
        matcher,
    )));

    {
        let mut s = state.lock().unwrap();
        s.initial_scan();
    }

    let state_for_cb = Arc::clone(&state);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let event = match result {
            Ok(e) => e,
            Err(e) => {
                eprintln!("watcher error: {e:?}");
                return;
            }
        };

        let mut local = Vec::new();
        {
            let mut s = state_for_cb.lock().unwrap();
            for path in event.paths {
                s.dispatch(&path, &mut local);
            }
        }

        if !local.is_empty() {
            queue.lock().unwrap().append(&mut local);
        }
    })?;

    watcher.watch(&cwd, notify::RecursiveMode::Recursive)?;
    states.lock().unwrap().insert(uid, state);
    Ok(watcher)
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
                            earliest_deadline = Some(match earliest_deadline {
                                None => ed,
                                Some(e) => e.min(ed),
                            });
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
        let mut path_states: BTreeMap<(usize, PathBuf), (usize, EventType)> = BTreeMap::new();
        for (i, report) in q.into_iter().enumerate() {
            let Report {
                uid, event, path, ..
            } = report;
            match path_states.entry((uid, path)) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert((i, event));
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
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
            let _ = writeln!(stdout, "{}:{}:{}", uid, event, path.display());
        }
        let _ = writeln!(stdout, "<flush>");
    }
}

fn main() {
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    compile_error!("unsupported platform");

    #[cfg(any(target_os = "linux", windows))]
    {
        enter_efficiency_mode();
        drop(thread::spawn(parent_process_watchdog));
    }

    let queue: Queue = Box::leak(Box::new(Mutex::new(Vec::new())));
    let states: States = Box::leak(Box::new(Mutex::new(BTreeMap::new())));
    drop(thread::spawn(move || handle_reports(queue, states)));

    let mut watchers: BTreeMap<usize, notify::RecommendedWatcher> = BTreeMap::new();

    for line in io::stdin().lines() {
        let line = line.expect("failed to read from stdin");
        let request: Request = serde_json::from_str(&line).expect("failed to parse input");

        match request {
            Request::Register(reg) => {
                let uid = reg.uid;
                match watchers.entry(uid) {
                    std::collections::btree_map::Entry::Occupied(_) => {
                        eprintln!("watcher with ID {uid} already exists");
                    }
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        match create_watcher(reg, queue, states) {
                            Ok(w) => {
                                entry.insert(w);
                            }
                            Err(e) => {
                                eprintln!("failed to watch on path: {e:?}");
                            }
                        }
                    }
                }
            }
            Request::Unregister(uid) => {
                // Drop the notify watcher first (stops new event callbacks).
                if watchers.remove(&uid).is_none() {
                    eprintln!("watcher with ID {uid} not found");
                }
                // Then remove the state (any in-flight callback still holds its Arc).
                states.lock().unwrap().remove(&uid);
            }
        }
    }

    exit(0);
}
