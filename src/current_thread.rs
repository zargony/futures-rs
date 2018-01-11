//! Execute tasks on the current thread
//!
//! This module implements an executor that keeps futures on the same thread
//! that they are submitted on. This allows it to execute futures that are
//! not `Send`. For more details on general executor concepts, like executing
//! futures, see [here].
//!
//! Before being able to spawn futures with this module, an executor
//! context must be setup. This is done by calling either [`block_on_all`] or
//! [`block_with_init`]. From within that context [`spawn`] or [`spawn_daemon`]
//! may be called with the future to run in the background.
//!
//! ```
//! # use futures::current_thread::*;
//! use futures::future::lazy;
//!
//! // Calling execute here results in a panic
//! // spawn(my_future);
//!
//! block_with_init(|_| {
//!     // The execution context is setup, futures may be executed.
//!     spawn(lazy(|| {
//!         println!("called from the current thread executor");
//!         Ok(())
//!     }));
//! });
//! ```
//!
//! # Execution model
//!
//! When an execution context is setup with `block_on_all` or `block_with_init`,
//! the current thread will block and all the futures managed by the executor
//! are driven to completion. Whenever a future receives a notification, it is
//! pushed to the end of a scheduled list. The executor will drain this list,
//! advancing the state of each future.
//!
//! All futures managed by this module will remain on the current thread,
//! as such, this module is able to safely execute futures that are not `Send`.
//!
//! Once a future is complete, it is dropped. Once all [non
//! daemon](#daemon-futures) futures are completed, [`block_on_all`] or
//! [`block_with_init`] will unblock.
//!
//! This module makes a best effort to fairly schedule futures that it manages.
//!
//! # Daemon Futures
//!
//! A daemon future is a future that does not require to be complete in order
//! for [`block_on_all`] or [`block_with_init`] to complete running. These are
//! useful for background "maintenance" tasks that are not critical to the
//! completion of the primary computation.
//!
//! When the execution functions finish running and unblock, any daemon futures
//! that have not yet completed are immediately dropped.
//!
//! A daemon future can be executed with [`spawn_daemon`].
//!
//! [here]: https://tokio.rs/docs/going-deeper-futures/tasks/
//! [`spawn_daemon`]: fn.spawn_daemon.html
//! [`spawn`]: fn.spawn.html
//! [`block_on_all`]: fn.block_on_all.html
//! [`block_on_init`]: fn.block_on_init.html

use Async;
use executor::{self, Spawn, Sleep, Wakeup};
use future::{Future, Executor, ExecuteError, ExecuteErrorKind};
use scheduler;
use task_impl::ThreadNotify;

use std::prelude::v1::*;

use std::{fmt, thread};
use std::cell::Cell;
use std::rc::Rc;

/// Executes futures on the current thread.
///
/// All futures executed using this executor will be executed on the current
/// thread as non-daemon futures. As such, `block_on_all` and `block_with_init`
/// will wait for these futures to complete before returning.
///
/// For more details, see the [module level](index.html) documentation.
#[derive(Debug, Clone)]
pub struct TaskExecutor {
    // Prevent the handle from moving across threads.
    _p: ::std::marker::PhantomData<Rc<()>>,
}

/// Executes daemon futures on the current thread.
///
/// All futures executed using this executor will be executed on the current
/// thread as daemon futures. As such, `block_on_all` and `block_with_init` will
/// **not** wait for these futures to complete before returning from `run`.
///
/// For more details, see the [module level](index.html) documentation.
#[derive(Debug, Clone)]
pub struct DaemonExecutor {
    // Prevent the handle from moving across threads.
    _p: ::std::marker::PhantomData<Rc<()>>,
}

/// A context yielded to the closure provided to `block_with_init`.
///
/// This context is mostly a future-proofing of the library to add future
/// contextual information into it. Currently it only contains the `Enter`
/// instance used to reserve the current thread for blocking on futures.
#[derive(Debug)]
pub struct Context<'a> {
    enter: executor::Enter,
    _p: ::std::marker::PhantomData<&'a ()>,
}

/// Implements the "blocking" logic for the current thread executor. A
/// `TaskRunner` will be created during `run` and will sit on the stack until
/// execution is complete.
#[derive(Debug)]
struct TaskRunner<T> {
    /// Executes futures.
    scheduler: Scheduler<T>,
}

