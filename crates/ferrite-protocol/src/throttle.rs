//! Rate limiting for authentication.
//!
//! Passwords are already compared in constant time (see [`crate::auth`]),
//! which stops an attacker learning anything from *how* an attempt fails.
//! It does nothing about how *many* attempts they get, and unlimited
//! guesses beat any password comparison. This is the other half.
//!
//! Two things happen to a source that keeps failing:
//!
//! - each failed attempt is answered more slowly than the last, which costs
//!   an online guessing loop its rate without costing the server anything
//!   (the delay is an async sleep, not a held thread);
//! - past a threshold within a sliding window, further attempts are refused
//!   before the password is even read, for a lockout period.
//!
//! Both the source address and the attempted user name are tracked, so a
//! distributed attempt at one account is caught by the user key even when
//! no single address stands out, and a single host spraying many user names
//! is caught by the address key.
//!
//! Two consequences worth knowing before tuning the policy. An address
//! lockout covers *every* account from that address — otherwise spraying
//! user names would walk around it — so clients sharing one outbound
//! address (a NAT, an application server pool) share the limit. And a user
//! lockout applies wherever it is attempted from, so a script with a stale
//! password can lock a real account out for the lockout period; the window
//! is deliberately generous enough that a person retyping a password never
//! reaches it.
//!
//! State is in memory, which is the right size for a single-node v1: an
//! external store would add an operational dependency to a database whose
//! whole point is not being one. A restart clears the lockouts, which is
//! the honest trade — it is also the moment every legitimate client
//! reconnects.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tracing::warn;

/// Most sources tracked at once.
///
/// The map is the only unbounded thing here, and it is fed by unauthenticated
/// peers. At the cap, expired entries are swept and new sources stop being
/// tracked rather than being allowed to grow memory without limit — an
/// attacker rotating through more than this many addresses evades the
/// address key, but not the user key, which is the one that matters for
/// guessing a specific account.
const MAX_TRACKED_SOURCES: usize = 10_000;

/// How aggressive the throttle is.
#[derive(Debug, Clone, Copy)]
pub struct ThrottlePolicy {
    /// Failures within `window` before a source is locked out.
    pub max_failures: u32,
    pub window: Duration,
    pub lockout: Duration,
    /// Added to the delay before answering, per failure already recorded.
    pub delay_step: Duration,
    pub max_delay: Duration,
}

impl Default for ThrottlePolicy {
    /// Five failures a minute is far above what a client with the right
    /// password does and far below what guessing needs.
    fn default() -> Self {
        Self {
            max_failures: 5,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(300),
            delay_step: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
        }
    }
}

/// What a source is identified by. Both are tracked for every attempt.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Address(IpAddr),
    User(String),
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Address(addr) => write!(f, "address {addr}"),
            Key::User(user) => write!(f, "user {user:?}"),
        }
    }
}

