use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;

const DEFAULT_SUBMIT_DELAY_MS: u64 = 300;

pub(super) fn tab_attention_priority(state: crate::detect::AgentState, seen: bool) -> u8 {
    match (state, seen) {
        (crate::detect::AgentState::Blocked, _) => 4,
        (crate::detect::AgentState::Idle, false) => 3,
        (crate::detect::AgentState::Working, _) => 2,
        (crate::detect::AgentState::Idle, true) => 1,
        (crate::detect::AgentState::Unknown, _) => 0,
    }
}

fn parse_api_key(key: &str) -> Option<crossterm::event::KeyEvent> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let normalized = key.trim();
    match normalized {
        "Enter" | "enter" => Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
        "Tab" | "tab" => Some(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty())),
        "Esc" | "esc" => Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
        "Backspace" | "backspace" => Some(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty())),
        "Up" | "up" => Some(KeyEvent::new(KeyCode::Up, KeyModifiers::empty())),
        "Down" | "down" => Some(KeyEvent::new(KeyCode::Down, KeyModifiers::empty())),
        "Left" | "left" => Some(KeyEvent::new(KeyCode::Left, KeyModifiers::empty())),
        "Right" | "right" => Some(KeyEvent::new(KeyCode::Right, KeyModifiers::empty())),
        "C-c" | "c-c" | "ctrl+c" => Some(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        _ if normalized.len() == 1 => normalized
            .chars()
            .next()
            .map(|ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty())),
        _ => None,
    }
}

pub(super) fn encode_api_text(runtime: &crate::terminal::TerminalRuntime, text: &str) -> Vec<u8> {
    let bracketed = runtime
        .input_state()
        .map(|state| state.bracketed_paste)
        .unwrap_or(false);
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

/// Paste `text` into a pane and press Enter, the way an agent is prompted.
///
/// Non-empty text is one write. Enter is a later write, after
/// [`submit_delay`], so it is not read as part of the paste. Empty text
/// presses Enter immediately. `Err` carries an error code and message for
/// the API to report.
pub(super) fn send_prompt(
    runtime: &crate::terminal::TerminalRuntime,
    text: &str,
) -> Result<(), (&'static str, String)> {
    let encoded_enter = encode_api_keys(runtime, &["Enter".into()])
        .map_err(|key| ("invalid_key", format!("unsupported key {key}")))?;
    let enter: Vec<u8> = encoded_enter.into_iter().flatten().collect();
    let text_bytes = if text.is_empty() {
        Vec::new()
    } else {
        encode_api_text(runtime, text)
    };
    deliver_text_then_keys(runtime, &text_bytes, &enter)
}

/// Write pane text and the keys that submit it.
///
/// Empty text sends `keys` at once. Non-empty text with a settle delay sends
/// the text first and the keys after the delay, as a second write. A delay of
/// zero keeps the old single write. Prompts for one pane stay in order: the
/// next text cannot pass the previous prompt's keys.
pub(super) fn deliver_text_then_keys(
    runtime: &crate::terminal::TerminalRuntime,
    text: &[u8],
    keys: &[u8],
) -> Result<(), (&'static str, String)> {
    let keys = Bytes::from(keys.to_vec());
    if text.is_empty() {
        return enqueue_submit(runtime, None, keys, Duration::ZERO);
    }
    let delay = submit_delay();
    if delay.is_zero() {
        let mut batch = text.to_vec();
        batch.extend_from_slice(&keys);
        return enqueue_submit(runtime, None, Bytes::from(batch), Duration::ZERO);
    }
    enqueue_submit(runtime, Some(Bytes::from(text.to_vec())), keys, delay)
}

fn submit_delay() -> Duration {
    Duration::from_millis(submit_delay_ms())
}

fn submit_delay_ms() -> u64 {
    // The owner mutex lets one test override HERDR_SUBMIT_DELAY_MS without
    // changing the delay other threads see. Release builds read the env var
    // and do not take that mutex.
    #[cfg(test)]
    if submit_env_owner_is_other_thread() {
        return DEFAULT_SUBMIT_DELAY_MS;
    }
    match std::env::var("HERDR_SUBMIT_DELAY_MS") {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .unwrap_or(DEFAULT_SUBMIT_DELAY_MS),
        Err(_) => DEFAULT_SUBMIT_DELAY_MS,
    }
}

#[cfg(test)]
fn submit_env_owner_is_other_thread() -> bool {
    let owner = submit_env_owner()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match *owner {
        Some(owner) => owner != std::thread::current().id(),
        None => false,
    }
}

#[cfg(test)]
fn submit_env_owner() -> &'static Mutex<Option<std::thread::ThreadId>> {
    static OWNER: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);
    &OWNER
}

