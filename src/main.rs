use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::io::{BufWriter, Write};
use std::mem;
use std::path::{self, PathBuf};
use std::process::exit;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, sleep};
use std::time::Duration;

use glob::glob;
use notify::{recommended_watcher, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
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

struct Event {
    uid: usize,
    event: EventType,
    path: String,
}

fn event_listener(receiver: Receiver<Event>) {
    let throttle = Duration::from_millis(400);

    let mut stdout = BufWriter::new(io::stdout());
    loop {
        let mut written = false;
        for event in receiver.try_iter() {
            writeln!(stdout, "{}:{}:{}", event.uid, event.event, event.path).unwrap();
            written = true;
        }
        if written {
            writeln!(stdout, "<flush>").unwrap();
            stdout.flush().unwrap();
        }
        sleep(throttle);
    }
}

fn new_watcher(
    mut config: WatcherConfig,
    sender: Sender<Event>,
) -> notify::Result<RecommendedWatcher> {
    let paths = mem::take(&mut config.patterns);
    let cwd = PathBuf::from(mem::take(&mut config.cwd)).canonicalize()?;
    let cwd2 = cwd.clone();

    let ignores: Vec<_> = mem::take(&mut config.ignores)
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
        .collect();

    let mut watcher = recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };

        let event_type = match event.kind {
            EventKind::Create(_) => EventType::Create,
            EventKind::Modify(_) => EventType::Change,
            EventKind::Remove(_) => EventType::Delete,
            _ => return,
        };

        if !config.events.contains(&event_type) {
            return;
        }

        'outer: for path in event.paths {
            let s = path.to_string_lossy();

            for ignore in ignores.iter() {
                if ignore.matches(s.as_ref()) {
                    continue 'outer;
                }
            }

            let Ok(path) = path.strip_prefix(&cwd2) else {
                continue;
            };

            sender
                .send(Event {
                    uid: config.uid,
                    event: event_type,
                    path: path.to_string_lossy().into_owned(),
                })
                .unwrap()
        }
    })?;

    for path in paths {
        let path = path::absolute(cwd.join(path)).unwrap();
        match glob(path.to_string_lossy().as_ref()) {
            Ok(paths) => {
                for path in paths {
                    match path {
                        Ok(path) => {
                            let s = path.to_string_lossy();
                            if !s.contains("/../")
                                && !s.contains("/./")
                                && !s.ends_with("/..")
                                && !s.ends_with("/.")
                            {
                                if let Err(e) = watcher.watch(&path, RecursiveMode::Recursive) {
                                    eprintln!("failed to watch on path: {e:?}");
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("glob failed: {e:?}")
                        }
                    }
                }
            }
            Err(e) => eprintln!("invalid glob pattern: {e:?}"),
        }
    }

    Ok(watcher)
}

fn main() {
    #[cfg(target_os = "linux")]
    drop(thread::spawn(parent_process_watchdog));

    let (sender, receiver) = mpsc::channel();
    drop(thread::spawn(|| event_listener(receiver)));

    let mut watchers = BTreeMap::new();

    for input in io::stdin().lines() {
        let input = input.expect("failed to read from stdin");
        let request: Request = serde_json::from_str(&input).expect("failed to parse input");

        match request {
            Request::Register(config) => match watchers.entry(config.uid) {
                Entry::Occupied(_) => eprintln!("watcher with ID {} already exists", config.uid),
                Entry::Vacant(entry) => match new_watcher(config, sender.clone()) {
                    Ok(watcher) => {
                        entry.insert(watcher);
                    }
                    Err(e) => eprintln!("failed to create watcher: {e:?}"),
                },
            },
            Request::Unregister(uid) => {
                if watchers.remove(&uid).is_none() {
                    eprintln!("watcher with ID {uid} not found");
                }
            }
        }
    }

    exit(0);
}
