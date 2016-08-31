//! Tasks used to drive a future computation
//!
//! It's intended over time a particular operation (such as servicing an HTTP
//! request) will involve many futures. This entire operation, however, can be
//! thought of as one unit, as the entire result is essentially just moving
//! through one large state machine.
//!
//! A "task" is the unit of abstraction for what is driving this state machine
//! and tree of futures forward. A task is used to poll futures and schedule
//! futures with, and has utilities for sharing data between tasks and handles
//! for notifying when a future is ready. Each task also has its own set of
//! task-local data, which can be accessed at any point by the task's future;
//! see `with_local_data`.
//!
//! Note that libraries typically should not manage tasks themselves, but rather
//! leave that to event loops and other "executors", or by using the `wait`
//! method to create and execute a task directly on the current thread.
//!
//! There are two basic execution models for tasks: via an `Executor` (which is
//! generally one or more threads together with a queue of tasks to execute) or
//! by blocking on the current thread (via `ThreadTask`).
//!
//! ## Functions
//!
//! There is an important bare function in this module: `park`. The `park`
//! function is similar to the standard library's `thread::park` method where it
//! returns a handle to wake up a task at a later date (via an `unpark` method).

use std::prelude::v1::*;

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{Ordering, AtomicUsize, ATOMIC_USIZE_INIT};
use std::thread;

use {BoxFuture, Poll, Future};
use stream::Stream;
use task::unpark_mutex::UnparkMutex;

mod unpark_mutex;
mod task_rc;
mod data;
pub use self::task_rc::TaskRc;
pub use self::data::LocalKey;

thread_local!(static CURRENT_TASK: Cell<(*const Task, *const data::LocalMap)> = {
    Cell::new((0 as *const _, 0 as *const _))
});

fn fresh_task_id() -> usize {
    // TODO: this assert is a real bummer, need to figure out how to reuse
    //       old IDs that are no longer in use.
    static NEXT_ID: AtomicUsize = ATOMIC_USIZE_INIT;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    assert!(id < usize::max_value() / 2,
            "too many previous tasks have been allocated");
    return id
}

fn set<F, R>(task: &Task, data: &data::LocalMap, f: F) -> R
    where F: FnOnce() -> R
{
    struct Reset((*const Task, *const data::LocalMap));
    impl Drop for Reset {
        fn drop(&mut self) {
            CURRENT_TASK.with(|c| c.set(self.0));
        }
    }

    CURRENT_TASK.with(|c| {
        let _reset = Reset(c.get());
        c.set((task as *const _, data as *const _));
        f()
    })
}

fn with<F: FnOnce(&Task, &data::LocalMap) -> R, R>(f: F) -> R {
    let (task, data) = CURRENT_TASK.with(|c| c.get());
    assert!(!task.is_null(), "no Task is currently running");
    debug_assert!(!data.is_null());
    unsafe {
        f(&*task, &*data)
    }
}

/// Returns a handle to the current task to call `unpark` at a later date.
///
/// This function is similar to the standard library's `thread::park` function
/// except that it won't block the current thread but rather the current future
/// that is being executed.
///
/// The returned handle implements the `Send` and `'static` bounds and may also
/// be cheaply cloned. This is useful for squirreling away the handle into a
/// location which is then later signaled that a future can make progress.
///
/// Implementations of the `Future` trait typically use this function if they
/// would otherwise perform a blocking operation. When something isn't ready
/// yet, this `park` function is called to acquire a handle to the current
/// task, and then the future arranges it such that when the block operation
/// otherwise finishes (perhaps in the background) it will `unpark` the returned
/// handle.
///
/// It's sometimes necessary to pass extra information to the task when
/// unparking it, so that the task knows something about *why* it was woken. See
/// the `Task::with_unpark_event` for details on how to do this.
///
/// # Panics
///
/// This function will panic if a task is not currently being executed. That
/// is, this method can be dangerous to call outside of an implementation of
/// `poll`.
pub fn park() -> Task {
    with(|task, _| task.clone())
}

pub struct Spawn<T> {
    obj: T,
    id: usize,
    data: data::LocalMap,
}