#[derive(Debug)]
struct Entry {
    /// Failure times inside the window, oldest first.
    failures: VecDeque<Instant>,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// Sliding-window failure counter, shared by every connection.
#[derive(Debug)]
pub struct AuthThrottle {
    policy: Option<ThrottlePolicy>,
    state: Mutex<HashMap<Key, Entry>>,
}

impl Default for AuthThrottle {
    fn default() -> Self {
        Self::new(ThrottlePolicy::default())
    }
}

impl AuthThrottle {
    pub fn new(policy: ThrottlePolicy) -> Self {
        Self {
            policy: Some(policy),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// A throttle that never refuses anything.
    ///
    /// For a listener whose peers are already trusted, and for tests that
    /// need to fail authentication more often than any real client would.
    pub fn disabled() -> Self {
        Self {
            policy: None,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// `Err(remaining)` when this source is locked out and the attempt must
    /// be refused without reading a password.
    pub fn check(&self, address: Option<IpAddr>, user: &str) -> Result<(), Duration> {
        let Some(policy) = self.policy else {
            return Ok(());
        };
        let now = Instant::now();
        let mut state = self.lock();
        let mut longest = Duration::ZERO;
        for key in keys(address, user) {
            if let Some(entry) = state.get_mut(&key) {
                prune(entry, now, policy.window);
                if let Some(until) = entry.locked_until {
                    if until > now {
                        longest = longest.max(until - now);
                    } else {
                        entry.locked_until = None;
                    }
                }
            }
        }
        if longest.is_zero() {
            Ok(())
        } else {
            Err(longest)
        }
    }

    /// Records a failed attempt and returns how long the refusal should be
    /// held back before it is sent.
    pub fn record_failure(&self, address: Option<IpAddr>, user: &str) -> Duration {
        let Some(policy) = self.policy else {
            return Duration::ZERO;
        };
        let now = Instant::now();
        let mut state = self.lock();
        // Only at the cap: sweeping is linear in the number of tracked
        // sources, and paying that on every failed password would hand an
        // attacker a cheaper way to load the server than guessing.
        if state.len() >= MAX_TRACKED_SOURCES {
            sweep(&mut state, now, policy);
        }

        let mut delay = Duration::ZERO;
        for key in keys(address, user) {
            if !state.contains_key(&key) {
                if state.len() >= MAX_TRACKED_SOURCES {
                    continue;
                }
                state.insert(
                    key.clone(),
                    Entry {
                        failures: VecDeque::new(),
                        locked_until: None,
                        last_seen: now,
                    },
                );
            }
            let entry = state.get_mut(&key).expect("present or just inserted");
            prune(entry, now, policy.window);
            entry.failures.push_back(now);
            entry.last_seen = now;

            let count = entry.failures.len() as u32;
            delay = delay.max(policy.delay_step * count).min(policy.max_delay);

            if count >= policy.max_failures && entry.locked_until.is_none() {
                entry.locked_until = Some(now + policy.lockout);
                ferrite_metrics::metrics().auth_lockouts_total.inc();
                warn!(
                    source = %key,
                    failures = count,
                    window_s = policy.window.as_secs(),
                    lockout_s = policy.lockout.as_secs(),
                    "too many failed authentications: locking this source out temporarily"
                );
            }
        }
        delay
    }

    /// Forgets a source's failures. A password that finally works is proof
    /// the attempts were a person mistyping, not a guessing loop.
    pub fn record_success(&self, address: Option<IpAddr>, user: &str) {
        if self.policy.is_none() {
            return;
        }
        let mut state = self.lock();
        for key in keys(address, user) {
            state.remove(&key);
        }
    }

    /// A poisoned lock must not take the listener down, and it must not
    /// silently disable the throttle either; the map is rebuilt empty,
    /// which loses history but keeps the limiter running.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Entry>> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut guard = poisoned.into_inner();
            guard.clear();
            guard
        })
    }
}

fn keys(address: Option<IpAddr>, user: &str) -> Vec<Key> {
    let mut keys = Vec::with_capacity(2);
    if let Some(address) = address {
        keys.push(Key::Address(address));
    }
    keys.push(Key::User(user.to_owned()));
    keys
}

/// Drops failures that have fallen out of the window.
fn prune(entry: &mut Entry, now: Instant, window: Duration) {
    while entry
        .failures
        .front()
        .is_some_and(|at| now.duration_since(*at) >= window)
    {
        entry.failures.pop_front();
    }
}

/// Drops sources with nothing left to remember.
fn sweep(state: &mut HashMap<Key, Entry>, now: Instant, policy: ThrottlePolicy) {
    state.retain(|_, entry| {
        prune(entry, now, policy.window);
        let locked = entry.locked_until.is_some_and(|until| until > now);
        locked || !entry.failures.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: Option<IpAddr> = Some(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)));
    const OTHER: Option<IpAddr> = Some(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2)));

