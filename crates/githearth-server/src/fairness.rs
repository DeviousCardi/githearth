//! The Git share scheduler (M4.2, cases U30 and U30b).
//!
//! Replaces the single global `Semaphore` in front of `upload-pack` and
//! `receive-pack`. A request belongs to exactly one organization and may hold
//! at most that organization's cap of the global slots:
//!
//! * `floor` — a queued organization with fewer than this many slots is served
//!   first, so a busy tenant cannot starve a newcomer or an idle tenant at its
//!   reserved one;
//! * `ceiling` — no organization may hold every global slot, even when it is
//!   the only tenant running, so a peer always has somewhere to arrive;
//! * the fair share of the rest is `ceil(global / organizations that are
//!   active or queued)`, clamped between the floor and the ceiling.
//!
//! Demand is the number of organizations that hold a slot *plus* the
//! organizations that are waiting for one. Counting a newcomer as soon as it
//! queues is what re-derives every cap on arrival: the incumbent's share drops
//! at once, and a grant to an organization already above the floor refuses to
//! consume capacity reserved for a queued peer's floor.
//!
//! The global count is still the configured `git_concurrency`, and the
//! scheduler takes its permits from the same `Semaphore` the metrics endpoint
//! and the existing concurrency tests read. That keeps the meaning of
//! `git_slots.available_permits()` and makes an externally held permit visible
//! to the scheduler.
//!
//! # Accounting
//!
//! A handoff sends a fully built [`GitPermit`] through the waiter's channel,
//! not a bare semaphore permit. The counters are incremented before the send,
//! so the permit's `Drop` is the only decrement path, whatever happens next: a
//! cancelled waiter whose receiver is dropped after the send still returns the
//! slot and decrements `total`/`active`. A handoff can therefore never wedge
//! the scheduler with free permits and a full counter.
//!
//! # Lock order
//!
//! The scheduler's `std::sync::Mutex` is held only around the in-memory
//! counters and a non-blocking `try_acquire`; it is never held across an
//! `.await`, a pool connection, or the per-repository filesystem lock. The
//! transport order stays: Git permit, then the repository lock, then any pool
//! connection (the rule in `reconcile.rs`). A waiter holds no lock while it
//! sleeps.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use uuid::Uuid;

struct Waiter {
    organization: Uuid,
    tx: oneshot::Sender<GitPermit>,
}

#[derive(Default)]
struct State {
    active: HashMap<Uuid, usize>,
    total: usize,
    waiters: VecDeque<Waiter>,
}

/// One held Git slot. Dropping it either hands the permit to the next waiter
/// (a queued organization below its floor first) or returns it to the global
/// semaphore.
pub struct GitPermit {
    scheduler: Arc<GitScheduler>,
    organization: Uuid,
    permit: Option<OwnedSemaphorePermit>,
}

impl std::fmt::Debug for GitPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitPermit")
            .field("organization", &self.organization)
            .finish()
    }
}

impl Drop for GitPermit {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            self.scheduler.release(self.organization, permit);
        }
    }
}

pub struct GitScheduler {
    slots: Arc<Semaphore>,
    capacity: usize,
    floor: usize,
    ceiling: usize,
    state: std::sync::Mutex<State>,
}

impl GitScheduler {
    pub fn new(slots: Arc<Semaphore>, capacity: usize, floor: usize, ceiling: usize) -> Arc<Self> {
        let capacity = capacity.max(1);
        // One organization may never take the whole installation, even on a
        // small `git_concurrency`: the ceiling always leaves a slot free for a
        // peer that arrives later.
        let ceiling_max = capacity.saturating_sub(1).max(1);
        let floor = floor.clamp(1, capacity).min(ceiling_max);
        let ceiling = ceiling.clamp(floor, ceiling_max);
        Arc::new(Self {
            slots,
            capacity,
            floor,
            ceiling,
            state: std::sync::Mutex::new(State::default()),
        })
    }

