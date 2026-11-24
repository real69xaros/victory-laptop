//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost.
//!
//! In the world the workers have more work than they can do, and most of
//! it is the host driver's compiles of big compute programs: 92 took 10 s
//! or more each in run vg2 (1,610 s of worker time), while a graphics
//! pipeline takes about 0.3-1.4 s. In the order of the title's first uses,
//! the cheap pipelines waited behind them: those first bound in vg2's
//! world were made a median 160-207 s later. `VICTORY_PIPELINE_SMALL_FIRST=1`
//! takes small pipelines before big compute programs ([`big_bytes`]) and
//! puts the big ones on at most [`big_workers`] workers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    pub(super) key: String,
    used: AtomicBool,
    /// A compute program big enough that the host driver takes seconds to
    /// compile it ([`big_bytes`]).
    big: bool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let key = key(&shaders, graphics.as_deref());
        let big = matches!(&shaders[..], [s] if s.stage == "compute" && s.container.len() >= big_bytes());
        Pending {
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            key,
            used: AtomicBool::new(false),
            big,
        }
    }

    fn make(&self) {
        self.made
            .get_or_init(|| HostPipeline::new(&self.shaders, self.graphics.as_deref()));
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Identifies the title pipeline by its shader containers and graphics state.
pub(crate) fn key(shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) -> String {
    let mut key = blake3::Hasher::new();
    for shader in shaders {
        key.update(shader.stage.as_bytes());
        key.update(shader.hash.as_bytes());
    }
    key.update(graphics.map(GraphicsState::describe).unwrap_or_default().as_bytes());
    key.finalize().to_hex()[..32].to_owned()
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        start(p, REQUESTED);
        wait(p, crate::frame::current());
    }
    p.made.get()?.as_ref()
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let budget = wait_budget();
    if budget.is_zero() {
        return;
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now).
    static SPENT: Mutex<(u64, Duration)> = Mutex::new((u64::MAX, Duration::ZERO));
    let shared = shared_budget();
    let left = {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

struct Queue {
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
    /// With [`small_first`]: the big ones ([`big_bytes`]), taken after the others.
    early_big: VecDeque<Arc<Pending>>,
    requested_big: VecDeque<Arc<Pending>>,
    /// With [`small_first`]: big ones being made now.
    big_making: usize,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: VecDeque::new(),
            requested: VecDeque::new(),
            early_big: VecDeque::new(),
            requested_big: VecDeque::new(),
            big_making: 0,
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        self.push_as(p, priority, small_first())
    }

    /// [`Queue::push`]; `split`: big ones go to queues of their own
    /// ([`small_first`]).
    fn push_as(&mut self, p: &Arc<Pending>, priority: u8, split: bool) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        let big = split && p.big;
        match (priority == REQUESTED, big) {
            (true, false) => self.requested.push_back(p.clone()),
            (true, true) => self.requested_big.push_back(p.clone()),
            (false, false) => self.early.push_back(p.clone()),
            (false, true) => self.early_big.push_back(p.clone()),
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        self.pop_as(small_first().then(big_workers))
    }

    /// [`Queue::pop`]; `big_cap`: with [`small_first`], the most big ones
    /// made at once – requested small ones first, then requested big ones
    /// while fewer than that are being made, then the early ones in the
    /// same order. `None` takes them as they were queued.
    fn pop_as(&mut self, big_cap: Option<usize>) -> Option<Arc<Pending>> {
        // An early entry can have been promoted and claimed already.
        fn early_entry(q: &mut VecDeque<Arc<Pending>>) -> Option<Arc<Pending>> {
            std::iter::from_fn(|| q.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        }
        let big_ok = big_cap.is_some_and(|cap| self.big_making < cap);
        let p = match self.requested.pop_front() {
            Some(p) => p,
            None => match big_ok.then(|| self.requested_big.pop_front()).flatten() {
                Some(p) => p,
                None => match early_entry(&mut self.early) {
                    Some(p) => p,
                    None => big_ok.then(|| early_entry(&mut self.early_big)).flatten()?,
                },
            },
        };
        if big_cap.is_some() && p.big {
            self.big_making += 1;
        }
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }

    /// A big one ([`Queue::pop_as`] with a cap) is made: room for the next.
    fn big_done(&mut self) {
        self.big_making = self.big_making.saturating_sub(1);
    }
}

/// Small pipelines before big compute programs, and the big ones on at most
/// [`big_workers`] workers (`VICTORY_PIPELINE_SMALL_FIRST=1`; off by default).
fn small_first() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_SMALL_FIRST").is_some_and(|v| v == "1"))
}

/// The size of a compute program's container from which it counts as big
/// ([`small_first`]): 16 KB, or `VICTORY_PIPELINE_BIG_KB`. Over runs vg1 and
/// vg2, programs of 16 KB or more were 228 of the 233 that the driver took
/// 5 s or more to compile, and 2 of those that took less than a second
/// (median by size: under 4 KB 0.09 s, 4-16 KB 0.76 s, 16-32 KB 4.2 s,
/// 32-64 KB 12.1 s, 64-128 KB 15.1 s).
fn big_bytes() -> usize {
    static KB: OnceLock<usize> = OnceLock::new();
    *KB.get_or_init(|| std::env::var("VICTORY_PIPELINE_BIG_KB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(16)) << 10
}

/// The workers that may make big ones at once ([`small_first`]): all but 3
/// (at least 1), or `VICTORY_PIPELINE_BIG_WORKERS`.
fn big_workers() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_PIPELINE_BIG_WORKERS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| worker_count().saturating_sub(3))
            .max(1)
    })
}

