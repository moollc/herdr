//! Bounded native notifications, owned entirely by a worker rather than the TUI.
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::{is_regular_file, RecentFile, MAX_RESULTS};

pub(super) const INTERVAL: Duration = Duration::from_millis(150);
const MAX_ROOTS: usize = 64;
const MAX_DIRECTORIES: usize = 512;
const MAX_ENTRIES: usize = 20_000;
const MAX_EVENTS: usize = 1024;
const MAX_EVENT_PATHS: usize = 64;
const MAX_PATH_BYTES: usize = 4096;
const PARTIAL: &str = "File watcher partial, a watch or event limit was reached";

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Roots {
    paths: Vec<PathBuf>,
    overflow: bool,
    processes: Vec<u32>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Activity {
    pub files: Vec<RecentFile>,
    pub error: Option<String>,
    pub truncated: bool,
}

pub(super) struct FileSystemWatcher {
    commands: Option<SyncSender<Roots>>,
    updates: Option<Receiver<Activity>>,
    sent: Option<Roots>,
    pub activity: Activity,
    state_dir: PathBuf,
    extra: Vec<PathBuf>,
    extra_overflow: bool,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    changed: bool,
}

impl FileSystemWatcher {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            commands: None,
            updates: None,
            sent: None,
            activity: Activity::default(),
            state_dir,
            extra: Vec::new(),
            extra_overflow: false,
            wake: None,
            changed: false,
        }
    }

    pub fn set_wake(&mut self, wake: impl Fn() + Send + Sync + 'static) {
        self.wake = Some(Arc::new(wake));
    }

    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    pub fn active(&self) -> bool {
        self.commands.is_some()
    }

    pub fn set_extra(&mut self, paths: &[String]) {
        self.extra_overflow = paths.len() > MAX_ROOTS;
        self.extra = paths
            .iter()
            .take(MAX_ROOTS)
            .map(|path| {
                path.strip_prefix("~/")
                    .and_then(|suffix| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(suffix))
                    })
                    .unwrap_or_else(|| path.into())
            })
            .collect();
    }

    /// No filesystem access, waiting, or unbounded queue work on the UI thread.
    pub fn sync(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        processes: impl IntoIterator<Item = u32>,
    ) {
        let mut roots = Roots::default();
        for path in paths.into_iter().chain(self.extra.iter().cloned()) {
            if path.as_os_str().len() > MAX_PATH_BYTES {
                roots.overflow = true;
            } else if !roots.paths.contains(&path) {
                if roots.paths.len() == MAX_ROOTS {
                    roots.overflow = true;
                    break;
                }
                roots.paths.push(path);
            }
        }
        for pid in processes
            .into_iter()
            .filter(|pid| *pid > 0)
            .take(MAX_ROOTS + 1)
        {
            if roots.processes.len() == MAX_ROOTS {
                roots.overflow = true;
                break;
            }
            roots.processes.push(pid);
        }
        roots.processes.sort();
        roots.processes.dedup();
        roots.overflow |= self.extra_overflow;
        roots.paths.sort();
        if self.commands.is_none()
            && self.sent.is_none()
            && (!roots.paths.is_empty() || !roots.processes.is_empty())
        {
            let (command_tx, command_rx) = mpsc::sync_channel(1);
            let (update_tx, update_rx) = mpsc::sync_channel(1);
            let state_dir = self.state_dir.clone();
            let wake = self.wake.clone();
            match std::thread::Builder::new()
                .name("recent-files".into())
                .spawn(move || run(command_rx, update_tx, state_dir, wake))
            {
                Ok(_) => {
                    self.commands = Some(command_tx);
                    self.updates = Some(update_rx);
                }
                Err(_) => {
                    self.activity.error = Some("File watcher could not start".into());
                    self.sent = Some(roots.clone()); // Fail visibly, without a spawn retry loop.
                }
            }
        }
        if self.sent.as_ref() != Some(&roots) {
            if let Some(commands) = &self.commands {
                match commands.try_send(roots.clone()) {
                    Ok(()) => self.sent = Some(roots),
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => {
                        self.activity.error = Some("File watcher stopped".into());
                        self.sent = Some(roots);
                        self.commands = None;
                    }
                }
            }
        }
        if let Some(updates) = &self.updates {
            // Capacity one; never drain a producer-fed unbounded loop.
            match updates.try_recv() {
                Ok(activity) => {
                    self.changed |= self.activity != activity;
                    self.activity = activity;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.activity.error = Some("File watcher stopped".into());
                }
            }
        }
    }
}