    fn policy() -> ThrottlePolicy {
        ThrottlePolicy {
            max_failures: 3,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(300),
            delay_step: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_is_locked_out_after_the_threshold() {
        let throttle = AuthThrottle::new(policy());
        assert!(throttle.check(ADDR, "app").is_ok());

        for _ in 0..2 {
            throttle.record_failure(ADDR, "app");
            assert!(
                throttle.check(ADDR, "app").is_ok(),
                "under the threshold, attempts still go through"
            );
        }
        throttle.record_failure(ADDR, "app");
        assert!(throttle.check(ADDR, "app").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_lockout_expires_on_its_own() {
        let throttle = AuthThrottle::new(policy());
        for _ in 0..3 {
            throttle.record_failure(ADDR, "app");
        }
        assert!(throttle.check(ADDR, "app").is_err());

        tokio::time::advance(Duration::from_secs(301)).await;
        assert!(
            throttle.check(ADDR, "app").is_ok(),
            "a lockout is temporary; nothing has to unlock it by hand"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failures_outside_the_window_do_not_count() {
        let throttle = AuthThrottle::new(policy());
        throttle.record_failure(ADDR, "app");
        throttle.record_failure(ADDR, "app");
        tokio::time::advance(Duration::from_secs(61)).await;
        throttle.record_failure(ADDR, "app");
        assert!(
            throttle.check(ADDR, "app").is_ok(),
            "the first two aged out, so this is the first failure of a new window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_successful_login_clears_the_history() {
        let throttle = AuthThrottle::new(policy());
        throttle.record_failure(ADDR, "app");
        throttle.record_failure(ADDR, "app");
        throttle.record_success(ADDR, "app");
        throttle.record_failure(ADDR, "app");
        throttle.record_failure(ADDR, "app");
        assert!(throttle.check(ADDR, "app").is_ok());
    }

    /// The point of tracking the user name as well: an attacker spread over
    /// many addresses is still guessing at one account.
    #[tokio::test(start_paused = true)]
    async fn one_account_attacked_from_everywhere_is_still_locked() {
        let throttle = AuthThrottle::new(policy());
        for i in 0..3u8 {
            let addr = Some(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 1, i)));
            throttle.record_failure(addr, "admin");
        }
        let fresh = Some(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 1, 99)));
        assert!(
            throttle.check(fresh, "admin").is_err(),
            "the user key must catch a distributed attempt"
        );
        assert!(
            throttle.check(fresh, "someone_else").is_ok(),
            "and must not lock out unrelated accounts"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn locking_one_source_does_not_lock_another() {
        let throttle = AuthThrottle::new(policy());
        for _ in 0..3 {
            throttle.record_failure(ADDR, "app");
        }
        assert!(throttle.check(ADDR, "app").is_err());
        assert!(throttle.check(OTHER, "other_user").is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn each_failure_is_answered_more_slowly_up_to_a_cap() {
        let throttle = AuthThrottle::new(ThrottlePolicy {
            max_failures: 100,
            ..policy()
        });
        let first = throttle.record_failure(ADDR, "app");
        let second = throttle.record_failure(ADDR, "app");
        assert!(second > first);
        for _ in 0..50 {
            throttle.record_failure(ADDR, "app");
        }
        assert_eq!(throttle.record_failure(ADDR, "app"), policy().max_delay);
    }

    #[tokio::test(start_paused = true)]
    async fn a_disabled_throttle_never_refuses_or_delays() {
        let throttle = AuthThrottle::disabled();
        for _ in 0..1000 {
            assert_eq!(throttle.record_failure(ADDR, "app"), Duration::ZERO);
        }
        assert!(throttle.check(ADDR, "app").is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn the_source_table_stays_bounded() {
        let throttle = AuthThrottle::new(policy());
        for i in 0..(MAX_TRACKED_SOURCES + 500) {
            let addr = IpAddr::V4(std::net::Ipv4Addr::from((i as u32).to_be_bytes()));
            throttle.record_failure(Some(addr), &format!("user{i}"));
        }
        assert!(throttle.lock().len() <= MAX_TRACKED_SOURCES);
    }
}