pub fn spawn<T>(obj: T) -> Spawn<T> {
    Spawn {
        obj: obj,
        id: fresh_task_id(),
        data: data::local_map(),
    }
}

/// For the duration of the given callback, add an "unpark event" to be
/// triggered when the task handle is used to unpark the task.
///
/// Unpark events are used to pass information about what event caused a task to
/// be unparked. In some cases, tasks are waiting on a large number of possible
/// events, and need precise information about the wakeup to avoid extraneous
/// polling.
///
/// Every `Task` handle comes with a set of unpark events which will fire when
/// `unpark` is called. When fired, these events insert an identifer into a
/// concurrent set, which the task can read from to determine what events
/// occurred.
///
/// This function immediately invokes the closure, `f`, but arranges things so
/// that `task::park` will produce a `Task` handle that includes the given
/// unpark event.
///
/// # Panics
///
/// This function will panic if a task is not currently being executed. That
/// is, this method can be dangerous to call outside of an implementation of
/// `poll`.
pub fn with_unpark_event<F, R>(event: UnparkEvent, f: F) -> R
    where F: FnOnce() -> R
{
    with(|task, data| {
        let new_task = Task {
            id: task.id,
            unpark: task.unpark.clone(),
            events: task.events.with_event(event),
        };
        set(&new_task, data, f)
    })
}

/// A handle to a "task", which represents a single lightweight "thread" of
/// execution driving a future to completion.
///
/// In general, futures are composed into large units of work, which are then
/// spawned as tasks onto an *executor*. The executor is responible for polling
/// the future as notifications arrive, until the future terminates.
///
/// Obtained by the `task::park` function, or by binding to an executor through
/// the `Task::new` constructor.
#[derive(Clone)]
pub struct Task {
    id: usize,
    unpark: Arc<Unpark>,
    events: Events,
}

fn _assert_kinds() {
    fn _assert_send<T: Send>() {}
    _assert_send::<Task>();
}

impl<T> Spawn<T> {
    fn enter<F, R>(&mut self, unpark: Arc<Unpark>, f: F) -> R
        where F: FnOnce(&mut T) -> R
    {
        let task = Task {
            id: self.id,
            unpark: unpark,
            events: Events::new(),
        };
        let obj = &mut self.obj;
        set(&task, &self.data, || f(obj))
    }
}

pub trait Executor: Send + Sync + 'static {
    fn execute(&self, r: Run);
}

impl<F: Future> Spawn<F> {
    pub fn poll_future(&mut self, unpark: Arc<Unpark>) -> Poll<F::Item, F::Error> {
        self.enter(unpark, |f| f.poll())
    }

    pub fn wait_future(&mut self) -> Result<F::Item, F::Error> {
        let unpark = Arc::new(ThreadUnpark(thread::current()));
        loop {
            match self.poll_future(unpark.clone()) {
                Poll::Ok(e) => return Ok(e),
                Poll::Err(e) => return Err(e),
                Poll::NotReady => thread::park(),
            }
        }
    }
}

impl Spawn<BoxFuture<(), ()>> {
    pub fn execute(self, exec: Arc<Executor>) {
        exec.clone().execute(Run {
            spawn: self,
            inner: Arc::new(Inner {
                exec: exec,
                mutex: UnparkMutex::new()
            }),
        })
    }
}

impl<S: Stream> Spawn<S> {
    pub fn poll_stream(&mut self, unpark: Arc<Unpark>)
                       -> Poll<Option<S::Item>, S::Error> {
        self.enter(unpark, |stream| stream.poll())
    }

    pub fn wait_stream(&mut self) -> Option<Result<S::Item, S::Error>> {
        let unpark = Arc::new(ThreadUnpark(thread::current()));
        loop {
            match self.poll_stream(unpark.clone()) {
                Poll::Ok(Some(e)) => return Some(Ok(e)),
                Poll::Ok(None) => return None,
                Poll::Err(e) => return Some(Err(e)),
                Poll::NotReady => thread::park(),
            }
        }
    }
}

struct ThreadUnpark(thread::Thread);