struct CurrentRunner {
    /// When set to true, the executor should return immediately, even if there
    /// still are non-daemon futures to run.
    cancel: Cell<bool>,

    /// Number of non-daemon futures currently being executed by the runner.
    non_daemons: Cell<usize>,

    /// Raw pointer to the current scheduler pusher.
    ///
    /// The raw pointer is required in order to store it in a thread-local slot.
    schedule: Cell<Option<*mut Schedule>>,
}

type Scheduler<T> = scheduler::Scheduler<SpawnedFuture, T>;
type Schedule = scheduler::Schedule<SpawnedFuture>;

#[derive(Debug)]
struct SpawnedFuture {
    /// True if the executed future should not prevent the executor from
    /// terminating.
    daemon: bool,

    /// The task to execute.
    inner: Task,
}

struct Task(Spawn<Box<Future<Item = (), Error = ()>>>);

/// Current thread's task runner. This is set in `TaskRunner::with`
thread_local!(static CURRENT: CurrentRunner = CurrentRunner {
    cancel: Cell::new(false),
    non_daemons: Cell::new(0),
    schedule: Cell::new(None),
});

/// Calls the given closure, then block until all futures submitted for
/// execution complete.
///
/// In more detail, this function will block until:
/// - All executing futures are complete, or
/// - `cancel_all_executing` is invoked.
pub fn block_with_init<F, R>(f: F) -> R
where F: FnOnce(&mut Context) -> R
{
    ThreadNotify::with_current(|mut thread_notify| {
        TaskRunner::enter(&mut thread_notify, f)
    })
}

/// Calls the given closure with a custom sleep strategy.
///
/// This function is the same as `run` except that it allows customizing the
/// sleep strategy.
pub fn block_with_init_and_sleep<S, F, R>(sleep: &mut S, f: F) -> R
where F: FnOnce(&mut Context) -> R,
      S: Sleep,
{
    TaskRunner::enter(sleep, f)
}

/// Executes a future on the current thread.
///
/// The provided future must complete or be canceled before
/// `run` will return.
///
/// # Panics
///
/// This function can only be invoked from the context of a
/// `run` call; any other use will result in a panic.
pub fn spawn<F>(future: F)
where F: Future<Item = (), Error = ()> + 'static
{
    execute(future, false).unwrap_or_else(|_| {
        panic!("cannot call `execute` unless the thread is already \
                in the context of a call to `block_on_all` or `block_with_init`")
    })
}

/// Executes a daemon future on the current thread.
///
/// Completion of the provided future is not required for the pending
/// `run` call to complete. If `run` returns before `future` completes, it
/// will be dropped.
///
/// # Panics
///
/// This function can only be invoked from the context of a
/// `run` call; any other use will result in a panic.
pub fn spawn_daemon<F>(future: F)
where F: Future<Item = (), Error = ()> + 'static
{
    execute(future, true).unwrap_or_else(|_| {
        panic!("cannot call `execute` unless the thread is already \
                in the context of a call to `block_on_all` or `block_with_init`")
    })
}

/// Cancels *all* executing futures.
///
/// This cancels both daemon and non-daemon futures.
///
/// # Panics
///
/// This function can only be invoked from the context of a
/// `run` call; any other use will result in a panic.
pub fn cancel_all_spawned() {
    CurrentRunner::with(|runner| runner.cancel_all_executing())
        .unwrap_or_else(|()| {
            panic!("cannot call `cancel_all_executing` unless the thread is already \
                    in the context of a call to `block_on_all` or `block_with_init`")
        })
}

/// Returns an executor that executes futures on the current thread.
///
/// The user of `TaskExecutor` must ensure that when a future is submitted,
/// that it is done within the context of a call to either `block_on_all` or
/// `block_with_init`.
///
/// For more details, see the [module level](index.html) documentation.
pub fn task_executor() -> TaskExecutor {
    TaskExecutor {
        _p: ::std::marker::PhantomData,
    }
}

impl<F> Executor<F> for TaskExecutor
where F: Future<Item = (), Error = ()> + 'static
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        execute(future, false)
    }
}

/// Returns an executor that executes daemon futures on the current thread.
///
/// The user of `DaemonExecutor` must ensure that when a future is submitted,
/// that it is done from the context a call to either `block_on_all` or
/// `block_with_init`.
///
/// For more details, see the [module level](index.html) documentation.
pub fn daemon_executor() -> DaemonExecutor {
    DaemonExecutor {
        _p: ::std::marker::PhantomData,
    }
}