struct Tree {
    roots: Vec<PathBuf>,
    directories: HashSet<PathBuf>,
    root_streams: bool,
    subscriptions: HashSet<PathBuf>,
    state_dir: PathBuf,
    library: Option<PathBuf>,
    home: Option<PathBuf>,
    entries: usize,
    activity: Activity,
}

impl Tree {
    fn new(state_dir: PathBuf) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self {
            roots: Vec::new(),
            directories: HashSet::new(),
            root_streams: false,
            subscriptions: HashSet::new(),
            state_dir: fs::canonicalize(&state_dir).unwrap_or(state_dir),
            library: home.as_ref().map(|home| home.join("Library")),
            home: home.and_then(|home| fs::canonicalize(home).ok()),
            entries: 0,
            activity: Activity::default(),
        }
    }

    fn partial(&mut self) {
        self.activity.error = Some(PARTIAL.into());
    }

    fn ignored(&self, path: &Path) -> bool {
        ignored_path(path, &self.state_dir, self.library.as_deref())
    }

    /// FSEvents already has a recursive stream. Changing its paths restarts
    /// that stream, so only root changes may touch native registrations.
    fn sync_streams(&mut self, watcher: &mut impl Watcher) -> bool {
        let desired: HashSet<_> = self.roots.iter().cloned().collect();
        if desired == self.subscriptions {
            return true;
        }
        let mut paths = watcher.paths_mut();
        let mut ok = true;
        for path in self.subscriptions.difference(&desired) {
            ok &= paths.remove(path).is_ok();
        }
        for path in desired.difference(&self.subscriptions) {
            ok &= paths.add(path, RecursiveMode::Recursive).is_ok();
        }
        ok &= paths.commit().is_ok();
        if !ok {
            self.activity.error = Some("File watcher could not update native roots".into());
            return false; // Drop the backend, releasing even partially applied registrations.
        }
        self.subscriptions = desired;
        true
    }

    fn reset<W: Watcher>(&mut self, request: &Roots, watcher: &mut W, force: bool) -> bool {
        self.root_streams = W::kind() == notify::WatcherKind::Fsevent;
        let mut roots = Vec::new();
        let mut error = request.overflow.then(|| PARTIAL.into());
        let mut paths = request.paths.clone();
        // Native process queries are bounded and off the TUI thread. They select
        // watch locations only; neither a cwd nor a PID proves file authorship.
        let mut probes = 0;
        for pid in &request.processes {
            let job = crate::platform::foreground_job(*pid);
            // A wrapper may lead the process group while its agent child has
            // changed cwd. Include members, as Herdr's pane cwd resolution does.
            let members = job
                .into_iter()
                .flat_map(|job| job.processes.into_iter().map(|process| process.pid));
            if !extend_process_roots(
                &mut paths,
                std::iter::once(*pid).chain(members),
                &mut probes,
                crate::platform::process_cwd,
            ) {
                error = Some(PARTIAL.into());
                break;
            }
        }
        for path in paths {
            match fs::canonicalize(&path) {
                Ok(path)
                    if path.is_dir()
                        && path.parent().is_some()
                        && !self
                            .home
                            .as_ref()
                            .is_some_and(|home| home.starts_with(&path))
                        && !self.ignored(&path) =>
                {
                    if !roots.contains(&path) {
                        roots.push(path);
                    }
                }
                _ => error = Some("File watcher skipped an unsafe or unavailable root".into()),
            }
        }
        roots.sort();
        // Avoid redundant subscriptions for overlapping roots, including aliases.
        let mut minimal: Vec<PathBuf> = Vec::new();
        for path in roots {
            if !minimal.iter().any(|root| path.starts_with(root)) {
                minimal.push(path);
            }
        }
        // A subscription update cannot reconstruct lost history. Keep loss
        // visible even when a new process/query limit leaves the root set unchanged.
        if error.is_some() {
            self.activity.error = error;
        }
        if !force && self.roots == minimal {
            return true;
        }
        self.roots = minimal;
        self.entries = 0;
        if self.root_streams && !self.sync_streams(watcher) {
            return false;
        }
        // Differential removal retains subscriptions shared by multiple workspaces.
        let removed: Vec<_> = self
            .directories
            .iter()
            .filter(|path| !self.roots.iter().any(|root| path.starts_with(root)))
            .cloned()
            .collect();
        for path in removed {
            if !self.root_streams && watcher.unwatch(&path).is_err() {
                self.activity.error = Some("File watcher could not release a watch".into());
                return false; // Drop the backend, rather than leaking registrations.
            }
            self.directories.remove(&path);
        }
        // Canonical roots, not caller-supplied path prefixes, establish membership.
        for root in self.roots.clone() {
            self.discover(root, watcher, false);
        }
        true
    }

    /// Pruned discovery is bounded on every backend. Non-stream backends retain
    /// a hard per-directory watch budget; recursive streams register only roots.
    fn discover(&mut self, start: PathBuf, watcher: &mut impl Watcher, new: bool) {
        let mut queue = VecDeque::from([start]);
        while let Some(path) = queue.pop_front() {
            if self.ignored(&path) || self.directories.contains(&path) {
                continue;
            }
            if fs::canonicalize(&path).ok().as_ref() != Some(&path)
                || !self.roots.iter().any(|root| path.starts_with(root))
            {
                self.partial();
                continue;
            }
            if self.directories.len() >= MAX_DIRECTORIES {
                self.partial();
                break;
            }
            if !self.root_streams && watcher.watch(&path, RecursiveMode::NonRecursive).is_err() {
                self.activity.error = Some("File watcher could not watch a directory".into());
                continue;
            }
            self.directories.insert(path.clone());
            let Ok(entries) = fs::read_dir(path) else {
                self.activity.error = Some("File watcher could not read a directory".into());
                continue;
            };
            for entry in entries {
                if self.entries >= MAX_ENTRIES {
                    self.partial();
                    return;
                }
                self.entries += 1;
                let Ok(entry) = entry else {
                    self.partial();
                    continue;
                };
                let path = entry.path();
                if path.as_os_str().len() > MAX_PATH_BYTES {
                    self.partial();
                    continue;
                }
                if self.ignored(&path) {
                    continue;
                }
                let Ok(kind) = entry.file_type() else {
                    self.partial();
                    continue;
                };
                if kind.is_dir() {
                    if queue.len() + self.directories.len() < MAX_DIRECTORIES {
                        queue.push_back(path);
                    } else {
                        self.partial();
                    }
                } else if kind.is_symlink() {
                    self.partial(); // Never follow loops or silently omit coverage.
                } else if new
                    && kind.is_file()
                    && fs::canonicalize(&path).ok().as_ref() == Some(&path)
                {
                    self.record(path, SystemTime::now());
                }
            }
        }
    }

    fn record(&mut self, path: PathBuf, time: SystemTime) {
        self.activity.files.retain(|file| file.path != path);
        self.activity.files.insert(
            0,
            RecentFile {
                path,
                modified: time,
                agent: String::new(),
            },
        );
        self.activity.truncated |= self.activity.files.len() > MAX_RESULTS;
        self.activity.files.truncate(MAX_RESULTS);
    }

    fn event(&mut self, event: Event, time: SystemTime, watcher: &mut impl Watcher) {
        if event.need_rescan() {
            self.partial();
        }
        if matches!(
            event.kind,
            EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
        ) {
            // Free registrations for removed or moved directories, including descendants.
            for path in event
                .paths
                .iter()
                .take(MAX_EVENT_PATHS)
                .filter(|path| !path.exists())
            {
                let removed: Vec<_> = self
                    .directories
                    .iter()
                    .filter(|dir| dir.starts_with(path))
                    .cloned()
                    .collect();
                for dir in removed {
                    if self.root_streams {
                        self.directories.remove(&dir);
                        continue;
                    }
                    match watcher.unwatch(&dir) {
                        Ok(()) => {
                            self.directories.remove(&dir);
                        }
                        Err(error) if matches!(error.kind, notify::ErrorKind::WatchNotFound) => {
                            self.directories.remove(&dir);
                        }
                        Err(_) => self.partial(), // Keep failed releases in the watch budget.
                    }
                }
            }
        }
        if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
            return; // Access/read notifications never count as writes.
        }
        for path in event.paths.into_iter().take(MAX_EVENT_PATHS) {
            if self.ignored(&path) {
                continue;
            }
            let Ok(canonical) = fs::canonicalize(&path) else {
                self.partial(); // A create/modify vanished before it could be observed.
                continue;
            };
            if canonical != path {
                self.partial(); // Symlink coverage is deliberately excluded.
                continue;
            }
            if self.ignored(&canonical)
                || !self.roots.iter().any(|root| canonical.starts_with(root))
            {
                continue; // Do not follow symlink escapes, even in a fabricated event.
            }
            if is_regular_file(&canonical) {
                self.record(canonical, time);
            } else if canonical.is_dir() {
                self.discover(canonical, watcher, true);
            }
        }
    }
}