/// The workers [`workers`] starts: half the logical processors, 2 to 8.
fn worker_count() -> usize {
    std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8))
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        AVAILABLE.notify_one();
    }
}

fn next() -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop() {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// Starts the workers (half the logical processors, 2 to 8) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count();
        if small_first() {
            tracing::info!(
                workers,
                big_workers = big_workers(),
                big_kb = big_bytes() >> 10,
                "host pipelines: small ones first, big compute programs on at most big_workers workers"
            );
        }
        for i in 0..workers {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(|| {
                    // Below the title's own threads (loading, recording).
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        let p = next();
                        let started = Instant::now();
                        p.make();
                        if small_first() && p.big {
                            {
                                let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                                queue.big_done();
                            }
                            AVAILABLE.notify_all();
                            tracing::info!(shaders = ?p.shaders, ms = started.elapsed().as_millis() as u64, "big host pipeline made");
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }

    /// A compute pipeline of a container of `kb` KB.
    fn compute(kb: usize, fill: u8) -> Arc<Pending> {
        Arc::new(Pending::new(vec![super::super::shader("compute", &vec![fill; kb << 10])], None))
    }

    #[test]
    fn big_compute_programs_are_told_apart_by_size() {
        assert!(compute(64, 1).big);
        assert!(compute(16, 2).big);
        assert!(!compute(15, 3).big);
        assert!(!Arc::new(Pending::new(Vec::new(), None)).big);
    }

    /// With the split, requested small ones come before requested big ones,
    /// those before early ones, and no more big ones than the cap are made
    /// at once; without it, the order is the queue's.
    #[test]
    fn small_ones_go_first_and_big_ones_stay_within_the_cap() {
        let (big_a, big_c) = (compute(64, 4), compute(32, 5));
        let small_b = compute(1, 6);
        let small_early = compute(2, 7);
        let big_early = compute(20, 8);
        let mut queue = Queue::new();
        assert_eq!(queue.push_as(&big_a, REQUESTED, true), Some(true));
        assert_eq!(queue.push_as(&small_b, REQUESTED, true), Some(true));
        assert_eq!(queue.push_as(&big_c, REQUESTED, true), Some(true));
        assert_eq!(queue.push_as(&small_early, EARLY, true), Some(true));
        assert_eq!(queue.push_as(&big_early, EARLY, true), Some(true));

        let cap = Some(1);
        assert!(Arc::ptr_eq(&queue.pop_as(cap).unwrap(), &small_b), "the requested small one first");
        assert!(Arc::ptr_eq(&queue.pop_as(cap).unwrap(), &big_a), "then a requested big one");
        assert!(Arc::ptr_eq(&queue.pop_as(cap).unwrap(), &small_early), "the cap is reached: an early small one");
        assert!(queue.pop_as(cap).is_none(), "only big ones left, and one is being made");
        queue.big_done();
        assert!(Arc::ptr_eq(&queue.pop_as(cap).unwrap(), &big_c), "room again: the next requested big one");
        queue.big_done();
        assert!(Arc::ptr_eq(&queue.pop_as(cap).unwrap(), &big_early));
        assert!(queue.pop_as(cap).is_none());

        // promoted from early to requested, a big one is taken once
        let promoted = compute(48, 9);
        assert_eq!(queue.push_as(&promoted, EARLY, true), Some(true));
        assert_eq!(queue.push_as(&promoted, REQUESTED, true), Some(false));
        queue.big_done();
        assert!(Arc::ptr_eq(&queue.pop_as(cap).unwrap(), &promoted));
        queue.big_done();
        assert!(queue.pop_as(cap).is_none(), "its stale early entry is skipped");

        // without the split: as queued
        let (x, y) = (compute(64, 10), compute(1, 11));
        let mut queue = Queue::new();
        queue.push_as(&x, REQUESTED, false);
        queue.push_as(&y, REQUESTED, false);
        assert!(Arc::ptr_eq(&queue.pop_as(None).unwrap(), &x));
        assert!(Arc::ptr_eq(&queue.pop_as(None).unwrap(), &y));
        assert_eq!(queue.big_making, 0, "nothing counted without a cap");
    }
}
