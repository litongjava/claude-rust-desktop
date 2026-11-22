//! One interruption future for every way a terminal, service manager or
//! supervisor asks this process to stop.
//!
//! Ctrl-C stays a turn-level interruption, as before, and can interrupt a
//! later turn again. SIGTERM, SIGHUP and terminal loss request cooperative
//! cancellation and a failure status. The active turn cancels its owned work
//! (process trees, hooks, agent children) and runs its destructors before the
//! runtime shuts down.
//!
//! Every platform publishes signals through the same generation channel, so
//! the call sites behave alike: Windows Ctrl-C is delivered exactly like a
//! Unix interrupt, and the channel is not tied to one signal source.
use std::cell::Cell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::watch;

/// The process-wide listener. Installed once, from inside the runtime.
static LISTENER: OnceLock<Result<Arc<SignalState>, String>> = OnceLock::new();

tokio::task_local! {
    /// The interrupt generation a signal waiter returned for while the turn
    /// future being polled on this task was running. Scoped to that future
    /// alone, so an unrelated waiter on the same task, a sibling select arm or
    /// a spawned task cannot record on this turn's behalf. Tokio restores the
    /// previous binding between polls, including across threads.
    static TURN_OBSERVED: Cell<Option<u64>>;
}

/// True when `candidate` is strictly newer than `previous`. Counts wrap, so
/// this is serial-number ordering: equal, older and exactly-half-range
/// candidates do not count as newer. Relevant generations are assumed to be
/// fewer than half the counter space apart.
fn is_newer(candidate: u64, previous: u64) -> bool {
    (candidate.wrapping_sub(previous) as i64) > 0
}

/// Keeps the first observation and afterwards only strictly newer ones, so a
/// stale generation can never replace a newer one.
fn advance(observed: Option<u64>, candidate: Option<u64>) -> Option<u64> {
    match (observed, candidate) {
        (None, candidate) => candidate,
        (Some(previous), Some(candidate)) if is_newer(candidate, previous) => Some(candidate),
        (observed, _) => observed,
    }
}

/// Records the generation a signal waiter returned for, only inside a captured
/// turn. Outside one there is no owner to record for, so this does nothing.
fn record_turn_observation(current: u64) {
    let _ = TURN_OBSERVED.try_with(|observed| {
        observed.set(advance(observed.get(), Some(current)));
    });
}

/// Runs one SDK message's turn with an observation of its own. The returned
/// generation is `None` when no waiter inside the turn returned for an OS
/// signal; `Some(0)` is valid after the counter wraps. A turn that returns an
/// error still reports what it observed; a dropped turn reports nothing.
pub(crate) async fn capture_turn_interrupt<F: Future>(turn: F) -> (F::Output, Option<u64>) {
    TURN_OBSERVED
        .scope(Cell::new(None), async {
            let result = turn.await;
            let observed = TURN_OBSERVED.with(Cell::get);
            (result, observed)
        })
        .await
}

/// Broadcast state shared by the signal listener and its waiters. Kept free of
/// OS signals so the notification semantics can be tested on any platform.
#[derive(Debug)]
struct SignalState {
    /// Counts notifications. A waiter records the count when it starts, so a
    /// signal is observed by each waiter exactly once and a later waiter is
    /// not woken by an earlier one.
    sender: watch::Sender<u64>,
    /// Kept alive so the channel never closes while the process runs.
    _receiver: watch::Receiver<u64>,
    /// SIGTERM, SIGHUP and terminal loss stop the process after turn cleanup.
    terminating: AtomicBool,
}

impl SignalState {
    fn new() -> Self {
        let (sender, receiver) = watch::channel(0u64);
        Self {
            sender,
            _receiver: receiver,
            terminating: AtomicBool::new(false),
        }
    }