fn ignored_path(path: &Path, state_dir: &Path, library: Option<&Path>) -> bool {
    path.starts_with(state_dir)
        || library.is_some_and(|library| path.starts_with(library))
        || path.components().any(|part| {
            let name = part.as_os_str().to_string_lossy();
            matches!(&*name, ".git" | "target" | "node_modules" | ".DS_Store")
                || name.ends_with(".lock")
                || name.ends_with(".swp")
                || name.ends_with(".swo")
                || name.ends_with('~')
                || name.starts_with(".#")
                || (name.starts_with('#') && name.ends_with('#'))
        })
}

/// Recursive streams also report ignored build trees. Discard those paths
/// before the bounded queue so a build cannot crowd out useful notifications.
fn forward(
    sender: &SyncSender<Notification>,
    overflow: &AtomicBool,
    result: notify::Result<Event>,
    state_dir: &Path,
    library: Option<&Path>,
) {
    let result = match result {
        Ok(mut event) => {
            let had_paths = !event.paths.is_empty();
            if event.paths.len() > MAX_EVENT_PATHS {
                overflow.store(true, Ordering::Relaxed);
                event.paths.truncate(MAX_EVENT_PATHS);
            }
            event.paths.retain(|path| {
                if path.as_os_str().len() > MAX_PATH_BYTES {
                    overflow.store(true, Ordering::Relaxed);
                    false
                } else {
                    !ignored_path(path, state_dir, library)
                }
            });
            if event.paths.is_empty() && (had_paths || !event.need_rescan()) {
                return;
            }
            Ok(event)
        }
        Err(error) => Err(error),
    };
    enqueue(sender, overflow, result);
}