struct SubmitJob {
    leading: Option<Bytes>,
    trailing: Bytes,
    delay: Duration,
}

#[derive(Default)]
struct SubmitLane {
    busy: bool,
    queue: VecDeque<SubmitJob>,
}

fn submit_lanes() -> std::sync::MutexGuard<'static, HashMap<u64, SubmitLane>> {
    static LANES: std::sync::LazyLock<Mutex<HashMap<u64, SubmitLane>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    LANES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn enqueue_submit(
    runtime: &crate::terminal::TerminalRuntime,
    leading: Option<Bytes>,
    trailing: Bytes,
    delay: Duration,
) -> Result<(), (&'static str, String)> {
    let key = runtime.submit_key();
    let tx = runtime.user_input_sender();
    {
        let mut lanes = submit_lanes();
        let lane = lanes.entry(key).or_default();
        if lane.busy {
            lane.queue.push_back(SubmitJob {
                leading,
                trailing,
                delay,
            });
            return Ok(());
        }
        lane.busy = true;
    }

    if delay.is_zero() {
        let bytes = match leading {
            Some(leading) => {
                let mut batch = leading.to_vec();
                batch.extend_from_slice(&trailing);
                Bytes::from(batch)
            }
            None => trailing,
        };
        if let Err(err) = tx.try_send(bytes) {
            pump_submit_lane(key, tx);
            return Err(("agent_send_failed", err.to_string()));
        }
        pump_submit_lane(key, tx);
        return Ok(());
    }

    if let Some(leading) = leading {
        if let Err(err) = tx.try_send(leading) {
            pump_submit_lane(key, tx);
            return Err(("agent_send_failed", err.to_string()));
        }
    }
    spawn_submit_lane(key, tx, trailing, delay);
    Ok(())
}

fn pump_submit_lane(key: u64, tx: crate::pane::UserInputSender) {
    let Some(job) = pop_submit_job(key) else {
        return;
    };
    spawn_submit_job(key, tx, job);
}

fn pop_submit_job(key: u64) -> Option<SubmitJob> {
    let mut lanes = submit_lanes();
    let lane = lanes.get_mut(&key)?;
    if let Some(job) = lane.queue.pop_front() {
        return Some(job);
    }
    lane.busy = false;
    lanes.remove(&key);
    None
}

fn spawn_submit_job(key: u64, tx: crate::pane::UserInputSender, job: SubmitJob) {
    let fail_tx = tx.clone();
    let fail_leading = job.leading.clone();
    let fail_trailing = job.trailing.clone();
    spawn_submit_task(
        async move {
            if let Some(leading) = job.leading {
                if tx.try_send(leading).is_err() {
                    tracing::warn!("queued prompt text was not written");
                    pump_submit_lane(key, tx);
                    return;
                }
            }
            run_submit_lane(key, tx, job.trailing, job.delay).await;
        },
        move || flush_unsent_submit(key, fail_tx, fail_leading, fail_trailing),
    );
}

fn spawn_submit_lane(key: u64, tx: crate::pane::UserInputSender, trailing: Bytes, delay: Duration) {
    let fail_tx = tx.clone();
    let fail_trailing = trailing.clone();
    spawn_submit_task(run_submit_lane(key, tx, trailing, delay), move || {
        flush_unsent_submit(key, fail_tx, None, fail_trailing)
    });
}