    /// The configured global concurrency.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn floor(&self) -> usize {
        self.floor
    }

    pub fn ceiling(&self) -> usize {
        self.ceiling
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Organizations holding or waiting for a slot. A queued newcomer counts
    /// immediately, which is what shrinks the incumbent's cap on arrival.
    fn demand(state: &State) -> usize {
        let mut organizations: HashSet<Uuid> = state
            .active
            .iter()
            .filter(|(_, held)| **held > 0)
            .map(|(organization, _)| *organization)
            .collect();
        organizations.extend(state.waiters.iter().map(|waiter| waiter.organization));
        organizations.len().max(1)
    }

    fn cap(&self, state: &State) -> usize {
        let fair = self.capacity.div_ceil(Self::demand(state));
        fair.clamp(self.floor, self.ceiling)
    }

    /// How many slots are reserved for queued organizations still below their
    /// floor, excluding `granting` (the organization asking for this slot).
    fn reserved_for_queued_floors(&self, state: &State, granting: Uuid) -> usize {
        let mut seen = HashSet::new();
        let mut reserved = 0;
        for waiter in &state.waiters {
            if waiter.organization == granting || !seen.insert(waiter.organization) {
                continue;
            }
            let active = state.active.get(&waiter.organization).copied().unwrap_or(0);
            if active < self.floor {
                reserved += 1;
            }
        }
        reserved
    }

    /// Forget waiters whose receiver was dropped by a transport timeout. They
    /// are skipped on release anyway; pruning them keeps them from counting as
    /// demand and bounds the queue's growth.
    fn prune(state: &mut State) {
        state.waiters.retain(|waiter| !waiter.tx.is_closed());
    }

    fn grant(&self, state: &mut State, organization: Uuid) -> Option<OwnedSemaphorePermit> {
        if state.total >= self.capacity {
            return None;
        }
        let active = state.active.get(&organization).copied().unwrap_or(0);
        if active >= self.cap(state) {
            return None;
        }
        // An organization above its floor may not take a slot that a queued
        // peer needs for the one slot that keeps it alive.
        if active >= self.floor
            && state.total + self.reserved_for_queued_floors(state, organization) + 1
                > self.capacity
        {
            return None;
        }
        let permit = self.slots.clone().try_acquire_owned().ok()?;
        state.total += 1;
        *state.active.entry(organization).or_insert(0) += 1;
        Some(permit)
    }

    /// Take a slot if one is free and this organization is under its cap.
    /// The transports use this for an immediate decision; [`Self::acquire`]
    /// is the queued form. Every web path that runs Git uses this too, so it
    /// is charged to the same organization as a clone or a push.
    pub fn try_acquire(self: &Arc<Self>, organization: Uuid) -> Option<GitPermit> {
        let mut state = self.lock();
        Self::prune(&mut state);
        let permit = self.grant(&mut state, organization)?;
        Some(GitPermit {
            scheduler: self.clone(),
            organization,
            permit: Some(permit),
        })
    }

    /// Wait for a slot. A release serves a waiter below its floor first, then
    /// the oldest waiter under its cap; a waiter is never served ahead of a
    /// floor waiter from another organization. The caller bounds the wait
    /// (the transports use `git_wait_millis` and answer `Retry-After`).
    pub async fn acquire(self: &Arc<Self>, organization: Uuid) -> GitPermit {
        if let Some(permit) = self.try_acquire(organization) {
            return permit;
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.lock();
            Self::prune(&mut state);
            // Re-check under the same lock: a release between the first try
            // and this enqueue would otherwise leave us waiting for a slot
            // that is already free.
            if let Some(permit) = self.grant(&mut state, organization) {
                return GitPermit {
                    scheduler: self.clone(),
                    organization,
                    permit: Some(permit),
                };
            }
            state.waiters.push_back(Waiter { organization, tx });
        }
        match rx.await {
            Ok(permit) => permit,
            // The sender was dropped without a handoff. The scheduler is held
            // by `self`, so this cannot happen in practice; retry rather than
            // fabricate a permit if it ever does.
            Err(_) => Box::pin(self.acquire(organization)).await,
        }
    }

    fn release(self: &Arc<Self>, organization: Uuid, permit: OwnedSemaphorePermit) {
        let mut permit = Some(permit);
        let mut state = self.lock();
        if let Some(active) = state.active.get_mut(&organization) {
            *active = active.saturating_sub(1);
        }
        state.total = state.total.saturating_sub(1);
        Self::prune(&mut state);

        // Pick a waiter: any organization below its floor outranks every
        // organization merely under its fair share; among equals, oldest
        // first. A waiter whose receiver was dropped (the transport timed out)
        // is skipped.
        loop {
            if state.waiters.is_empty() {
                return;
            }
            let mut chosen = None;
            let mut fallback = None;
            for (index, waiter) in state.waiters.iter().enumerate() {
                let active = state.active.get(&waiter.organization).copied().unwrap_or(0);
                if active >= self.cap(&state) {
                    continue;
                }
                if active < self.floor {
                    chosen = Some(index);
                    break;
                }
                if fallback.is_none() {
                    fallback = Some(index);
                }
            }
            let Some(index) = chosen.or(fallback) else {
                // Every waiter is over its cap. Return the permit to the
                // semaphore; a later release will reconsider.
                return;
            };
            let waiter = state.waiters.remove(index).expect("index from the queue");
            state.total += 1;
            *state.active.entry(waiter.organization).or_insert(0) += 1;
            let outgoing = permit.take().expect("permit until handed over");
            let git_permit = GitPermit {
                scheduler: self.clone(),
                organization: waiter.organization,
                permit: Some(outgoing),
            };
            match waiter.tx.send(git_permit) {
                Ok(()) => return,
                Err(mut returned) => {
                    // The receiver went away after we built the permit. Undo
                    // the accounting this handoff reserved and try the next
                    // waiter with the same semaphore permit.
                    if let Some(active) = state.active.get_mut(&returned.organization) {
                        *active = active.saturating_sub(1);
                    }
                    state.total = state.total.saturating_sub(1);
                    permit = returned.permit.take();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The published defaults: eight global slots, floor one, ceiling four.
    fn scheduler() -> Arc<GitScheduler> {
        GitScheduler::new(Arc::new(Semaphore::new(8)), 8, 1, 4)
    }

    fn p95(samples: &mut [Duration]) -> Duration {
        samples.sort_unstable();
        let index = ((samples.len() as f64) * 0.95).ceil() as usize;
        samples[index.saturating_sub(1).min(samples.len() - 1)]
    }

    fn active(scheduler: &Arc<GitScheduler>, organization: Uuid) -> usize {
        let state = scheduler.lock();
        state.active.get(&organization).copied().unwrap_or(0)
    }

    async fn wait_for_active(scheduler: &Arc<GitScheduler>, organization: Uuid, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if active(scheduler, organization) >= expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "organization never reached {expected} active slots"
            );
            tokio::task::yield_now().await;
        }
    }

    async fn wait_for_waiters(scheduler: &Arc<GitScheduler>, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let queued = scheduler.lock().waiters.len();
            if queued == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected} queued waiters, saw {queued}"
            );
            tokio::task::yield_now().await;
        }
    }

    /// m4: on a small installation the configured ceiling is clamped below
    /// the capacity, so one organization can never take every slot even when
    /// `git_concurrency` is below the ceiling default.
    #[test]
    fn the_ceiling_is_clamped_below_the_global_capacity() {
        let scheduler = GitScheduler::new(Arc::new(Semaphore::new(4)), 4, 1, 4);
        assert_eq!(scheduler.ceiling(), 3);
        assert_eq!(scheduler.floor(), 1);
    }

    #[tokio::test]
    async fn one_organization_cannot_take_every_global_slot() {
        let scheduler = scheduler();
        let org = Uuid::new_v4();
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(
                scheduler
                    .try_acquire(org)
                    .expect("within the ceiling of four"),
            );
        }
        assert!(
            scheduler.try_acquire(org).is_none(),
            "the fifth is over its ceiling"
        );
        assert_eq!(
            scheduler.slots.available_permits(),
            4,
            "half the installation stays available to another organization"
        );
    }

    #[tokio::test]
    async fn two_organizations_split_the_installation_and_neither_exceeds_its_share() {
        let scheduler = GitScheduler::new(Arc::new(Semaphore::new(8)), 8, 1, 6);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(scheduler.try_acquire(a).expect("A within its share"));
        }
        for _ in 0..4 {
            held.push(scheduler.try_acquire(b).expect("B within its share"));
        }
        assert_eq!(
            scheduler.slots.available_permits(),
            0,
            "the installation is full"
        );
        assert!(
            scheduler.try_acquire(a).is_none(),
            "A may not exceed half the installation"
        );
        assert!(
            scheduler.try_acquire(b).is_none(),
            "B may not exceed half the installation"
        );
        assert_eq!(active(&scheduler, a), 4);
        assert_eq!(active(&scheduler, b), 4);
    }

    /// M4: the first organization into an idle installation cannot fill the
    /// old six-slot ceiling. With the measured default of four, the newcomer's
    /// four requests are served immediately, without waiting for a release.
    /// Reverting the default ceiling to six makes the newcomer's last two
    /// requests queue with nothing to release them and this test times out.
    #[tokio::test]
    async fn a_queued_newcomer_gets_its_floor_when_the_incumbent_arrives_first() {
        let config = crate::config::Config::default();
        let slots = Arc::new(Semaphore::new(config.git_concurrency));
        let scheduler = GitScheduler::new(
            slots,
            config.git_concurrency,
            config.git_organization_floor,
            config.git_organization_ceiling,
        );
        let incumbent = Uuid::new_v4();
        let newcomer = Uuid::new_v4();
        let mut held = Vec::new();
        while let Some(permit) = scheduler.try_acquire(incumbent) {
            held.push(permit);
        }
        assert_eq!(
            held.len(),
            config.git_organization_ceiling,
            "the first mover fills its ceiling, not the installation"
        );

        // The newcomer arrives at its four-slot share. Four slots are free, so
        // every request must be served without dropping an incumbent permit.
        let mut queued = Vec::new();
        for _ in 0..4 {
            let scheduler = scheduler.clone();
            queued.push(tokio::spawn(
                async move { scheduler.acquire(newcomer).await },
            ));
        }
        for task in queued {
            let permit = tokio::time::timeout(Duration::from_millis(500), task)
                .await
                .expect("the newcomer is served from the free slots")
                .expect("the task did not panic");
            held.push(permit);
        }
        assert_eq!(
            active(&scheduler, newcomer),
            4,
            "the newcomer reaches its four-slot share"
        );
        assert_eq!(held.len(), 8, "the installation is exactly full");
    }

    /// m1: the busy organization must actually have a queued waiter, otherwise
    /// plain FIFO passes the test. The busy organization is under its cap but
    /// above its floor; the quiet one is at zero, so the release must pick the
    /// quiet one even though the busy waiter is older.
    #[tokio::test]
    async fn a_queued_organization_at_its_floor_is_served_before_a_busier_one() {
        let scheduler = GitScheduler::new(Arc::new(Semaphore::new(6)), 6, 1, 6);
        let busy = Uuid::new_v4();
        let quiet = Uuid::new_v4();
        let filler = Uuid::new_v4();
        let third = Uuid::new_v4();
        let busy_permit = scheduler.try_acquire(busy).expect("busy slot one");
        let mut filler_permits = Vec::new();
        // Three organizations share the six slots (two each at their fair
        // share): the filler reaches its cap of three while busy holds one.
        for _ in 0..3 {
            filler_permits.push(
                scheduler
                    .try_acquire(filler)
                    .expect("the filler organization's slots"),
            );
        }
        let mut third_permits = Vec::new();
        for _ in 0..2 {
            third_permits.push(
                scheduler
                    .try_acquire(third)
                    .expect("the third organization's slots"),
            );
        }
        let busy_waiter = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire(busy).await })
        };
        wait_for_waiters(&scheduler, 1).await;
        let quiet_waiter = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.acquire(quiet).await })
        };
        wait_for_waiters(&scheduler, 2).await;
        // One release from the filling organization. The busy organization
        // would be next in FIFO order, but it is above the floor while the
        // quiet one is below it.
        drop(third_permits.pop().expect("one third-organization slot"));
        let permit = tokio::time::timeout(Duration::from_secs(2), quiet_waiter)
            .await
            .expect("the floor waiter is woken")
            .expect("the task did not panic");
        assert_eq!(active(&scheduler, quiet), 1);
        assert!(
            !busy_waiter.is_finished(),
            "the busy organization is still queued"
        );
        drop(permit);
        drop(busy_waiter);
        drop(busy_permit);
        drop(filler_permits);
        drop(third_permits);
    }

    /// M2: a waiter whose receiver is dropped after the handoff must not leak
    /// the counters. The only decrement path is `GitPermit::drop`, so the
    /// handoff carries the permit itself.
    #[tokio::test]
    async fn a_cancelled_handoff_returns_the_slot_and_leaves_no_leak() {
        // Capacity two with the ceiling clamped to one: A holds one, B's
        // first request takes the other, and three waiters queue.
        let scheduler = GitScheduler::new(Arc::new(Semaphore::new(2)), 2, 1, 2);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let held = vec![scheduler.try_acquire(a).expect("the one A slot")];
        // Queue four waiters for B, then drop every receiver before the
        // releases happen. Each handoff builds and sends a `GitPermit`; the
        // receiver's `Drop` must return it.
        let mut waiters = Vec::new();
        for _ in 0..4 {
            let scheduler = scheduler.clone();
            waiters.push(tokio::spawn(async move { scheduler.acquire(b).await }));
        }
        wait_for_waiters(&scheduler, 3).await;
        for waiter in &waiters {
            waiter.abort();
        }
        for waiter in waiters {
            let _ = waiter.await;
        }
        drop(held);
        // Give the aborted handoffs a chance to run their drops.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (total, held_slots) = {
            let scheduler_ref = &scheduler;
            let state = scheduler_ref.lock();
            (state.total, scheduler_ref.slots.available_permits())
        };
        assert_eq!(held_slots, 2, "every semaphore permit is free");
        assert_eq!(total, 0, "the counters agree with the permits held");
        assert_eq!(active(&scheduler, b), 0);
        let permit = scheduler
            .try_acquire(a)
            .expect("the scheduler still serves after cancelled handoffs");
        drop(permit);
    }

    /// The exit bound is relative: an organization at its configured share
    /// sees a p95 within twice the single-organization p95 at the same total
    /// load. This measures the scheduler itself, with a fixed simulated
    /// service time, so the number does not depend on Git or PostgreSQL.
    ///
    /// The measured organization presents sustained load at its share (four in
    /// flight), alone and then while a second organization saturates its own
    /// share with many more requests. The negative control repeats the shared
    /// leg with the ceiling set to the global capacity — fairness removed —
    /// and requires the same harness to report a breach. That control is what
    /// makes the number evidence rather than an assertion: if the cap is
    /// deleted the positive assertion fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_organization_at_its_share_stays_within_twice_the_single_organization_p95() {
        const REQUESTS: usize = 48;
        const IN_FLIGHT: usize = 4;
        const OTHER_REQUESTS: usize = 480;
        const OTHER_IN_FLIGHT: usize = 32;
        const SERVICE: Duration = Duration::from_millis(20);

        // Alone: the measured organization sends 48 requests at four in
        // flight. With the ceiling of four its cap equals its offered load.
        let single = {
            let scheduler = scheduler();
            let organization = Uuid::new_v4();
            let samples = run_load(
                scheduler.clone(),
                organization,
                REQUESTS,
                IN_FLIGHT,
                SERVICE,
            )
            .await;
            assert!(samples.len() == REQUESTS, "every request completed");
            let mut samples = samples;
            p95(&mut samples)
        };

        // Shared: the other organization fills the installation first, then
        // the measured organization sends the same 48-at-four.
        let shared = {
            let scheduler = scheduler();
            let measured = Uuid::new_v4();
            let other = Uuid::new_v4();
            let mut background = spawn_background_load(
                scheduler.clone(),
                other,
                OTHER_REQUESTS,
                OTHER_IN_FLIGHT,
                SERVICE,
            );
            wait_for_active(&scheduler, other, scheduler.ceiling()).await;
            let mut samples =
                run_load(scheduler.clone(), measured, REQUESTS, IN_FLIGHT, SERVICE).await;
            background.abort_all();
            while background.join_next().await.is_some() {}
            p95(&mut samples)
        };

        // Negative control: the same harness with no per-organization cap —
        // the old single global semaphore, where one organization may hold
        // every slot. The positive assertion above must fail in this shape.
        let uncontrolled = {
            let scheduler = unrestricted();
            let measured = Uuid::new_v4();
            let other = Uuid::new_v4();
            let mut background = spawn_background_load(
                scheduler.clone(),
                other,
                OTHER_REQUESTS,
                OTHER_IN_FLIGHT,
                SERVICE,
            );
            wait_for_active(&scheduler, other, 8).await;
            let mut samples =
                run_load(scheduler.clone(), measured, REQUESTS, IN_FLIGHT, SERVICE).await;
            background.abort_all();
            while background.join_next().await.is_some() {}
            p95(&mut samples)
        };

        let ratio = shared.as_secs_f64() / single.as_secs_f64();
        let control_ratio = uncontrolled.as_secs_f64() / single.as_secs_f64();
        eprintln!(
            "fairness: single p95 {single:?}, shared p95 {shared:?}, ratio {ratio:.2}; \
             no-cap control p95 {uncontrolled:?}, control ratio {control_ratio:.2}"
        );
        assert!(
            ratio <= 2.0,
            "shared p95 {shared:?} ({ratio:.2}x) exceeds twice the single-organization p95 {single:?}"
        );
        assert!(
            control_ratio > 2.0,
            "the harness must detect the loss of the cap: no-cap p95 {uncontrolled:?} \
             is only {control_ratio:.2}x the single-organization p95 {single:?}"
        );
    }

    /// The old shape: no per-organization ceiling and no floor reservation.
    /// Built directly rather than through `new`, which deliberately clamps.
    fn unrestricted() -> Arc<GitScheduler> {
        // `floor = ceiling = capacity` removes both the floor reservation and
        // the per-organization cap — the old single global semaphore. `new`
        // deliberately clamps this shape away, so the control builds it
        // directly.
        Arc::new(GitScheduler {
            slots: Arc::new(Semaphore::new(8)),
            capacity: 8,
            floor: 8,
            ceiling: 8,
            state: std::sync::Mutex::new(State::default()),
        })
    }

    /// A saturating tenant: many requests, at most `in_flight` submitted at
    /// once, held for `service` each. The gate is what keeps the queue at a
    /// realistic client's depth rather than putting every request in the
    /// scheduler's deque at once.
    fn spawn_background_load(
        scheduler: Arc<GitScheduler>,
        organization: Uuid,
        requests: usize,
        in_flight: usize,
        service: Duration,
    ) -> tokio::task::JoinSet<()> {
        let gate = Arc::new(Semaphore::new(in_flight));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..requests {
            let scheduler = scheduler.clone();
            let gate = gate.clone();
            tasks.spawn(async move {
                let _gate = gate.acquire_owned().await.expect("gate open");
                let permit = scheduler.acquire(organization).await;
                tokio::time::sleep(service).await;
                drop(permit);
            });
        }
        tasks
    }

    /// Run `requests` invocations for one organization, at most `in_flight`
    /// submitted at once, and return each completion latency from the start.
    async fn run_load(
        scheduler: Arc<GitScheduler>,
        organization: Uuid,
        requests: usize,
        in_flight: usize,
        service: Duration,
    ) -> Vec<Duration> {
        let start = Instant::now();
        let gate = Arc::new(Semaphore::new(in_flight));
        let mut tasks = Vec::new();
        for _ in 0..requests {
            let scheduler = scheduler.clone();
            let gate = gate.clone();
            tasks.push(tokio::spawn(async move {
                let _gate = gate.acquire_owned().await.expect("gate open");
                let permit = scheduler.acquire(organization).await;
                tokio::time::sleep(service).await;
                drop(permit);
                start.elapsed()
            }));
        }
        let mut samples = Vec::new();
        for task in tasks {
            samples.push(task.await.expect("load task did not panic"));
        }
        samples
    }
}