/// Bound cwd queries across all pane groups, not separately for each wrapper.
fn extend_process_roots(
    paths: &mut Vec<PathBuf>,
    processes: impl IntoIterator<Item = u32>,
    probes: &mut usize,
    cwd: impl Fn(u32) -> Option<PathBuf>,
) -> bool {
    for pid in processes {
        if *probes >= MAX_ROOTS {
            return false;
        }
        *probes += 1;
        if let Some(path) = cwd(pid) {
            if paths.contains(&path) {
                continue;
            }
            if paths.len() >= MAX_ROOTS || path.as_os_str().len() > MAX_PATH_BYTES {
                return false;
            }
            paths.push(path);
        }
    }
    true
}

type Notification = (Result<Event, ()>, SystemTime);

fn enqueue(
    sender: &SyncSender<Notification>,
    overflow: &AtomicBool,
    result: notify::Result<Event>,
) {
    // Bound both payload and queue. The OS callback never blocks behind the TUI.
    let result = result
        .map(|mut event| {
            if event.paths.len() > MAX_EVENT_PATHS {
                overflow.store(true, Ordering::Relaxed);
                event.paths.truncate(MAX_EVENT_PATHS);
            }
            event.paths.retain(|path| {
                let valid = path.as_os_str().len() <= MAX_PATH_BYTES;
                if !valid {
                    overflow.store(true, Ordering::Relaxed);
                }
                valid
            });
            event
        })
        .map_err(|_| ());
    if sender.try_send((result, SystemTime::now())).is_err() {
        overflow.store(true, Ordering::Relaxed);
    }
}