impl<F> Executor<F> for DaemonExecutor
where F: Future<Item = (), Error = ()> + 'static
{
    fn execute(&self, future: F) -> Result<(), ExecuteError<F>> {
        execute(future, true)
    }
}

impl<'a> Context<'a> {
    /// Returns a reference to the executor `Enter` handle.
    ///
    /// This can be used to schedule callbacks to run when the `Enter` scope is
    /// exited or otherwise pass the instance of `Enter` to other APIs which
    /// work with `Enter`.
    pub fn enter(&self) -> &executor::Enter {
        &self.enter
    }
}

/// Submits a future to the current executor. This is done by
/// checking the thread-local variable tracking the current executor.
///
/// If this function is not called in context of an executor, i.e. outside of
/// `run`, then `Err` is returned.
///
/// This function does not panic.
fn execute<F>(future: F, daemon: bool) -> Result<(), ExecuteError<F>>
where F: Future<Item = (), Error = ()> + 'static,
{
    CURRENT.with(|current| {
        match current.schedule.get() {
            Some(schedule) => {
                let spawned = SpawnedFuture {
                    daemon: daemon,
                    inner: Task::new(future),
                };

                if !daemon {
                    let non_daemons = current.non_daemons.get();
                    current.non_daemons.set(non_daemons + 1);
                }

                unsafe { (*schedule).schedule(spawned); }

                Ok(())
            }
            None => {
                Err(ExecuteError::new(ExecuteErrorKind::Shutdown, future))
            }
        }
    })
}

impl<T> TaskRunner<T>
where T: Wakeup,
{
    /// Return a new `TaskRunner`
    fn new(wakeup: T) -> TaskRunner<T> {
        let scheduler = scheduler::Scheduler::new(wakeup);

        TaskRunner {
            scheduler: scheduler,
        }
    }

    /// Enter a new `TaskRunner` context
    ///
    /// This function handles advancing the scheduler state and blocking while
    /// listening for notified futures.
    ///
    /// First, a new task runner is created backed by the current `ThreadNotify`
    /// handle. Passing `ThreadNotify` into the scheduler is how scheduled
    /// futures unblock the thread, signalling that there is more work to do.
    ///
    /// Before any future is polled, the scheduler must be set to a thread-local
    /// variable so that `execute` is able to submit new futures to the current
    /// executor. Because `Scheduler::schedule` requires `&mut self`, this
    /// introduces a mutability hazard. This hazard is minimized with some
    /// indirection. See `set_schedule` for more details.
    ///
    /// Once all context is setup, the init closure is invoked. This is the
    /// "boostrapping" process that executes the initial futures into the
    /// scheduler. After this, the function loops and advances the scheduler
    /// state until all non daemon futures complete. When no scheduled futures
    /// are ready to be advanced, the thread is blocked using
    /// `ThreadNotify::park`.
    fn enter<S, F, R>(sleep: &mut S, f: F) -> R
    where F: FnOnce(&mut Context) -> R,
          S: Sleep<Wakeup = T>,
    {
        let mut runner = TaskRunner::new(sleep.wakeup());

        CURRENT.with(|current| {
            // Make sure that another task runner is not set.
            //
            // This should not be ever possible due to how `set_schedule`
            // is setup, but better safe than sorry!
            assert!(current.schedule.get().is_none());

            let enter = executor::enter()
                .expect("cannot execute `current_thread` executor from within \
                         another executor");

            // Enter an execution scope
            let mut ctx = Context {
                enter: enter,
                _p: ::std::marker::PhantomData,
            };

            // Set the scheduler to the TLS and perform setup work,
            // returning a future to execute.
            //
            // This could possibly suubmit other futures for execution.
            let ret = current.set_schedule(&mut runner.scheduler as &mut Schedule, || {
                f(&mut ctx)
            });

            // Execute the runner.
            //
            // This function will not return until either
            //
            // a) All non daemon futures have completed execution
            // b) `cancel_all_executing` is called, forcing the executor to
            // return.
            runner.run(sleep, current);

            // Not technically required, but this makes the fact that `ctx`
            // needs to live until this point explicit.
            drop(ctx);

            ret
        })
    }

    fn run<S>(&mut self, sleep: &mut S, current: &CurrentRunner)
    where S: Sleep<Wakeup = T>,
    {
        use scheduler::Tick;

        while current.is_running() {
            // Try to advance the scheduler state
            let res = self.scheduler.tick(|scheduler, spawned, notify| {
                // `scheduler` is a `&mut Scheduler` reference returned back
                // from the scheduler to us, but only within the context of this
                // closure.
                //
                // This lets us push new futures into the scheduler. It also
                // lets us pass the scheduler mutable reference into
                // `set_schedule`, which sets the thread-local variable that
                // `spawn` uses for submitting new futures to the
                // "current" executor.
                //
                // See `set_schedule` documentation for more details on how we
                // guard against mutable pointer aliasing.
                current.set_schedule(scheduler as &mut Schedule, || {
                    match spawned.inner.0.poll_future_notify(notify, 0) {
                        Ok(Async::Ready(_)) | Err(_) => {
                            Async::Ready(spawned.daemon)
                        }
                        Ok(Async::NotReady) => Async::NotReady,
                    }
                })
            });

            // Process the result of ticking the scheduler
            match res {
                // A future completed. `is_daemon` is true when the future was
                // submitted as a daemon future.
                Tick::Data(is_daemon) => {
                    if !is_daemon {
                        let non_daemons = current.non_daemons.get();
                        debug_assert!(non_daemons > 0);
                        current.non_daemons.set(non_daemons - 1);
                    }
                },
                Tick::Empty => {
                    // The scheduler did not have any work to process.
                    //
                    // At this point, the scheduler is currently running given
                    // that the `while` condition was true and no user code has
                    // been executed.

                    debug_assert!(current.is_running());

                    // Block the current thread until a future managed by the scheduler
                    // receives a readiness notification.
                    sleep.sleep();
                }
                Tick::Inconsistent => {
                    // Yield the thread and loop
                    thread::yield_now();
                }
            }
        }
    }
}