    /// Publishes one signal. `terminating` also remembers that the process
    /// should exit after cleanup, including when no waiter exists yet.
    fn notify(&self, terminating: bool) {
        if terminating {
            self.terminating.store(true, Ordering::SeqCst);
        }
        self.sender
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    fn is_terminating(&self) -> bool {
        self.terminating.load(Ordering::SeqCst)
    }

    /// Resolves on Ctrl-C, SIGTERM or SIGHUP. Every waiter resolves, so a
    /// listener never consumes a signal that belongs to another owner. The
    /// generation is offered to the captured turn this task is polling, if any.
    async fn interrupted(&self) {
        let mut receiver = self.sender.subscribe();
        let seen = *receiver.borrow();
        loop {
            if self.is_terminating() {
                return;
            }
            let current = *receiver.borrow();
            if current != seen {
                record_turn_observation(current);
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// Resolves only for a signal that ends the process, including one
    /// received before this waiter started. Ctrl-C does not resolve it, so
    /// idle prompt editing keeps its current draft behaviour.
    async fn termination(&self) {
        let mut receiver = self.sender.subscribe();
        loop {
            if self.is_terminating() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

/// A turn-scoped observer that remembers the signal count it saw when it was
/// created. Waiting on it does not restart the baseline, so a signal received
/// while an earlier wait was dropped, or before the first poll, still cancels
/// the turn.
pub struct InterruptScope {
    /// `None` when no process-wide listener could be installed.
    state: Option<Arc<SignalState>>,
    /// Subscribed synchronously at creation, before the first poll.
    receiver: Option<watch::Receiver<u64>>,
    /// The count captured at creation. Advanced only by an explicit `rearm`.
    seen: u64,
    /// The newest generation this scope returned for, kept for its own rearm.
    observed: Option<u64>,
}

impl InterruptScope {
    fn from_state(state: Arc<SignalState>) -> Self {
        let receiver = state.sender.subscribe();
        let seen = *receiver.borrow();
        Self {
            state: Some(state),
            receiver: Some(receiver),
            seen,
            observed: None,
        }
    }

    /// Resolves once a Ctrl-C, SIGTERM or SIGHUP arrived after this scope was
    /// created. A termination received even earlier is still reported.
    pub async fn interrupted(&mut self) {
        let (Some(state), Some(receiver)) = (self.state.as_ref(), self.receiver.as_mut()) else {
            // Startup already refused to continue; a missing listener must
            // never cancel a turn that is otherwise running.
            std::future::pending().await
        };
        loop {
            if state.is_terminating() {
                return;
            }
            let current = *receiver.borrow();
            if current != self.seen {
                self.observed = advance(self.observed, Some(current));
                record_turn_observation(current);
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// Moves the baseline forward to a generation someone else handled, so a
    /// later wait does not report it again. Only a strictly newer value moves
    /// it, and only when a listener exists; a signal nobody handled therefore
    /// stays pending.
    pub(crate) fn rearm_to(&mut self, handled: Option<u64>) {
        if self.state.is_none() {
            return;
        }
        if let Some(handled) = handled {
            if is_newer(handled, self.seen) {
                self.seen = handled;
            }
        }
    }

    /// Handles the generations this scope itself returned for. Anything newer
    /// that arrived afterwards is still reported.
    pub fn rearm(&mut self) {
        let own = self.observed.take();
        self.rearm_to(own);
    }
}

/// Creates a turn-scoped observer. The signal count is captured now, not on
/// the first poll, so a signal delivered while the caller is handling an event
/// is not mistaken for the baseline of the next wait.
pub fn interrupt_scope() -> InterruptScope {
    match try_listener() {
        Ok(state) => InterruptScope::from_state(state.clone()),
        Err(_) => InterruptScope {
            state: None,
            receiver: None,
            seen: 0,
            observed: None,
        },
    }
}

/// The registration/installation boundary. A failed listener setup is returned
/// to the caller, never swallowed: startup must refuse to continue because
/// Tokio does not restore default handling for a signal whose receiver was
/// dropped after a partial registration, so a partially installed handler
/// could silently ignore SIGTERM or SIGHUP.
fn initialize(
    start: impl FnOnce(&Arc<SignalState>) -> std::io::Result<()>,
) -> Result<Arc<SignalState>, String> {
    let state = Arc::new(SignalState::new());
    start(&state).map_err(|error| {
        format!(
            "Cannot listen for interruption signals ({error}); refusing to start because a partially installed handler could swallow SIGTERM or SIGHUP"
        )
    })?;
    Ok(state)
}

fn try_listener() -> Result<&'static Arc<SignalState>, &'static str> {
    LISTENER
        .get_or_init(|| initialize(|state| spawn_listener(state.clone())))
        .as_ref()
        .map_err(String::as_str)
}

#[cfg(unix)]
fn spawn_listener(state: Arc<SignalState>) -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let (interrupt, terminate, hangup) = register_signals(|kind| signal(kind))?;
    spawn_registered(state, interrupt, terminate, hangup);
    Ok(())
}

/// Registers all three signals in one place and stops at the first failure, so
/// the caller can refuse startup instead of running with a partial set.
#[cfg(unix)]
fn register_signals<R>(
    mut register: impl FnMut(tokio::signal::unix::SignalKind) -> std::io::Result<R>,
) -> std::io::Result<(R, R, R)> {
    use tokio::signal::unix::SignalKind;
    Ok((
        register(SignalKind::interrupt())?,
        register(SignalKind::terminate())?,
        register(SignalKind::hangup())?,
    ))
}

#[cfg(unix)]
fn spawn_registered(
    state: Arc<SignalState>,
    mut interrupt: tokio::signal::unix::Signal,
    mut terminate: tokio::signal::unix::Signal,
    mut hangup: tokio::signal::unix::Signal,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = interrupt.recv() => state.notify(false),
                _ = terminate.recv() => state.notify(true),
                _ = hangup.recv() => state.notify(true),
            }
        }
    });
}

/// Windows keeps its Ctrl-C behaviour, but publishes every Ctrl-C through the
/// same channel in a loop, so a later turn can be interrupted again.
#[cfg(not(unix))]
fn spawn_listener(state: Arc<SignalState>) -> std::io::Result<()> {
    spawn_ctrl_c_listener(state);
    Ok(())
}

fn spawn_ctrl_c_listener(state: Arc<SignalState>) {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            state.notify(false);
        }
    });
}

/// Registers the handlers. Call this once, early, while the runtime is up, and
/// stop startup if it fails.
pub fn install() -> Result<(), &'static str> {
    try_listener().map(|_| ())
}

/// Resolves on Ctrl-C, SIGTERM or SIGHUP.
pub async fn interrupted() {
    match try_listener() {
        Ok(state) => state.interrupted().await,
        // Startup already refused to continue; a missing listener must never
        // cancel a turn that is otherwise running.
        Err(_) => std::future::pending().await,
    }
}

/// A one-shot command has no earlier turn whose interrupt can be ignored.
pub(crate) fn received_signal() -> bool {
    LISTENER.get().and_then(|listener| listener.as_ref().ok())
        .is_some_and(|state| state.is_terminating() || *state.sender.borrow() != 0)
}

/// True once termination is requested. Safe to check from a terminal reader
/// thread, which must never install the async listener itself.
pub fn terminating() -> bool {
    LISTENER
        .get()
        .and_then(|listener| listener.as_ref().ok())
        .is_some_and(|state| state.is_terminating())
}

pub(crate) fn notify_terminal_loss() {
    if let Some(Ok(state)) = LISTENER.get() {
        if !state.is_terminating() {
            state.notify(true);
        }
    }
}

#[cfg(unix)]
static WATCHING_INPUT: AtomicBool = AtomicBool::new(false);

/// Check exceptional conditions without reading input. A plain prompt can
/// reach EOF before the periodic watcher runs, so it checks here on exit.
#[cfg(unix)]
pub(crate) fn check_terminal_hangup() -> bool {
    use std::os::fd::AsRawFd;
    if !WATCHING_INPUT.load(Ordering::Acquire) {
        return false;
    }
    let mut fd = libc::pollfd {
        fd: std::io::stdin().as_raw_fd(),
        events: 0,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    let disconnected =
        ready > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
    if disconnected {
        notify_terminal_loss();
    }
    disconnected
}

/// Interactive stdin can disappear without a SIGHUP (for example when it
/// is a PTY slave that was never made the controlling terminal). Detect
/// hangup even while the REPL is running work with no active input reader.
#[cfg(unix)]
pub(crate) fn watch_terminal() {
    let Ok(state) = try_listener() else {
        return;
    };
    WATCHING_INPUT.store(true, Ordering::Release);
    tokio::spawn(async move {
        let termination = state.termination();
        tokio::pin!(termination);
        while !check_terminal_hangup() {
            tokio::select! {
                _ = &mut termination => break,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
    });
}

/// Resolves only for a signal that ends the process.
pub async fn termination() {
    match try_listener() {
        Ok(state) => state.termination().await,
        Err(_) => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// A failed installation is returned to the startup path, which then stops
    /// before any turn. It is not retried or silently downgraded.
    #[test]
    fn a_failed_installation_is_reported_instead_of_continuing() {
        let error = initialize(|_| Err(std::io::Error::other("injected failure"))).unwrap_err();
        assert!(
            error.contains("Cannot listen for interruption signals"),
            "{error}"
        );
        assert!(error.contains("refusing to start"), "{error}");
        assert!(error.contains("injected failure"), "{error}");
    }

    /// Registration stops at the first failure. The production function needs
    /// real streams, so the injected closure returns a plain counter: no OS
    /// handler is installed by this test, and it is safe in parallel.
    #[cfg(unix)]
    #[test]
    fn partial_registration_failure_stops_and_is_returned() {
        for failure_at in 1..=3 {
            let mut calls = 0;
            let result = register_signals(|_| {
                calls += 1;
                if calls == failure_at {
                    Err(std::io::Error::other(format!("failure {failure_at}")))
                } else {
                    Ok(calls)
                }
            });
            let error = result.err().expect("a partial registration must fail");
            assert_eq!(calls, failure_at, "registration continued after a failure");
            assert!(
                error.to_string().contains(&format!("failure {failure_at}")),
                "{error}"
            );
        }
    }

    /// Notification is what the converted Windows path now uses. Two waiters
    /// started before the signal both wake, a waiter started afterwards is not
    /// woken by the old signal, and the following signal wakes it.
    #[tokio::test]
    async fn each_notification_wakes_its_waiters_once() {
        let state = Arc::new(SignalState::new());
        let first = {
            let state = state.clone();
            tokio::spawn(async move { state.interrupted().await })
        };
        let second = {
            let state = state.clone();
            tokio::spawn(async move { state.interrupted().await })
        };
        tokio::task::yield_now().await;
        state.notify(false);
        timeout(Duration::from_secs(1), first)
            .await
            .expect("the first waiter was not woken")
            .unwrap();
        timeout(Duration::from_secs(1), second)
            .await
            .expect("the other waiter was not woken")
            .unwrap();

        let mut later = Box::pin(state.interrupted());
        assert!(
            timeout(Duration::from_millis(200), later.as_mut())
                .await
                .is_err(),
            "an earlier signal must not wake a later waiter"
        );
        state.notify(false);
        timeout(Duration::from_secs(1), later.as_mut())
            .await
            .expect("the next signal did not wake the waiter");
    }

    /// A termination signal is remembered when no waiter existed yet, and it
    /// also wakes a turn-level waiter.
    #[tokio::test]
    async fn a_termination_is_remembered_before_any_waiter() {
        let state = SignalState::new();
        state.notify(true);
        assert!(state.is_terminating());
        timeout(Duration::from_secs(1), state.termination())
            .await
            .expect("a termination received earlier was forgotten");
        timeout(Duration::from_secs(1), state.interrupted())
            .await
            .expect("a termination must interrupt the active turn");
    }

    /// Ctrl-C only interrupts the turn: it does not request a process exit.
    #[tokio::test]
    async fn an_interrupt_does_not_become_a_termination() {
        let state = SignalState::new();
        // Both waiters start before the signal, as a turn and an idle prompt
        // would.
        let mut turn = Box::pin(state.interrupted());
        let mut termination = Box::pin(state.termination());
        assert!(timeout(Duration::from_millis(50), turn.as_mut())
            .await
            .is_err());
        assert!(timeout(Duration::from_millis(50), termination.as_mut())
            .await
            .is_err());
        state.notify(false);
        assert!(!state.is_terminating());
        timeout(Duration::from_secs(1), turn.as_mut())
            .await
            .expect("Ctrl-C must still interrupt the turn");
        assert!(
            timeout(Duration::from_millis(200), termination.as_mut())
                .await
                .is_err(),
            "Ctrl-C must not end the prompt"
        );
    }

    /// The scope captures the count before its first poll, so a signal that
    /// arrives in between is not taken as the new baseline.
    #[tokio::test]
    async fn a_scope_sees_a_signal_sent_before_its_first_poll() {
        let state = Arc::new(SignalState::new());
        let mut scope = InterruptScope::from_state(state.clone());
        state.notify(false);
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("a signal sent before the first poll was lost");
    }

    /// The turn loop creates a fresh wait each iteration. A signal delivered
    /// after the previous wait was dropped must still wake the next one.
    #[tokio::test]
    async fn a_signal_between_two_waits_is_not_lost() {
        let state = Arc::new(SignalState::new());
        let mut scope = InterruptScope::from_state(state.clone());
        tokio::select! {
            _ = scope.interrupted() => panic!("no signal was sent yet"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        state.notify(false);
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("a signal between two waits was lost");
    }

    /// A new scope starts after an earlier signal: it must not inherit it, so
    /// a Ctrl-C from a previous turn cannot cancel the next turn.
    #[tokio::test]
    async fn a_new_scope_ignores_an_earlier_signal() {
        let state = Arc::new(SignalState::new());
        state.notify(false);
        let mut scope = InterruptScope::from_state(state.clone());
        assert!(
            timeout(Duration::from_millis(100), scope.interrupted())
                .await
                .is_err(),
            "a new scope inherited an earlier Ctrl-C"
        );
    }

    /// SIGTERM and SIGHUP are sticky, so a termination sent before the scope
    /// exists still stops the turn.
    #[tokio::test]
    async fn a_termination_is_seen_by_a_scope() {
        let state = Arc::new(SignalState::new());
        state.notify(true);
        let mut scope = InterruptScope::from_state(state.clone());
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("a termination received earlier was forgotten");
    }

    /// A scope never advances its own baseline, so an interruption already
    /// reported is reported again instead of being silently cleared.
    #[tokio::test]
    async fn a_scope_stays_interrupted_on_repeated_waits() {
        let state = Arc::new(SignalState::new());
        let mut scope = InterruptScope::from_state(state.clone());
        state.notify(false);
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("the first wait missed the signal");
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("a second wait forgot an interruption already observed");
    }

    /// A long-running observer that handled one interruption rearms and then
    /// waits for the next signal instead of firing again immediately.
    #[tokio::test]
    async fn a_rearmed_scope_waits_for_the_next_signal() {
        let state = Arc::new(SignalState::new());
        let mut scope = InterruptScope::from_state(state.clone());
        state.notify(false);
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("the first signal was lost");
        scope.rearm();
        assert!(
            timeout(Duration::from_millis(100), scope.interrupted())
                .await
                .is_err(),
            "a rearmed scope repeated an interruption it already handled"
        );
        state.notify(false);
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("the signal sent after rearming was lost");
    }

    /// A state whose counter starts at a given generation.
    fn state_at(count: u64) -> Arc<SignalState> {
        let (sender, receiver) = watch::channel(count);
        Arc::new(SignalState {
            sender,
            _receiver: receiver,
            terminating: AtomicBool::new(false),
        })
    }

    /// Asserts that an already-polled future is still pending, so it has
    /// registered with the channel before a signal is published.
    async fn assert_pending<F: Future + Unpin>(mut future: F) {
        assert!(
            timeout(Duration::from_millis(50), &mut future)
                .await
                .is_err(),
            "the future was expected to stay pending"
        );
    }

    /// The counter wraps, so detection must compare for inequality with the
    /// captured count rather than expect exactly one increment.
    #[tokio::test]
    async fn a_scope_detects_a_signal_across_counter_wrap() {
        let state = state_at(u64::MAX);
        let mut scope = InterruptScope::from_state(state.clone());
        state.notify(false);
        assert_eq!(*state.sender.borrow(), 0, "the counter did not wrap");
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("a wrapped counter hid the signal");
    }

    /// Without a process-wide listener a scope never resolves, and rearming it
    /// is harmless.
    #[tokio::test]
    async fn a_scope_without_a_listener_never_resolves() {
        let mut scope = InterruptScope {
            state: None,
            receiver: None,
            seen: 0,
            observed: None,
        };
        scope.rearm();
        scope.rearm_to(Some(1));
        assert!(
            timeout(Duration::from_millis(100), scope.interrupted())
                .await
                .is_err(),
            "a scope without a listener resolved"
        );
    }

    /// An unrelated waiter must not clear the SDK's pending Ctrl-C. The turn is
    /// polled while the signal arrives but nothing inside it returns for one, so
    /// its handoff is `None` and the outer scope still reports the signal.
    #[tokio::test]
    async fn unrelated_waiter_does_not_make_sdk_forget_interrupt() {
        let state = Arc::new(SignalState::new());
        let mut outer = InterruptScope::from_state(state.clone());
        // Registered and pending before the signal, outside the captured turn.
        let mut unrelated = Box::pin(state.interrupted());
        assert_pending(unrelated.as_mut()).await;
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
        let mut turn = Box::pin(capture_turn_interrupt(async move {
            finish_rx.await.ok();
            7u32
        }));
        assert_pending(turn.as_mut()).await;
        state.notify(false);
        timeout(Duration::from_secs(1), unrelated.as_mut())
            .await
            .expect("the unrelated waiter should have returned");
        finish_tx.send(()).unwrap();
        let (value, observed) = timeout(Duration::from_secs(1), turn.as_mut())
            .await
            .expect("the turn did not finish");
        assert_eq!(value, 7);
        assert_eq!(observed, None, "the turn itself observed no interruption");
        outer.rearm_to(observed);
        timeout(Duration::from_millis(100), outer.interrupted())
            .await
            .expect("an unrelated waiter hid the pending Ctrl-C from the SDK");

        // A second captured turn has its own cell, and a waiter in a spawned
        // task has no turn binding at all.
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
        let mut second = Box::pin(capture_turn_interrupt(async move {
            finish_rx.await.ok();
            8u32
        }));
        assert_pending(second.as_mut()).await;
        let mut spawned = {
            let state = state.clone();
            tokio::spawn(async move { state.interrupted().await })
        };
        assert_pending(&mut spawned).await;
        state.notify(false);
        timeout(Duration::from_secs(1), &mut spawned)
            .await
            .expect("the spawned waiter should have returned")
            .unwrap();
        finish_tx.send(()).unwrap();
        let (_, observed) = timeout(Duration::from_secs(1), second.as_mut())
            .await
            .expect("the second turn did not finish");
        assert_eq!(
            observed, None,
            "another owner's observation leaked into this turn"
        );
    }

    /// The SDK forgets exactly the generation the completed turn returned. A
    /// failed turn still reports it, and the next signal is not forgotten.
    #[tokio::test]
    async fn sdk_forgets_only_the_completed_turns_observed_generation() {
        let state = Arc::new(SignalState::new());
        let mut outer = InterruptScope::from_state(state.clone());
        let turn_state = state.clone();
        let (result, observed) = capture_turn_interrupt(async move {
            let mut nested = InterruptScope::from_state(turn_state.clone());
            turn_state.notify(false);
            timeout(Duration::from_secs(1), nested.interrupted())
                .await
                .expect("the nested waiter missed generation 1");
            Err::<(), &'static str>("cancelled")
        })
        .await;
        assert_eq!(result, Err("cancelled"));
        assert_eq!(observed, Some(1), "the turn's own generation");
        outer.rearm_to(observed);
        assert!(
            timeout(Duration::from_millis(100), outer.interrupted())
                .await
                .is_err(),
            "the completed turn's generation ended the SDK again"
        );
        state.notify(false);
        timeout(Duration::from_secs(1), outer.interrupted())
            .await
            .expect("the signal sent after rearming was lost");
    }

    /// A plain one-shot waiter inside the turn records its generation even when
    /// the operation turns the cancellation into a successful answer.
    #[tokio::test]
    async fn nested_one_shot_records_even_if_cancellation_is_translated() {
        let state = Arc::new(SignalState::new());
        let mut outer = InterruptScope::from_state(state.clone());
        let turn_state = state.clone();
        let (result, observed) = capture_turn_interrupt(async move {
            let mut question = Box::pin(turn_state.interrupted());
            assert_pending(question.as_mut()).await;
            turn_state.notify(false);
            timeout(Duration::from_secs(1), question.as_mut())
                .await
                .expect("the one-shot did not return for the signal");
            // The operation answers instead of propagating the cancellation.
            "answer: cancelled"
        })
        .await;
        assert_eq!(result, "answer: cancelled");
        assert_eq!(observed, Some(1));
        outer.rearm_to(observed);
        assert!(
            timeout(Duration::from_millis(100), outer.interrupted())
                .await
                .is_err(),
            "rearming kept a generation the turn already handled"
        );
    }

    /// A second signal that arrived after the turn's last observation survives
    /// the handoff, even though another waiter also returned for it.
    #[tokio::test]
    async fn second_interrupt_after_last_turn_observation_survives_rearm() {
        let state = Arc::new(SignalState::new());
        let mut outer = InterruptScope::from_state(state.clone());
        let turn_state = state.clone();
        let (_, observed) = capture_turn_interrupt(async move {
            let mut waiter = Box::pin(turn_state.interrupted());
            assert_pending(waiter.as_mut()).await;
            turn_state.notify(false);
            timeout(Duration::from_secs(1), waiter.as_mut())
                .await
                .expect("the turn missed generation 1");
        })
        .await;
        assert_eq!(observed, Some(1));
        // Generation 2 arrives after the turn stopped watching.
        let mut unrelated = Box::pin(state.interrupted());
        assert_pending(unrelated.as_mut()).await;
        state.notify(false);
        timeout(Duration::from_secs(1), unrelated.as_mut())
            .await
            .expect("the unrelated waiter missed generation 2");
        outer.rearm_to(observed);
        timeout(Duration::from_millis(100), outer.interrupted())
            .await
            .expect("a second Ctrl-C sent after the turn finished was forgotten");
    }

    /// Recording keeps the first observation and only strictly newer ones, and
    /// the same comparison drives baseline advancement, including across wrap.
    #[tokio::test]
    async fn observation_and_rearm_ignore_stale_generations_across_wrap() {
        assert_eq!(advance(None, None), None);
        // Zero is a valid first observation, distinct from "none".
        assert_eq!(advance(None, Some(0)), Some(0));
        // Out of order: the older sample does not replace the newer one.
        assert_eq!(advance(None, Some(2)), Some(2));
        assert_eq!(advance(Some(2), Some(1)), Some(2));
        // MAX - 1 -> MAX -> 0 advances; a stale MAX after the wrap does not.
        let mut observed = advance(None, Some(u64::MAX - 1));
        observed = advance(observed, Some(u64::MAX));
        assert_eq!(observed, Some(u64::MAX));
        observed = advance(observed, Some(0));
        assert_eq!(observed, Some(0), "zero after the wrap is newer than MAX");
        assert_eq!(advance(observed, Some(u64::MAX)), Some(0));
        // A duplicate is not newer; the next generation is.
        assert_eq!(advance(Some(0), Some(0)), Some(0));
        assert_eq!(advance(Some(0), Some(1)), Some(1));
        // Exactly half the range is the ambiguous tie and does not advance.
        assert!(!is_newer(1u64 << 63, 0));
        assert!(is_newer((1u64 << 63) - 1, 0));

        // The baseline obeys the same ordering.
        let state = Arc::new(SignalState::new());
        let mut scope = InterruptScope::from_state(state.clone());
        state.notify(false);
        state.notify(false);
        timeout(Duration::from_millis(100), scope.interrupted())
            .await
            .expect("generation 1");
        scope.rearm_to(Some(2));
        assert!(
            timeout(Duration::from_millis(50), scope.interrupted())
                .await
                .is_err(),
            "the baseline did not reach generation 2"
        );
        scope.rearm_to(Some(1));
        assert!(
            timeout(Duration::from_millis(50), scope.interrupted())
                .await
                .is_err(),
            "a stale generation moved the baseline back"
        );
        state.notify(false);
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("the generation after the stale one was lost");

        // Across the wrap, zero after MAX becomes the baseline.
        let state = state_at(u64::MAX);
        let mut scope = InterruptScope::from_state(state.clone());
        state.notify(false);
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("zero after the wrap");
        scope.rearm_to(Some(u64::MAX));
        scope.rearm_to(Some(0));
        assert!(
            timeout(Duration::from_millis(50), scope.interrupted())
                .await
                .is_err(),
            "the wrapped baseline was not advanced"
        );
    }

    /// The continuing worker rearms from its own observation, so a signal that
    /// arrived between its wait and its rearm is still handled. An unrelated
    /// waiter returning for the same generation must not change that.
    #[tokio::test]
    async fn worker_rearm_keeps_interrupt_after_its_own_observation() {
        let state = Arc::new(SignalState::new());
        let mut scope = InterruptScope::from_state(state.clone());
        state.notify(false);
        timeout(Duration::from_secs(1), scope.interrupted())
            .await
            .expect("the worker missed generation 1");
        // An unrelated waiter is registered and pending before the second
        // signal, then returns for it before the worker rearms.
        let mut unrelated = Box::pin(state.interrupted());
        assert!(
            timeout(Duration::from_millis(50), unrelated.as_mut())
                .await
                .is_err(),
            "the unrelated waiter should still be pending"
        );
        state.notify(false);
        timeout(Duration::from_secs(1), unrelated.as_mut())
            .await
            .expect("the unrelated waiter missed generation 2");
        scope.rearm();
        timeout(Duration::from_millis(100), scope.interrupted())
            .await
            .expect("rearming forgot a signal newer than the handled one");
        scope.rearm();
        assert!(
            timeout(Duration::from_millis(100), scope.interrupted())
                .await
                .is_err(),
            "the worker kept reporting a generation it already handled"
        );
    }

    /// Termination stays sticky and is not a generation receipt, so rearming
    /// cannot clear it.
    #[tokio::test]
    async fn termination_stays_sticky_without_a_turn_receipt() {
        let state = Arc::new(SignalState::new());
        let mut outer = InterruptScope::from_state(state.clone());
        state.notify(true);
        let turn_state = state.clone();
        let (_, observed) = capture_turn_interrupt(async move {
            timeout(Duration::from_secs(1), turn_state.interrupted())
                .await
                .expect("the free waiter must report a termination");
            let mut scoped = InterruptScope::from_state(turn_state.clone());
            timeout(Duration::from_secs(1), scoped.interrupted())
                .await
                .expect("the scoped waiter must report a termination");
        })
        .await;
        assert_eq!(
            observed, None,
            "a sticky termination is not a generation receipt"
        );
        outer.rearm_to(observed);
        timeout(Duration::from_secs(1), outer.interrupted())
            .await
            .expect("a termination must stay sticky after rearming");
    }
}