fn run(
    commands: Receiver<Roots>,
    updates: SyncSender<Activity>,
    state_dir: PathBuf,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let (events_tx, events_rx) = mpsc::sync_channel(MAX_EVENTS);
    let overflow = Arc::new(AtomicBool::new(false));
    let dropped = Arc::clone(&overflow);
    let mut tree = Tree::new(state_dir);
    let ignored_state = tree.state_dir.clone();
    let ignored_library = tree.library.clone();
    let watcher = RecommendedWatcher::new(
        move |result: notify::Result<Event>| {
            forward(
                &events_tx,
                &dropped,
                result,
                &ignored_state,
                ignored_library.as_deref(),
            );
        },
        notify::Config::default().with_follow_symlinks(false),
    );
    let Ok(mut watcher) = watcher else {
        tree.activity.error = Some("File watcher could not start".into());
        let _ = updates.send(tree.activity);
        if let Some(wake) = &wake {
            wake();
        }
        return;
    };
    let mut published = None;
    let mut request = Roots::default();
    let mut next_roots = Instant::now();
    loop {
        let start = Instant::now();
        let mut force = false;
        match commands.recv_timeout(INTERVAL) {
            Ok(roots) => {
                request = roots;
                force = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if force || start >= next_roots {
            if !tree.reset(&request, &mut watcher, force) {
                let _ = updates.try_send(tree.activity);
                if let Some(wake) = &wake {
                    wake();
                }
                return;
            }
            next_roots = Instant::now() + Duration::from_secs(1);
        }
        let batch_start = Instant::now();
        for (event, time) in events_rx.try_iter().take(MAX_EVENTS) {
            match event {
                Ok(event) => tree.event(event, time, &mut watcher),
                Err(()) => tree.activity.error = Some("File watcher lost filesystem events".into()),
            }
            if batch_start.elapsed() >= Duration::from_millis(20) {
                break; // Leave queued work for the next coalescing interval.
            }
        }
        if overflow.swap(false, Ordering::Relaxed) {
            tree.partial();
        }
        if published.as_ref() != Some(&tree.activity) {
            match updates.try_send(tree.activity.clone()) {
                Err(TrySendError::Disconnected(_)) => break,
                Err(TrySendError::Full(_)) => {} // Retry after the consumer drains the old snapshot.
                Ok(()) => {
                    // Keep the first snapshot as a readiness barrier, but the
                    // consumer already holds empty activity. Waking for that
                    // snapshot would allocate a redundant full server frame.
                    let changed = published.is_some() || tree.activity != Activity::default();
                    published = Some(tree.activity.clone());
                    if changed {
                        if let Some(wake) = &wake {
                            wake();
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RenameMode};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-fs-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn roots(&self) -> Roots {
            Roots {
                paths: vec![self.0.clone()],
                ..Roots::default()
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Synthetic fixtures only, never user files.
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn backend() -> RecommendedWatcher {
        notify::recommended_watcher(|_: notify::Result<Event>| {}).unwrap()
    }
    fn tree(fixture: &Fixture) -> Tree {
        Tree::new(fixture.0.join("state"))
    }
    fn write_event(path: &Path) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Any,
        )))
        .add_path(path.into())
    }
    fn wait_for(
        watcher: &mut FileSystemWatcher,
        roots: &[PathBuf],
        condition: impl Fn(&Activity) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            watcher.sync(roots.iter().cloned(), []);
            if condition(&watcher.activity) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "notification missed one-second deadline: {:?}",
                watcher.activity.error
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn real_native_writes_move_a_then_b_then_a_within_one_second() {
        let fixture = Fixture::new();
        let roots = vec![fixture.0.clone()];
        let mut watcher = FileSystemWatcher::new(fixture.0.join("state"));
        watcher.sync(roots.clone(), []);
        // The worker's first update is a subscription readiness barrier.
        let deadline = Instant::now() + Duration::from_secs(2);
        while watcher.updates.as_ref().unwrap().try_recv().is_err() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let a = fixture.0.join("A.txt");
        let b = fixture.0.join("B.txt");
        fs::write(&a, "first").unwrap();
        wait_for(&mut watcher, &roots, |activity| {
            activity.files.first().is_some_and(|f| f.path == a)
        });
        fs::write(&b, "second").unwrap();
        wait_for(&mut watcher, &roots, |activity| {
            activity.files.len() == 2 && activity.files[0].path == b && activity.files[1].path == a
        });
        fs::write(&a, "again").unwrap();
        wait_for(&mut watcher, &roots, |activity| {
            activity.files.len() == 2 && activity.files[0].path == a && activity.files[1].path == b
        });
        assert!(
            watcher.activity.error.is_none(),
            "{:?}",
            watcher.activity.error
        );
    }

    #[test]
    fn roots_open_close_overlap_aliases_and_unsafe_paths_release_watches() {
        let fixture = Fixture::new();
        let nested = fixture.0.join("nested");
        fs::create_dir(&nested).unwrap();
        let mut tree = tree(&fixture);
        let mut backend = backend();
        assert!(tree.reset(
            &Roots {
                paths: vec![nested.clone(), fixture.0.clone(), nested.join("..")],
                ..Roots::default()
            },
            &mut backend,
            true
        ));
        assert_eq!(tree.roots, vec![fixture.0.clone()]);
        assert_eq!(tree.directories.len(), 2);
        assert!(tree.reset(
            &Roots {
                paths: vec![nested.clone()],
                ..Roots::default()
            },
            &mut backend,
            true
        ));
        assert_eq!(tree.directories, HashSet::from([nested]));
        assert!(tree.reset(&Roots::default(), &mut backend, true));
        assert!(tree.directories.is_empty());
        let mut paths = vec![PathBuf::from("/"), fixture.0.join("missing")];
        paths.extend(tree.home.clone());
        assert!(tree.reset(
            &Roots {
                paths,
                ..Roots::default()
            },
            &mut backend,
            true
        ));
        assert!(tree.roots.is_empty());
        assert!(tree.activity.error.is_some());
    }

    #[test]
    fn ignore_rules_prune_subscriptions_and_forged_symlink_events() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let mut tree = tree(&fixture);
        let names = [".git", "target", "node_modules", "state", "ignored.lock"];
        for name in names {
            fs::create_dir(fixture.0.join(name)).unwrap();
        }
        let mut backend = backend();
        tree.reset(&fixture.roots(), &mut backend, true);
        assert_eq!(tree.directories.len(), 1);
        for name in [".DS_Store", "x.lock", "x.swp", "x.swo", "x~", ".#x", "#x#"] {
            let path = fixture.0.join(name);
            fs::write(&path, "ignored").unwrap();
            tree.event(write_event(&path), SystemTime::now(), &mut backend);
        }
        let external = outside.0.join("external");
        fs::write(&external, "external").unwrap();
        std::os::unix::fs::symlink(&outside.0, fixture.0.join("escape")).unwrap();
        tree.event(
            write_event(&fixture.0.join("escape/external")),
            SystemTime::now(),
            &mut backend,
        );
        tree.event(write_event(&external), SystemTime::now(), &mut backend);
        assert!(tree.activity.files.is_empty());
        if let Some(library) = &tree.library {
            assert!(tree.ignored(&library.join("output.txt")));
        }
    }

    #[test]
    fn burst_coalesces_deduplicates_caps_and_keeps_last_receipt_order() {
        let fixture = Fixture::new();
        let mut tree = tree(&fixture);
        let mut backend = backend();
        tree.reset(&fixture.roots(), &mut backend, true);
        let path = fixture.0.join("repeat");
        fs::write(&path, "fixture").unwrap();
        let time = SystemTime::now();
        for _ in 0..10_000 {
            tree.record(path.clone(), time);
        }
        assert_eq!(tree.activity.files.len(), 1);
        for index in 0..MAX_RESULTS + 20 {
            tree.record(fixture.0.join(index.to_string()), time);
        }
        tree.record(path.clone(), time);
        assert_eq!(tree.activity.files.len(), MAX_RESULTS);
        assert!(tree.activity.truncated);
        assert_eq!(tree.activity.files[0].path, path);
        assert!(tree.activity.files.iter().all(|file| file.agent.is_empty()));
    }

    #[test]
    fn directory_creation_and_rename_discover_children_without_symlink_traversal() {
        let fixture = Fixture::new();
        let mut tree = tree(&fixture);
        let mut backend = backend();
        tree.reset(&fixture.roots(), &mut backend, true);
        let old = fixture.0.join("old");
        let new = fixture.0.join("new");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("child"), "fixture").unwrap();
        tree.event(
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(old.clone()),
            SystemTime::now(),
            &mut backend,
        );
        assert!(tree.directories.contains(&old));
        assert_eq!(tree.activity.files[0].path, old.join("child"));
        fs::rename(&old, &new).unwrap();
        tree.event(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(old.clone())
                .add_path(new.clone()),
            SystemTime::now(),
            &mut backend,
        );
        assert!(!tree.directories.contains(&old));
        assert!(tree.directories.contains(&new));
        assert_eq!(tree.activity.files[0].path, new.join("child"));
    }

    #[test]
    fn directory_budget_is_bounded_and_reported_in_the_panel_activity() {
        let fixture = Fixture::new();
        for index in 0..MAX_DIRECTORIES + 10 {
            fs::create_dir(fixture.0.join(index.to_string())).unwrap();
        }
        let mut tree = tree(&fixture);
        tree.reset(&fixture.roots(), &mut backend(), true);
        assert!(tree.directories.len() <= MAX_DIRECTORIES);
        assert!(tree.activity.error.is_some());
    }

    #[test]
    fn root_limit_loss_is_visible_even_when_effective_roots_do_not_change() {
        let fixture = Fixture::new();
        let mut tree = tree(&fixture);
        let mut backend = backend();
        let mut request = fixture.roots();
        tree.reset(&request, &mut backend, true);
        assert!(tree.activity.error.is_none());
        request.overflow = true;
        tree.reset(&request, &mut backend, false);
        assert!(tree.activity.error.is_some());
    }

    #[test]
    fn wrapper_member_cwds_are_included_and_share_one_global_probe_budget() {
        let mut paths = vec![PathBuf::from("/workspace")];
        let mut probes = 0;
        let cwd = |pid| {
            Some(PathBuf::from(if pid == 3 {
                "/agent-output"
            } else {
                "/workspace"
            }))
        };
        assert!(extend_process_roots(
            &mut paths,
            [1, 2, 3],
            &mut probes,
            cwd
        ));
        assert_eq!(
            paths,
            vec![PathBuf::from("/workspace"), PathBuf::from("/agent-output")]
        );
        assert_eq!(probes, 3);
        assert!(!extend_process_roots(&mut paths, 4..1000, &mut probes, cwd));
        assert_eq!(probes, MAX_ROOTS);
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn callback_flood_bounds_queue_payload_and_reports_dropped_events() {
        let (sender, receiver) = mpsc::sync_channel(MAX_EVENTS);
        let overflow = AtomicBool::new(false);
        for _ in 0..MAX_EVENTS + 200 {
            enqueue(&sender, &overflow, Ok(Event::new(EventKind::Other)));
        }
        assert!(overflow.swap(false, Ordering::Relaxed));
        assert_eq!(receiver.try_iter().count(), MAX_EVENTS);
        let mut oversized = Event::new(EventKind::Other);
        oversized.paths = (0..MAX_EVENT_PATHS + 50)
            .map(|index| PathBuf::from(format!("/{index}")))
            .collect();
        enqueue(&sender, &overflow, Ok(oversized));
        assert_eq!(
            receiver.try_recv().unwrap().0.unwrap().paths.len(),
            MAX_EVENT_PATHS
        );
        assert!(overflow.swap(false, Ordering::Relaxed));
        enqueue(
            &sender,
            &overflow,
            Ok(write_event(Path::new(&"x".repeat(MAX_PATH_BYTES + 1)))),
        );
        assert!(receiver.try_recv().unwrap().0.unwrap().paths.is_empty());
        assert!(overflow.load(Ordering::Relaxed));
    }

    #[test]
    fn discovery_entry_budget_stops_visibly_without_rescanning_on_idle() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("one"), "fixture").unwrap();
        let mut tree = tree(&fixture);
        tree.roots = vec![fixture.0.clone()];
        tree.entries = MAX_ENTRIES;
        let mut backend = backend();
        tree.discover(fixture.0.clone(), &mut backend, false);
        assert_eq!(tree.entries, MAX_ENTRIES);
        assert!(tree.activity.error.is_some());
        let entries = tree.entries;
        tree.reset(&fixture.roots(), &mut backend, false);
        assert_eq!(tree.entries, entries);
    }

    #[test]
    fn payload_and_root_budgets_fail_visibly_without_unbounded_requests() {
        let fixture = Fixture::new();
        let mut watcher = FileSystemWatcher::new(fixture.0.join("state"));
        let paths: Vec<_> = (0..MAX_ROOTS + 100)
            .map(|i| fixture.0.join(i.to_string()))
            .collect();
        watcher.sync(paths, []);
        assert!(watcher.sent.as_ref().unwrap().overflow);
        assert_eq!(watcher.sent.as_ref().unwrap().paths.len(), MAX_ROOTS);
        let mut tree = tree(&fixture);
        let mut backend = backend();
        tree.reset(&fixture.roots(), &mut backend, true);
        let mut event = Event::new(EventKind::Other);
        event.attrs.set_flag(notify::event::Flag::Rescan);
        tree.event(event, SystemTime::now(), &mut backend);
        assert!(tree.activity.error.is_some());
    }
    #[derive(Default)]
    struct CountingWatcher {
        adds: Vec<(PathBuf, RecursiveMode)>,
        removes: Vec<PathBuf>,
    }

    impl Watcher for CountingWatcher {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Ok(Self::default())
        }

        fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            self.adds.push((path.into(), mode));
            Ok(())
        }

        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.removes.push(path.into());
            Ok(())
        }

        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::Fsevent
        }
    }

    #[test]
    fn safety_regression_native_stream_is_stable_through_directory_churn() {
        let fixture = Fixture::new();
        let mut tree = tree(&fixture);
        let mut watcher = CountingWatcher::default();
        assert!(tree.reset(&fixture.roots(), &mut watcher, true));
        let nested = fixture.0.join("nested");
        fs::create_dir(&nested).unwrap();
        for index in 0..MAX_DIRECTORIES + 20 {
            fs::create_dir(nested.join(index.to_string())).unwrap();
        }
        tree.event(
            Event::new(EventKind::Create(CreateKind::Folder)).add_path(nested.clone()),
            SystemTime::now(),
            &mut watcher,
        );
        assert!(
            tree.activity.error.is_some(),
            "discovery cap must remain visible"
        );
        assert_eq!(
            watcher.adds,
            vec![(fixture.0.clone(), RecursiveMode::Recursive)]
        );
        fs::remove_dir_all(&nested).unwrap();
        tree.event(
            Event::new(EventKind::Remove(notify::event::RemoveKind::Folder)).add_path(nested),
            SystemTime::now(),
            &mut watcher,
        );
        assert!(
            watcher.removes.is_empty(),
            "directory deletion must not restart a root stream"
        );
        assert!(tree.reset(&Roots::default(), &mut watcher, true));
        assert_eq!(watcher.removes, vec![fixture.0.clone()]);
        assert!(tree.directories.is_empty());
    }

    #[test]
    fn safety_regression_symlink_loop_is_skipped_visibly() {
        let fixture = Fixture::new();
        std::os::unix::fs::symlink(&fixture.0, fixture.0.join("loop")).unwrap();
        let mut tree = tree(&fixture);
        let mut watcher = CountingWatcher::default();
        tree.reset(&fixture.roots(), &mut watcher, true);
        assert!(
            tree.activity.error.is_some(),
            "skipped symlink coverage must be visible"
        );
        assert_eq!(tree.directories.len(), 1);
    }

    #[test]
    fn safety_regression_ignored_build_events_cannot_fill_the_queue() {
        let (sender, receiver) = mpsc::sync_channel(MAX_EVENTS);
        let overflow = AtomicBool::new(false);
        for _ in 0..MAX_EVENTS * 3 {
            forward(
                &sender,
                &overflow,
                Ok(write_event(Path::new("/work/target/build/out"))),
                Path::new("/state"),
                None,
            );
        }
        assert!(receiver.try_recv().is_err());
        assert!(!overflow.load(Ordering::Relaxed));
        forward(
            &sender,
            &overflow,
            Ok(write_event(Path::new("/work/source.rs"))),
            Path::new("/state"),
            None,
        );
        assert_eq!(
            receiver.try_recv().unwrap().0.unwrap().paths,
            vec![PathBuf::from("/work/source.rs")]
        );
        let mut rescan = Event::new(EventKind::Other);
        rescan.attrs.set_flag(notify::event::Flag::Rescan);
        forward(&sender, &overflow, Ok(rescan), Path::new("/state"), None);
        assert!(receiver.try_recv().unwrap().0.unwrap().need_rescan());
    }

    #[test]
    fn safety_regression_vanished_creation_is_visible_without_losing_later_writes() {
        let fixture = Fixture::new();
        let mut tree = tree(&fixture);
        let mut watcher = CountingWatcher::default();
        tree.reset(&fixture.roots(), &mut watcher, true);
        tree.event(
            write_event(&fixture.0.join("vanished")),
            SystemTime::now(),
            &mut watcher,
        );
        assert!(tree.activity.error.is_some());
        let good = fixture.0.join("good.txt");
        fs::write(&good, "ok").unwrap();
        tree.event(write_event(&good), SystemTime::now(), &mut watcher);
        assert_eq!(tree.activity.files[0].path, good);
    }

    #[test]
    fn safety_regression_ignored_subtree_rescan_does_not_mark_useful_history_partial() {
        let (sender, receiver) = mpsc::sync_channel(MAX_EVENTS);
        let overflow = AtomicBool::new(false);
        let mut rescan = Event::new(EventKind::Other).add_path(PathBuf::from("/work/target"));
        rescan.attrs.set_flag(notify::event::Flag::Rescan);
        forward(&sender, &overflow, Ok(rescan), Path::new("/state"), None);
        assert!(receiver.try_recv().is_err());
        assert!(!overflow.load(Ordering::Relaxed));
    }

    #[test]
    fn safety_regression_idle_worker_does_not_publish_or_wake_unchanged_snapshots() {
        let fixture = Fixture::new();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake_count = Arc::clone(&count);
        let mut watcher = FileSystemWatcher::new(fixture.0.join("state"));
        watcher.set_wake(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        });
        let roots = vec![fixture.0.clone()];
        watcher.sync(roots.clone(), []);
        // Consume the readiness barrier directly so an empty default value
        // cannot make the test pass before native registration has finished.
        let deadline = Instant::now() + Duration::from_secs(2);
        let initial = loop {
            match watcher.updates.as_ref().unwrap().try_recv() {
                Ok(activity) => break activity,
                Err(mpsc::TryRecvError::Empty) => {}
                Err(error) => panic!("readiness barrier failed: {error}"),
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(initial == Activity::default());
        let before = 0;
        let deadline = Instant::now() + INTERVAL * 4;
        while Instant::now() < deadline {
            watcher.sync(roots.clone(), []);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(count.load(Ordering::Relaxed), before);
        let path = fixture.0.join("new.txt");
        fs::write(&path, "changed").unwrap();
        wait_for(&mut watcher, &roots, |activity| {
            activity.files.iter().any(|file| file.path == path)
        });
        assert!(count.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn initial_worker_error_wakes_without_a_prior_activity_snapshot() {
        let fixture = Fixture::new();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake_count = Arc::clone(&count);
        let mut watcher = FileSystemWatcher::new(fixture.0.join("state"));
        watcher.set_wake(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        });
        let roots = vec![fixture.0.join("unavailable")];
        wait_for(&mut watcher, &roots, |activity| {
            activity.error.is_some() && count.load(Ordering::Relaxed) > 0
        });
        assert!(watcher.take_changed());
    }
}