impl CurrentRunner {
    fn with<F, R>(f: F) -> Result<R, ()>
    where F: FnOnce(&Self) -> R,
    {
        CURRENT.with(|current| {
            if current.schedule.get().is_some() {
                Ok(f(current))
            } else {
                Err(())
            }
        })
    }

    /// Set the provided schedule handle to the TLS slot for the duration of the
    /// closure.
    ///
    /// `spawn` will access the CURRENT thread-local variable in
    /// order to push a future into the scheduler. This requires a `&mut`
    /// reference, introducing mutability hazards.
    ///
    /// Rust requires that `&mut` references are not aliases, i.e. there are
    /// never two "live" mutable references to the same piece of data. In order
    /// to store a `&mut` reference in a thread-local variable, we must ensure
    /// that one can not access the scheduler anywhere else.
    ///
    /// To do this, we only allow access to the thread local variable from
    /// within the closure passed to `set_schedule`. This function also takes a
    /// &mut reference to the scheduler, which is essentially holding a "lock"
    /// on that reference, preventing any other location in the code from
    /// also getting that &mut reference.
    ///
    /// When `set_schedule` returns, the thread-local variable containing the
    /// mut reference is set to null. This is done even if the closure panics.
    ///
    /// This reduces the odds of introducing pointer aliasing.
    fn set_schedule<F, R>(&self, schedule: &mut Schedule, f: F) -> R
    where F: FnOnce() -> R
    {
        // Ensure that the runner is removed from the thread-local context
        // when leaving the scope. This handles cases that involve panicking.
        struct Reset<'a>(&'a CurrentRunner);

        impl<'a> Drop for Reset<'a> {
            fn drop(&mut self) {
                self.0.schedule.set(None);
            }
        }

        let _reset = Reset(self);

        self.schedule.set(Some(schedule as *mut Schedule));

        f()
    }

    fn is_running(&self) -> bool {
        self.non_daemons.get() > 0 && !self.cancel.get()
    }

    fn cancel_all_executing(&self) {
        self.cancel.set(true);
    }
}

impl Task {
    fn new<T: Future<Item = (), Error = ()> + 'static>(f: T) -> Self {
        Task(executor::spawn(Box::new(f)))
    }
}

impl fmt::Debug for Task {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Task")
            .finish()
    }
}
