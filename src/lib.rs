#![cfg_attr(test, deny(warnings))]
#![deny(missing_docs)]

//! # scoped-pool
//!
//! A flexible thread pool providing scoped threads.
//!

extern crate variance;
extern crate crossbeam;

#[macro_use]
extern crate scopeguard;

use variance::InvariantLifetime as Id;
use crossbeam::sync::MsQueue;

use std::{thread, mem};
use std::sync::{Arc, Mutex, Condvar};

/// A thread-pool providing scoped and unscoped threads.
///
/// The primary ways of interacting with the `Pool` are
/// the `spawn` and `scoped` convenience methods or through
/// the `Scope` type directly.
#[derive(Clone)]
pub struct Pool {
    queue: Arc<MsQueue<PoolMessage>>,
    wait: Arc<WaitGroup>
}

impl Pool {
    /// Create a new Pool with `size` threads.
    ///
    /// If `size` is zero, no threads will be spawned. Threads can
    /// be added later via `expand`.
    ///
    /// NOTE: Since Pool can be freely cloned, it does not represent a unique
    /// handle to the thread pool. As a consequence, the thread pool is not
    /// automatically shut down; you must explicitly call `Pool::shutdown` to
    /// shut down the pool.
    #[inline]
    pub fn new(size: usize) -> Pool {
        // Create an empty pool.
        let pool = Pool::empty();

        // Start the requested number of threads.
        for _ in 0..size { pool.expand(); }

        pool
    }

    /// Create an empty Pool, with no threads.
    ///
    /// Note that no jobs will run until `expand` is called and
    /// worker threads are added.
    #[inline]
    pub fn empty() -> Pool {
        Pool {
            queue: Arc::new(MsQueue::new()),
            wait: Arc::new(WaitGroup::new())
        }
    }

    /// How many worker threads are currently active.
    #[inline]
    pub fn workers(&self) -> usize {
        // All threads submit themselves when they start and
        // complete when they stop, so the threads we are waiting
        // for are still active.
        self.wait.waiting()
    }

    /// Spawn a `'static'` job to be run on this pool.
    ///
    /// We do not wait on the job to complete.
    ///
    /// Panics in the job will propogate to the calling thread.
    #[inline]
    pub fn spawn<F: FnOnce() + Send + 'static>(&self, job: F) {
        // Run the job on a scope which lasts forever, and won't block.
        Scope::forever(self.clone()).execute(job)
    }

    /// Create a Scope for scheduling a group of jobs in `'scope'`.
    ///
    /// `scoped` will return only when the `scheduler` function and
    /// all jobs queued on the given Scope have been run.
    ///
    /// Panics in any of the jobs or in the scheduler function itself
    /// will propogate to the calling thread.
    #[inline]
    pub fn scoped<'scope, F, R>(&self, scheduler: F) -> R
    where F: FnOnce(&Scope<'scope>) -> R {
        // Zoom to the correct scope, then run the scheduler.
        Scope::forever(self.clone()).zoom(scheduler)
    }

    /// Shutdown the Pool.
    ///
    /// WARNING: Extreme care should be taken to not call shutdown concurrently
    /// with any scoped calls, or deadlock can occur.
    ///
    /// All threads will be shut down eventually, but only threads started before the
    /// call to shutdown are guaranteed to be shut down before the call to shutdown
    /// returns.
    #[inline]
    pub fn shutdown(&self) {
        // Start the shutdown process.
        self.queue.push(PoolMessage::Quit);

        // Wait for it to complete.
        self.wait.join()
    }

    /// Expand the Pool by spawning an additional thread.
    ///
    /// Can accelerate the completion of running jobs.
    #[inline]
    pub fn expand(&self) {
        let pool = self.clone();

        // Submit the new thread to the thread waitgroup.
        pool.wait.submit();

        // Start the actual thread.
        thread::spawn(move || pool.run_thread());
    }

    fn run_thread(self) {
        // Create a sentinel to capture panics on this thread.
        let mut thread_sentinel = ThreadSentinel(Some(self.clone()));

        loop {
            match self.queue.pop() {
                // On Quit, repropogate and quit.
                PoolMessage::Quit => {
                    // Repropogate the Quit message to other threads.
                    self.queue.push(PoolMessage::Quit);

                    // Cancel the thread sentinel so we don't panic waiting
                    // shutdown threads, and don't restart the thread.
                    thread_sentinel.cancel();

                    // Terminate the thread.
                    break
                },

                // On Task, run the task then complete the WaitGroup.
                PoolMessage::Task(job, wait) => {
                    let sentinel = Sentinel(self.clone(), Some(wait.clone()));
                    job.run();
                    sentinel.cancel();
                }
            }
        }
    }
}

