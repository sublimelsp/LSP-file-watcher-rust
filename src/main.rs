use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::io::{BufWriter, Write};
use std::path::{self, PathBuf};
use std::process::exit;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, sleep};
use std::time::Duration;

use notify::{recommended_watcher, Event, EventKind, RecursiveMode, Watcher};
use serde::Deserialize;

#[cfg(target_os = "linux")]
fn parent_process_watchdog() {
    fn parent_died() -> ! {
        eprintln!("parent process died");
        exit(1);
    }

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

#[derive(Debug, Deserialize)]
struct WatcherConfig {
    cwd: String,
    events: Vec<EventType>,
    ignores: Vec<String>,
    patterns: Vec<String>,
    uid: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Request {
    Register(WatcherConfig),
    Unregister(usize),
}

fn event_listener(
    configs: Arc<Mutex<BTreeMap<usize, (PathBuf, impl Fn(EventType, &[PathBuf]) -> Vec<String>)>>>,
    receiver: Receiver<notify::Result<Event>>,
) {
    let throttle = Duration::from_millis(400);

    let mut stdout = BufWriter::new(io::stdout());
    loop {
        let mut written = false;
        for event in receiver.try_iter() {
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    eprintln!("watcher error: {e:?}");
                    continue;
                }
            };

            let event_type = match event.kind {
                EventKind::Create(_) => EventType::Create,
                EventKind::Modify(_) => EventType::Change,
                EventKind::Remove(_) => EventType::Delete,
                _ => continue,
            };

            for (uid, (_, cb)) in configs.lock().unwrap().iter() {
                let paths = cb(event_type, &event.paths);
                for path in paths {
                    writeln!(stdout, "{}:{}:{}", uid, event_type, path).unwrap();
                    written = true;
                }
            }
        }
        if written {
            writeln!(stdout, "<flush>").unwrap();
            stdout.flush().unwrap();
        }
        sleep(throttle);
    }
}

fn new_watcher(config: WatcherConfig) -> (PathBuf, impl Fn(EventType, &[PathBuf]) -> Vec<String>) {
    let cwd = PathBuf::from(config.cwd).canonicalize().unwrap();
    let cwd2 = cwd.clone();

    let paths_to_patterns = |paths: Vec<String>| -> Vec<glob::Pattern> {
        paths
            .into_iter()
            .map(|path| path::absolute(cwd.join(path)).unwrap())
            .filter_map(|path| {
                glob::Pattern::new(path.to_string_lossy().as_ref()).map_or_else(
                    |e| {
                        eprintln!("invalid glob pattern: {e:?}");
                        None
                    },
                    |pat| Some(pat),
                )
            })
            .collect()
    };

    let raw_patterns: Vec<_> = config
        .patterns
        .iter()
        .map(|path| path::absolute(cwd.join(path)).unwrap())
        .collect();
    let patterns = paths_to_patterns(config.patterns);
    let ignores = paths_to_patterns(config.ignores);

    let events = config.events;

    (cwd, move |event, paths| {
        if !events.contains(&event) {
            return Vec::new();
        }

        paths
            .iter()
            .filter_map(|path| {
                if patterns.iter().all(|pattern| !pattern.matches_path(&path))
                    && raw_patterns.iter().all(|raw| !path.starts_with(raw))
                    || ignores.iter().any(|ignore| ignore.matches_path(&path))
                {
                    return None;
                }

                let Ok(path) = path.strip_prefix(&cwd2) else {
                    return None;
                };

                Some(path.to_string_lossy().into_owned())
            })
            .collect()
    })
}

fn main() {
    #[cfg(target_os = "linux")]
    drop(thread::spawn(parent_process_watchdog));

    let configs = Arc::new(Mutex::new(BTreeMap::new()));
    let cconfigs = configs.clone();
    let (sender, receiver) = mpsc::channel();
    drop(thread::spawn(move || event_listener(cconfigs, receiver)));

    let mut watching_path = BTreeMap::new();
    let mut watcher = recommended_watcher(move |event| sender.send(event).unwrap())
        .expect("failed to create watcher");

    for input in io::stdin().lines() {
        let input = input.expect("failed to read from stdin");
        let request: Request = serde_json::from_str(&input).expect("failed to parse input");

        match request {
            Request::Register(config) => match configs.lock().unwrap().entry(config.uid) {
                Entry::Occupied(_) => eprintln!("watcher with ID {} already exists", config.uid),
                Entry::Vacant(entry) => {
                    let (cwd, cb) = new_watcher(config);
                    *watching_path.entry(cwd.clone()).or_insert_with(|| {
                        if let Err(e) = watcher.watch(&cwd, RecursiveMode::Recursive) {
                            eprintln!("failed to watch on path: {e:?}");
                        }
                        0usize
                    }) += 1;
                    entry.insert((cwd, cb));
                }
            },
            Request::Unregister(uid) => {
                if let Some((cwd, _)) = configs.lock().unwrap().remove(&uid) {
                    let count = watching_path.get_mut(&cwd).unwrap();
                    *count -= 1;
                    if *count == 0 {
                        watching_path.remove(&cwd);
                    }
                } else {
                    eprintln!("watcher with ID {uid} not found");
                }
            }
        }
    }

    exit(0);
}