impl Unpark for ThreadUnpark {
    fn unpark(&self) {
        self.0.unpark()
    }
}

pub struct Run {
    spawn: Spawn<BoxFuture<(), ()>>,
    inner: Arc<Inner>,
}

struct Inner {
    mutex: UnparkMutex<Run>,
    exec: Arc<Executor>,
}

impl Run {
    /// Actually run the task (invoking `poll` on its future) on the current
    /// thread.
    pub fn run(self) {
        let Run { mut spawn, inner } = self;

        // SAFETY: the ownership of this `Run` object is evidence that
        // we are in the `POLLING`/`REPOLL` state for the mutex.
        unsafe {
            inner.mutex.start_poll();

            loop {
                match spawn.poll_future(inner.clone()) {
                    Poll::NotReady => {}
                    Poll::Ok(()) |
                    Poll::Err(()) => return inner.mutex.complete(),
                }
                let run = Run { spawn: spawn, inner: inner.clone() };
                match inner.mutex.wait(run) {
                    Ok(()) => return,            // we've waited
                    Err(r) => spawn = r.spawn,   // someone's notified us
                }
            }
        }
    }
}

impl Unpark for Inner {
    fn unpark(&self) {
        match self.mutex.notify() {
            Ok(run) => self.exec.execute(run),
            Err(()) => {}
        }
    }
}

// A collection of UnparkEvents to trigger on `unpark`
#[derive(Clone)]
struct Events {
    set: Vec<UnparkEvent>, // TODO: change to some SmallVec
}

#[derive(Clone)]
/// A set insertion to trigger upon `unpark`.
///
/// Unpark events are used to communicate information about *why* an unpark
/// occured, in particular populating sets with event identifiers so that the
/// unparked task can avoid extraneous polling. See `with_unpark_event` for
/// more.
pub struct UnparkEvent {
    set: Arc<EventSet>,
    item: usize,
}

// /// A way of notifying a task that it should wake up and poll its future.
// pub trait Executor: Send + Sync + 'static {
//     /// Indicate that the task should attempt to poll its future in a timely
//     /// fashion. This is typically done when alerting a future that an event of
//     /// interest has occurred through `Task::unpark`.
//     ///
//     /// It must be guaranteed that, for each call to `notify`, `poll` will be
//     /// called at least once subsequently (unless the task has terminated). If
//     /// the task is currently polling its future when `notify` is called, it
//     /// must poll the future *again* afterwards, ensuring that all relevant
//     /// events are eventually observed by the future.
//     fn execute(&self, run: Run);
// }

/// A concurrent set which allows for the insertion of `usize` values.
///
/// `EventSet`s are used to communicate precise information about the event(s)
/// that trigged a task notification. See `task::with_unpark_event` for details.
pub trait EventSet: Send + Sync + 'static {
    /// Insert the given ID into the set
    fn insert(&self, id: usize);
}

pub trait Unpark: Send + Sync + 'static {
    fn unpark(&self);
}

impl Task {
    /// Indicate that the task should attempt to poll its future in a timely
    /// fashion. This is typically done when alerting a future that an event of
    /// interest has occurred through `Task::unpark`.
    ///
    /// It's guaranteed that, for each call to `notify`, `poll` will be called
    /// at least once subsequently (unless the task has terminated). If the task
    /// is currently polling its future when `notify` is called, it must poll
    /// the future *again* afterwards, ensuring that all relevant events are
    /// eventually observed by the future.
    pub fn unpark(&self) {
        self.events.trigger();
        self.unpark.unpark();
    }
}

impl Events {
    fn new() -> Events {
        Events { set: Vec::new() }
    }

    fn trigger(&self) {
        for event in self.set.iter() {
            event.set.insert(event.item)
        }
    }

    fn with_event(&self, event: UnparkEvent) -> Events {
        let mut set = self.set.clone();
        set.push(event);
        Events { set: set }
    }
}

impl UnparkEvent {
    /// Construct an unpark event that will insert `id` into `set` when
    /// triggered.
    pub fn new(set: Arc<EventSet>, id: usize) -> UnparkEvent {
        UnparkEvent {
            set: set,
            item: id,
        }
    }
}
