// adaptive thread pool, stolen from sled
use super::{Promise, PromiseResolver};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use once_cell::sync::Lazy;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

/// Spawn a function on the threadpool.
pub(super) fn spawn<F, R>(work: F) -> Promise<R>
where
    F: FnOnce(PromiseResolver<R>) + Send + 'static,
    R: Send + 'static,
{
    let (resolver, promise) = Promise::pair();
    let rejecter = resolver.clone();
    let resolver_work = resolver.clone();
    let task = move || {
        work(resolver_work);
    };

    const MAX_THREADS: u64 = 256;

    static DYNAMIC_THREAD_COUNT: AtomicU64 = AtomicU64::new(0);

    struct Pool {
        sender: Sender<Box<dyn FnOnce() + Send + 'static>>,
        receiver: Receiver<Box<dyn FnOnce() + Send + 'static>>,
    }

    static POOL: Lazy<Pool, fn() -> Pool> = Lazy::new(init_pool);

    fn init_pool() -> Pool {
        for _ in 0..2 {
            thread::Builder::new()
                .spawn(|| {
                    for task in &POOL.receiver {
                        (task)()
                    }
                })
                .expect("cannot start a thread driving blocking tasks");
        }

        // We want to use an unbuffered channel here to help
        // us drive our dynamic control. In effect, the
        // kernel's scheduler becomes the queue, reducing
        // the number of buffers that work must flow through
        // before being acted on by a core. This helps keep
        // latency snappy in the overall async system by
        // reducing bufferbloat.
        let (sender, receiver) = bounded(0);
        Pool { sender, receiver }
    }

    // Create up to MAX_THREADS dynamic blocking task worker threads.
    // Dynamic threads will terminate themselves if they don't
    // receive any work after one second.
    fn maybe_create_another_blocking_thread() -> bool {
        // We use a `Relaxed` atomic operation because
        // it's just a heuristic, and would not lose correctness
        // even if it's random.
        let workers = DYNAMIC_THREAD_COUNT.load(Ordering::Relaxed);
        if workers >= MAX_THREADS {
            log::warn!(
                "Workers reaches the limit size: {}. \
                    Currently have {} dynamic threads",
                MAX_THREADS,
                workers
            );
            return false;
        }

        let spawn_res = thread::Builder::new().spawn(|| {
            let wait_limit = Duration::from_secs(1);

            DYNAMIC_THREAD_COUNT.fetch_add(1, Ordering::Relaxed);
            while let Ok(task) = POOL.receiver.recv_timeout(wait_limit) {
                (task)();
            }
            DYNAMIC_THREAD_COUNT.fetch_sub(1, Ordering::Relaxed);
        });

        if let Err(e) = spawn_res {
            log::warn!(
                "Failed to dynamically increase the threadpool size: {:?}. \
                    Currently have {} dynamic threads",
                e,
                workers
            );
            false
        } else {
            true
        }
    }

    match POOL.sender.try_send(Box::new(task)) {
        Ok(()) => {
            // everything is under control. 😊
        }
        Err(TrySendError::Full(task)) => {
            // enlarge the thread pool to receive more task. 👷
            if maybe_create_another_blocking_thread() {
                // Sender.send will wait for a receive operation to appear
                // on the other side of the channel.
                if POOL.sender.send(task).is_err() {
                    // this should never happen.
                    log::error!("threadpool is disconnected.");
                    rejecter.reject();
                }
            } else {
                // the thread pool is too full to receive task. 😖
                // we try to execute the task immediately.
                task();
            }
        }
        Err(TrySendError::Disconnected(task)) => {
            // this should never happen. 😖
            // but if happened, we try to execute the task immediately.
            log::error!(
                "unable to send to blocking threadpool \
                    due to receiver disconnection"
            );
            task();
        }
    }

    promise
}
