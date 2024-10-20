use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::io::{BufWriter, Write};
use std::path::{self, PathBuf};
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use glob::Pattern;
use notify_debouncer_full::notify::{self, Watcher};
use notify_debouncer_full::DebounceEventResult;
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
struct RegisterRequest {
    cwd: String,
    events: Vec<EventType>,
    ignores: Vec<String>,
    patterns: Vec<String>,
    uid: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Request {
    Register(RegisterRequest),
    Unregister(usize),
}

#[derive(Debug)]
struct WatcherConfig {
    cwd: PathBuf,
    events: Vec<EventType>,
    ignores: Vec<Pattern>,
    patterns: Vec<Pattern>,
    prefixes: Vec<PathBuf>,
}

impl WatcherConfig {
    fn from_request(req: RegisterRequest) -> WatcherConfig {
        let cwd = PathBuf::from(req.cwd).canonicalize().unwrap();

        let paths_to_patterns = |paths: Vec<String>| -> Vec<Pattern> {
            paths
                .into_iter()
                .map(|path| path::absolute(cwd.join(path)).unwrap())
                .filter_map(|path| {
                    Pattern::new(path.to_string_lossy().as_ref()).map_or_else(
                        |e| {
                            eprintln!("invalid glob pattern: {e:?}");
                            None
                        },
                        |pat| Some(pat),
                    )
                })
                .collect()
        };

        let prefixes: Vec<_> = req
            .patterns
            .iter()
            .map(|path| path::absolute(cwd.join(path)).unwrap())
            .collect();
        let patterns = paths_to_patterns(req.patterns);
        let ignores = paths_to_patterns(req.ignores);

        let events = req.events;

        WatcherConfig {
            cwd,
            events,
            ignores,
            patterns,
            prefixes,
        }
    }
}

fn normalize_events(events: &mut Vec<notify::Event>) {
    use notify::event::{CreateKind, EventAttributes, ModifyKind, RemoveKind, RenameMode};
    use notify::{Event, EventKind};

    let mut i = 0;
    while i < events.len() {
        let event = &mut events[i];
        if let EventKind::Modify(ModifyKind::Name(rename)) = event.kind {
            match rename {
                RenameMode::From => {
                    event.kind = EventKind::Remove(RemoveKind::Any);
                }
                RenameMode::To => {
                    event.kind = EventKind::Create(CreateKind::Any);
                    let paths = event.paths.clone();
                    events.insert(
                        i + 1,
                        Event {
                            kind: EventKind::Modify(ModifyKind::Any),
                            paths,
                            attrs: EventAttributes::new(),
                        },
                    )
                }
                RenameMode::Both => {
                    assert_eq!(event.paths.len(), 2);
                    event.kind = EventKind::Remove(RemoveKind::Any);
                    let dest = event.paths.pop().unwrap();
                    events.insert(
                        i + 1,
                        Event {
                            kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                            paths: vec![dest],
                            attrs: EventAttributes::new(),
                        },
                    )
                }
                _ => (),
            }
        }
        i += 1;
    }
}

fn event_handler(configs: Arc<Mutex<BTreeMap<usize, WatcherConfig>>>, events: DebounceEventResult) {
    let mut events = match events {
        Ok(events) => events.into_iter().map(|event| event.event).collect(),
        Err(errors) => {
            for e in errors {
                eprintln!("watcher error: {e:?}");
            }
            return;
        }
    };

    normalize_events(&mut events);

    let mut stdout = BufWriter::new(io::stdout().lock());
    let mut written = false;
    for event in events {
        let event_type = match event.kind {
            notify::EventKind::Create(_) => EventType::Create,
            notify::EventKind::Modify(_) => EventType::Change,
            notify::EventKind::Remove(_) => EventType::Delete,
            _ => continue,
        };

        for (uid, config) in configs.lock().unwrap().iter() {
            if !config.events.contains(&event_type) {
                continue;
            }

            for path in event.paths.iter() {
                if config
                    .patterns
                    .iter()
                    .all(|pattern| !pattern.matches_path(&path))
                    && config
                        .prefixes
                        .iter()
                        .all(|prefix| !path.starts_with(prefix))
                    || config
                        .ignores
                        .iter()
                        .any(|ignore| ignore.matches_path(&path))
                {
                    continue;
                }

                let Ok(path) = path.strip_prefix(&config.cwd) else {
                    continue;
                };

                let path = path.to_string_lossy();

                writeln!(stdout, "{}:{}:{}", uid, event_type, path.as_ref()).unwrap();
                written = true;
            }
        }
    }
    if written {
        writeln!(stdout, "<flush>").unwrap();
        stdout.flush().unwrap();
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    drop(std::thread::spawn(parent_process_watchdog));

    let configs = Arc::new(Mutex::new(BTreeMap::new()));
    let configs_clone = configs.clone();
    let mut watching_path = BTreeMap::new();
    let mut watcher =
        notify_debouncer_full::new_debouncer(Duration::from_millis(400), None, move |events| {
            event_handler(configs_clone.clone(), events)
        })
        .expect("failed to create watcher");

    for input in io::stdin().lines() {
        let input = input.expect("failed to read from stdin");
        let request: Request = serde_json::from_str(&input).expect("failed to parse input");

        match request {
            Request::Register(req) => match configs.lock().unwrap().entry(req.uid) {
                Entry::Occupied(_) => eprintln!("watcher with ID {} already exists", req.uid),
                Entry::Vacant(entry) => {
                    let config = WatcherConfig::from_request(req);
                    if let Some(count) = watching_path.get_mut(&config.cwd) {
                        *count += 1;
                    } else {
                        if let Err(e) = watcher
                            .watcher()
                            .watch(&config.cwd, notify::RecursiveMode::Recursive)
                        {
                            eprintln!("failed to watch on path: {e:?}");
                            continue;
                        }
                        watching_path.insert(config.cwd.clone(), 1usize);
                    }
                    entry.insert(config);
                }
            },
            Request::Unregister(uid) => {
                if let Some(config) = configs.lock().unwrap().remove(&uid) {
                    let count = watching_path.get_mut(&config.cwd).unwrap();
                    *count -= 1;
                    if *count == 0 {
                        watching_path.remove(&config.cwd);
                    }
                } else {
                    eprintln!("watcher with ID {uid} not found");
                }
            }
        }
    }

    exit(0);
}