fn spawn_submit_task(
    task: impl std::future::Future<Output = ()> + Send + 'static,
    on_timer_fail: impl FnOnce() + Send + 'static,
) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(task);
        return;
    }
    std::thread::spawn(move || match build_submit_runtime() {
        Ok(rt) => rt.block_on(task),
        Err(err) => {
            tracing::warn!(%err, "submit settle timer did not start");
            on_timer_fail();
        }
    });
}

fn build_submit_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    #[cfg(test)]
    while submit_timer_hold() {
        std::thread::sleep(Duration::from_millis(1));
    }
    #[cfg(test)]
    if submit_timer_force_fail() {
        return Err(std::io::Error::other("submit settle timer forced to fail"));
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
}

#[cfg(test)]
static SUBMIT_TIMER_FORCE_FAIL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static SUBMIT_TIMER_HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn set_submit_timer_force_fail(force: bool) {
    SUBMIT_TIMER_FORCE_FAIL.store(force, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
fn submit_timer_force_fail() -> bool {
    SUBMIT_TIMER_FORCE_FAIL.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
fn set_submit_timer_hold(hold: bool) {
    SUBMIT_TIMER_HOLD.store(hold, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
fn submit_timer_hold() -> bool {
    SUBMIT_TIMER_HOLD.load(std::sync::atomic::Ordering::SeqCst)
}

fn write_submit_now(tx: &crate::pane::UserInputSender, leading: Option<Bytes>, trailing: Bytes) {
    if let Some(leading) = leading {
        if tx.try_send(leading).is_err() {
            tracing::warn!("queued prompt text was not written");
            return;
        }
    }
    if tx.try_send(trailing).is_err() {
        tracing::warn!("submit keys were not written");
    }
}

/// The fallback thread could not start the settle timer. The paste for this
/// job is already written when `leading` is `None`. Write the pending Enter
/// now, then each queued job the same way (its paste, if it is still pending,
/// then its Enter, with no delay). `pop_submit_job` clears the lane when the
/// queue is empty, so a later prompt is not stuck behind `busy`.
fn flush_unsent_submit(
    key: u64,
    tx: crate::pane::UserInputSender,
    leading: Option<Bytes>,
    trailing: Bytes,
) {
    write_submit_now(&tx, leading, trailing);
    while let Some(job) = pop_submit_job(key) {
        write_submit_now(&tx, job.leading, job.trailing);
    }
}

async fn run_submit_lane(
    key: u64,
    tx: crate::pane::UserInputSender,
    mut trailing: Bytes,
    mut delay: Duration,
) {
    loop {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if !trailing.is_empty() {
            let bytes = std::mem::replace(&mut trailing, Bytes::new());
            if tx.try_send(bytes).is_err() {
                tracing::warn!("submit keys were not written");
            }
        }
        let Some(job) = pop_submit_job(key) else {
            return;
        };
        if let Some(leading) = job.leading {
            if tx.try_send(leading).is_err() {
                tracing::warn!("queued prompt text was not written");
                delay = Duration::ZERO;
                trailing = Bytes::new();
                continue;
            }
        }
        delay = job.delay;
        trailing = job.trailing;
    }
}

pub(super) fn encode_api_keys(
    runtime: &crate::terminal::TerminalRuntime,
    keys: &[String],
) -> Result<Vec<Vec<u8>>, String> {
    let mut encoded_keys = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(key_event) = parse_api_key(key) else {
            return Err(key.clone());
        };
        encoded_keys.push(runtime.encode_terminal_key(key_event.into()));
    }
    Ok(encoded_keys)
}

pub(super) fn detect_state_from_api(
    state: crate::api::schema::PaneAgentState,
) -> crate::detect::AgentState {
    match state {
        crate::api::schema::PaneAgentState::Idle => crate::detect::AgentState::Idle,
        crate::api::schema::PaneAgentState::Working => crate::detect::AgentState::Working,
        crate::api::schema::PaneAgentState::Blocked => crate::detect::AgentState::Blocked,
        crate::api::schema::PaneAgentState::Unknown => crate::detect::AgentState::Unknown,
    }
}

pub(super) fn pane_agent_status(
    state: crate::detect::AgentState,
    seen: bool,
) -> crate::api::schema::AgentStatus {
    match (state, seen) {
        (crate::detect::AgentState::Idle, false) => crate::api::schema::AgentStatus::Done,
        (crate::detect::AgentState::Idle, true) => crate::api::schema::AgentStatus::Idle,
        (crate::detect::AgentState::Working, _) => crate::api::schema::AgentStatus::Working,
        (crate::detect::AgentState::Blocked, _) => crate::api::schema::AgentStatus::Blocked,
        (crate::detect::AgentState::Unknown, _) => crate::api::schema::AgentStatus::Unknown,
    }
}

pub(super) fn normalize_reported_agent_label(agent: &str) -> Option<String> {
    let trimmed = agent.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(agent) = crate::detect::parse_agent_label(trimmed) {
        return Some(crate::detect::agent_label(agent).to_string());
    }
    Some(trimmed.to_string())
}

pub(super) fn normalize_custom_status(status: Option<String>) -> Option<String> {
    let trimmed = status?.trim().to_string();
    let mut normalized = String::new();
    for ch in trimmed.chars().filter(|ch| !ch.is_control()).take(32) {
        normalized.push(ch);
    }
    (!normalized.trim().is_empty()).then(|| normalized.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds `HERDR_SUBMIT_DELAY_MS` for this thread. Other threads keep the
    /// default delay so a parallel test does not observe this override.
    struct SubmitDelayEnv {
        prior: Option<String>,
    }

    impl SubmitDelayEnv {
        fn claim() -> Self {
            let prior = std::env::var("HERDR_SUBMIT_DELAY_MS").ok();
            *submit_env_owner()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(std::thread::current().id());
            Self { prior }
        }

        fn set(&self, value: Option<&str>) {
            match value {
                Some(value) => std::env::set_var("HERDR_SUBMIT_DELAY_MS", value),
                None => std::env::remove_var("HERDR_SUBMIT_DELAY_MS"),
            }
        }
    }

    impl Drop for SubmitDelayEnv {
        fn drop(&mut self) {
            match &self.prior {
                Some(value) => std::env::set_var("HERDR_SUBMIT_DELAY_MS", value),
                None => std::env::remove_var("HERDR_SUBMIT_DELAY_MS"),
            }
            *submit_env_owner()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    /// Forces the fallback settle thread to fail its runtime build, and holds
    /// that thread until the test has queued a second prompt.
    struct ForceSubmitTimerFail;

    impl ForceSubmitTimerFail {
        fn arm() -> Self {
            set_submit_timer_force_fail(true);
            set_submit_timer_hold(true);
            Self
        }
    }

    impl Drop for ForceSubmitTimerFail {
        fn drop(&mut self) {
            set_submit_timer_hold(false);
            set_submit_timer_force_fail(false);
        }
    }

    fn wait_pane_bytes(rx: &mut tokio::sync::mpsc::Receiver<Bytes>) -> Bytes {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match rx.try_recv() {
                Ok(bytes) => return bytes,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        panic!("timed out waiting for pane bytes");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    panic!("pane channel closed");
                }
            }
        }
    }

    fn test_runtime() -> (
        crate::terminal::TerminalRuntime,
        tokio::sync::mpsc::Receiver<Bytes>,
    ) {
        crate::terminal::TerminalRuntime::test_with_channel(80, 24)
    }

    async fn recv_within(rx: &mut tokio::sync::mpsc::Receiver<Bytes>, budget: Duration) -> Bytes {
        tokio::time::timeout(budget, rx.recv())
            .await
            .expect("timed out waiting for pane bytes")
            .expect("pane channel closed")
    }

    #[tokio::test]
    async fn paste_then_enter_settles_and_keeps_order() {
        let env = SubmitDelayEnv::claim();
        env.set(None);
        assert_eq!(submit_delay_ms(), DEFAULT_SUBMIT_DELAY_MS);

        let (runtime, mut rx) = test_runtime();
        send_prompt(&runtime, "").unwrap();
        assert_eq!(&rx.try_recv().unwrap()[..], b"\r");
        assert!(rx.try_recv().is_err(), "empty text is Enter alone");

        let (runtime, mut rx) = test_runtime();
        let started = tokio::time::Instant::now();
        send_prompt(&runtime, "one").unwrap();
        send_prompt(&runtime, "two").unwrap();
        assert_eq!(&rx.try_recv().unwrap()[..], b"one");
        assert!(
            rx.try_recv().is_err(),
            "the second paste waits for the first Enter"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), rx.recv())
                .await
                .is_err(),
            "Enter arrived before the settle delay"
        );
        let enter = recv_within(&mut rx, Duration::from_secs(2)).await;
        assert!(
            started.elapsed() >= Duration::from_millis(DEFAULT_SUBMIT_DELAY_MS),
            "Enter arrived after {}ms",
            started.elapsed().as_millis()
        );
        assert_eq!(&enter[..], b"\r");
        let second = recv_within(&mut rx, Duration::from_millis(500)).await;
        assert_eq!(&second[..], b"two");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "the second Enter arrived before its own settle delay"
        );
        let enter = recv_within(&mut rx, Duration::from_secs(2)).await;
        assert_eq!(&enter[..], b"\r");
        assert!(rx.try_recv().is_err());

        env.set(Some("0"));
        assert_eq!(submit_delay_ms(), 0);
        let (runtime, mut rx) = test_runtime();
        send_prompt(&runtime, "joined").unwrap();
        assert_eq!(&rx.try_recv().unwrap()[..], b"joined\r");
        assert!(rx.try_recv().is_err(), "delay 0 is one write");
    }

    #[test]
    fn failed_submit_timer_writes_enter_and_clears_lane() {
        let _fail = ForceSubmitTimerFail::arm();
        // The pane constructor spawns a task, so it needs a runtime. Dropping
        // the enter guard (block_on returns) leaves this thread with no
        // current runtime, which is the fallback-thread path.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("pane test runtime");
        let (runtime, mut rx) = rt.block_on(async {
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 16)
        });
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the fallback thread is only used off a tokio runtime"
        );

        let delay = Duration::from_millis(DEFAULT_SUBMIT_DELAY_MS);
        let started = std::time::Instant::now();
        enqueue_submit(
            &runtime,
            Some(Bytes::from_static(b"one")),
            Bytes::from_static(b"\r"),
            delay,
        )
        .unwrap();
        assert_eq!(&rx.try_recv().unwrap()[..], b"one");
        assert!(
            rx.try_recv().is_err(),
            "Enter was written while the timer was held"
        );
        enqueue_submit(
            &runtime,
            Some(Bytes::from_static(b"two")),
            Bytes::from_static(b"\r"),
            delay,
        )
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "queued paste was written before the timer failed"
        );
        set_submit_timer_hold(false);

        assert_eq!(&wait_pane_bytes(&mut rx)[..], b"\r");
        assert_eq!(&wait_pane_bytes(&mut rx)[..], b"two");
        assert_eq!(&wait_pane_bytes(&mut rx)[..], b"\r");
        assert!(
            started.elapsed() < delay,
            "Enter waited out the settle delay after the timer failed to start"
        );

        enqueue_submit(
            &runtime,
            Some(Bytes::from_static(b"three")),
            Bytes::from_static(b"\r"),
            delay,
        )
        .unwrap();
        assert_eq!(&wait_pane_bytes(&mut rx)[..], b"three");
        assert_eq!(&wait_pane_bytes(&mut rx)[..], b"\r");
    }
}