/// An execution scope, represents a set of jobs running on a Pool.
///
/// ## Understanding Scope lifetimes
///
/// Besides `Scope<'static>`, all `Scope` objects are accessed behind a
/// reference of the form `&'scheduler Scope<'scope>`.
///
/// `'scheduler` is the lifetime associated with the *body* of the
/// "scheduler" function (functions passed to `zoom`/`scoped`).
///
/// `'scope` is the lifetime which data captured in `execute` or `recurse`
/// closures must outlive - in other words, `'scope` is the maximum lifetime
/// of all jobs scheduler on a `Scope`.
///
/// Note that since `'scope: 'scheduler` (`'scope` outlives `'scheduler`)
/// `&'scheduler Scope<'scope>` can't be captured in an `execute` closure;
/// this is the reason for the existence of the `recurse` API, which will
/// inject the same scope with a new `'scheduler` lifetime (this time set
/// to the body of the function passed to `recurse`).
pub struct Scope<'scope> {
    pool: Pool,
    wait: Arc<WaitGroup>,
    _scope: Id<'scope>
}

impl<'scope> Scope<'scope> {
    /// Create a Scope which lasts forever.
    #[inline]
    pub fn forever(pool: Pool) -> Scope<'static> {
        Scope {
            pool: pool,
            wait: Arc::new(WaitGroup::new()),
            _scope: Id::default()
        }
    }

    /// Add a job to this scope.
    ///
    /// Subsequent calls to `join` will wait for this job to complete.
    pub fn execute<F>(&self, job: F)
    where F: FnOnce() + Send + 'scope {
        // Submit the job *before* submitting it to the queue.
        self.wait.submit();

        let task = unsafe {
            // Safe because we will ensure the task finishes executing before
            // 'scope via joining before the resolution of `'scope`.
            mem::transmute::<Box<Task + Send + 'scope>,
                             Box<Task + Send + 'static>>(Box::new(job))
        };

        // Submit the task to be executed.
        self.pool.queue.push(PoolMessage::Task(task, self.wait.clone()));
    }

    /// Add a job to this scope which itself will get access to the scope.
    ///
    /// Like with `execute`, subsequent calls to `join` will wait for this
    /// job (and all jobs scheduled on the scope it receives) to complete.
    pub fn recurse<F>(&self, job: F)
    where F: FnOnce(&Self) + Send + 'scope {
        // Create another scope with the *same* lifetime.
        let this = unsafe { self.clone() };

        self.execute(move || job(&this));
    }

    /// Create a new subscope, bound to a lifetime smaller than our existing Scope.
    ///
    /// The subscope has a different job set, and is joined before zoom returns.
    pub fn zoom<'smaller, F, R>(&self, scheduler: F) -> R
    where F: FnOnce(&Scope<'smaller>) -> R,
          'scope: 'smaller {
        let scope = unsafe { self.refine::<'smaller>() };

        // Join the scope either on completion of the scheduler or panic.
        defer!(scope.join());

        // Schedule all tasks then join all tasks
        scheduler(&scope)
    }

    /// Awaits all jobs submitted on this Scope to be completed.
    ///
    /// Only guaranteed to join jobs which where `execute`d logically
    /// prior to `join`. Jobs `execute`d concurrently with `join` may
    /// or may not be completed before `join` returns.
    #[inline]
    pub fn join(&self) {
        self.wait.join()
    }

    #[inline]
    unsafe fn clone(&self) -> Self {
        Scope {
            pool: self.pool.clone(),
            wait: self.wait.clone(),
            _scope: Id::default()
        }
    }

    // Create a new scope with a smaller lifetime on the same pool.
    #[inline]
    unsafe fn refine<'other>(&self) -> Scope<'other> where 'scope: 'other {
        Scope {
            pool: self.pool.clone(),
            wait: Arc::new(WaitGroup::new()),
            _scope: Id::default()
        }
    }
}

enum PoolMessage {
    Quit,
    Task(Box<Task + Send>, Arc<WaitGroup>)
}

/// A synchronization primitive for awaiting a set of actions.
///
/// Adding new jobs is done with `submit`, jobs are completed with `complete`,
/// and any thread may wait for all jobs to be `complete`d with `join`.
pub struct WaitGroup {
    // The lock and condition variable the joining threads
    // use to wait for the active tasks to complete.
    //
    // If the state is set to None, the group is poisoned.
    state: Mutex<WaitGroupState>,
    cond: Condvar
}

struct WaitGroupState {
    pending: usize,
    poisoned: bool
}

impl WaitGroup {
    /// Create a new empty WaitGroup.
    #[inline]
    pub fn new() -> Self {
        WaitGroup {
            state: Mutex::new(WaitGroupState {
                pending: 0,
                poisoned: false
            }),
            cond: Condvar::new()
        }
    }

    /// How many submitted tasks are waiting for completion.
    #[inline]
    pub fn waiting(&self) -> usize {
        self.state.lock().unwrap().pending
    }

    /// Submit to this WaitGroup, causing `join` to wait
    /// for an additional `complete`.
    #[inline]
    pub fn submit(&self) {
        self.state.lock().unwrap().pending += 1;
    }

    /// Complete a previous `submit`.
    #[inline]
    pub fn complete(&self) {
        let mut state = self.state.lock().unwrap();

        // Mark the current job complete.
        state.pending -= 1;

        // If that was the last job, wake joiners.
        if state.pending == 0 {
            self.cond.notify_all()
        }
    }

    /// Poison the WaitGroup so all `join`ing threads panic.
    #[inline]
    pub fn poison(&self) {
        let mut state = self.state.lock().unwrap();

        // Set the poison flag to false.
        state.poisoned = true;

        // Mark the current job complete.
        state.pending -= 1;

        // If that was the last job, wake joiners.
        if state.pending == 0 {
            self.cond.notify_all()
        }
    }

    /// Wait for `submit`s to this WaitGroup to be `complete`d.
    ///
    /// Submits occuring completely before joins will always be waited on.
    ///
    /// Submits occuring concurrently with a `join` may or may not
    /// be waited for.
    ///
    /// Before submitting, `join` will always return immediately.
    #[inline]
    pub fn join(&self) {
        let mut lock = self.state.lock().unwrap();

        while lock.pending > 0 {
            lock = self.cond.wait(lock).unwrap();
        }

        if lock.poisoned {
            panic!("WaitGroup explicitly poisoned!")
        }
    }
}

// Poisons the given pool on drop unless canceled.
//
// Used to ensure panic propogation between jobs and waiting threads.
struct Sentinel(Pool, Option<Arc<WaitGroup>>);

impl Sentinel {
    fn cancel(mut self) {
        self.1.take().map(|wait| wait.complete());
    }
}

impl Drop for Sentinel {
    fn drop(&mut self) {
        self.1.take().map(|wait| wait.poison());
    }
}

struct ThreadSentinel(Option<Pool>);

impl ThreadSentinel {
    fn cancel(&mut self) {
        self.0.take().map(|pool| {
            pool.wait.complete();
        });
    }
}

impl Drop for ThreadSentinel {
    fn drop(&mut self) {
        self.0.take().map(|pool| {
            // NOTE: We restart the thread first so we don't accidentally
            // hit zero threads before restarting.

            // Restart the thread.
            pool.expand();

            // Poison the pool.
            pool.wait.poison();
        });
    }
}

trait Task {
    fn run(self: Box<Self>);
}

impl<F: FnOnce()> Task for F {
    fn run(self: Box<Self>) { (*self)() }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use std::thread::sleep;

    use {Pool, Scope};

    #[test]
    fn test_simple_use() {
        let pool = Pool::new(4);

        let mut buf = [0, 0, 0, 0];

        pool.scoped(|scope| {
            for i in &mut buf {
                scope.execute(move || *i += 1);
            }
        });

        assert_eq!(&buf, &[1, 1, 1, 1]);
    }

    #[test]
    fn test_zoom() {
        let pool = Pool::new(4);

        let mut outer = 0;

        pool.scoped(|scope| {
            let mut inner = 0;
            scope.zoom(|scope2| scope2.execute(|| inner = 1));
            assert_eq!(inner, 1);

            outer = 1;
        });

        assert_eq!(outer, 1);
    }

    #[test]
    fn test_recurse() {
        let pool = Pool::new(12);

        let mut buf = [0, 0, 0, 0];

        pool.scoped(|next| {
            next.recurse(|next| {
                buf[0] = 1;

                next.execute(|| {
                    buf[1] = 1;
                });
            });
        });

        assert_eq!(&buf, &[1, 1, 0, 0]);
    }

    #[test]
    fn test_spawn_doesnt_hang() {
        let pool = Pool::new(1);
        pool.spawn(move || loop {});
    }

    #[test]
    fn test_forever_zoom() {
        let pool = Pool::new(16);
        let forever = Scope::forever(pool.clone());

        let ran = AtomicBool::new(false);

        forever.zoom(|scope| scope.execute(|| ran.store(true, Ordering::SeqCst)));

        assert!(ran.load(Ordering::SeqCst));
    }

    #[test]
    fn test_shutdown() {
        let pool = Pool::new(4);
        pool.shutdown();
    }

    #[test]
    #[should_panic]
    fn test_scheduler_panic() {
        let pool = Pool::new(4);
        pool.scoped(|_| panic!());
    }

    #[test]
    #[should_panic]
    fn test_scoped_execute_panic() {
        let pool = Pool::new(4);
        pool.scoped(|scope| scope.execute(|| panic!()));
    }

    #[test]
    #[should_panic]
    fn test_pool_panic() {
        let _pool = Pool::new(1);
        panic!();
    }

    #[test]
    #[should_panic]
    fn test_zoomed_scoped_execute_panic() {
        let pool = Pool::new(4);
        pool.scoped(|scope| scope.zoom(|scope2| scope2.execute(|| panic!())));
    }

    #[test]
    #[should_panic]
    fn test_recurse_scheduler_panic() {
        let pool = Pool::new(4);
        pool.scoped(|scope| scope.recurse(|_| panic!()));
    }

    #[test]
    #[should_panic]
    fn test_recurse_execute_panic() {
        let pool = Pool::new(4);
        pool.scoped(|scope| scope.recurse(|scope2| scope2.execute(|| panic!())));
    }

    struct Canary<'a> {
        drops: DropCounter<'a>,
        expected: usize
    }

    #[derive(Clone)]
    struct DropCounter<'a>(&'a AtomicUsize);

    impl<'a> Drop for DropCounter<'a> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl<'a> Drop for Canary<'a> {
        fn drop(&mut self) {
            let drops = self.drops.0.load(Ordering::SeqCst);
            assert_eq!(drops, self.expected);
        }
    }

    #[test]
    #[should_panic]
    fn test_scoped_panic_waits_for_all_tasks() {
        let tasks = 50;
        let panicking_task_fraction = 10;
        let panicking_tasks = tasks / panicking_task_fraction;
        let expected_drops = tasks + panicking_tasks;

        let counter = Box::new(AtomicUsize::new(0));
        let drops = DropCounter(&*counter);

        // Actual check occurs on drop of this during unwinding.
        let _canary = Canary {
            drops: drops.clone(),
            expected: expected_drops
        };

        let pool = Pool::new(12);

        pool.scoped(|scope| {
            for task in 0..tasks {
                let drop_counter = drops.clone();

                scope.execute(move || {
                    sleep(Duration::from_millis(10));

                    drop::<DropCounter>(drop_counter);
                });

                if task % panicking_task_fraction == 0 {
                    let drop_counter = drops.clone();

                    scope.execute(move || {
                        // Just make sure we capture it.
                        let _drops = drop_counter;
                        panic!();
                    });
                }
            }
        });
    }

    #[test]
    #[should_panic]
    fn test_scheduler_panic_waits_for_tasks() {
        let tasks = 50;
        let counter = Box::new(AtomicUsize::new(0));
        let drops = DropCounter(&*counter);

        let _canary = Canary {
            drops: drops.clone(),
            expected: tasks
        };

        let pool = Pool::new(12);

        pool.scoped(|scope| {
            for _ in 0..tasks {
                let drop_counter = drops.clone();

                scope.execute(move || {
                    sleep(Duration::from_millis(25));
                    drop::<DropCounter>(drop_counter);
                });
            }

            panic!();
        });
    }
}

