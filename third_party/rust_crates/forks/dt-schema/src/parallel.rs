// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::{
    io::Write,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

struct SharedMutableState<I, E> {
    results: Vec<Result<Vec<I>, E>>,
    abort: bool,
}

struct SharedState<I, E> {
    mutable: Mutex<SharedMutableState<I, E>>,
    abort_cv: Condvar,
    work_done: AtomicUsize,
}

impl<I, E> SharedState<I, E> {
    fn new() -> Arc<Self> {
        Arc::new(SharedState {
            mutable: Mutex::new(SharedMutableState {
                results: vec![],
                abort: false,
            }),
            abort_cv: Condvar::new(),
            work_done: AtomicUsize::new(0),
        })
    }

    fn add_result(&self, res: Result<Vec<I>, E>) {
        let mut mutable = self.mutable.lock().unwrap();
        if res.is_err() {
            mutable.abort = true;
        }
        mutable.results.push(res);
        self.abort_cv.notify_all();
    }

    fn did_work(&self) {
        self.work_done.fetch_add(1, Ordering::Relaxed);
    }

    fn wait_for_finish(&self) -> Result<usize, ()> {
        let lock = self.mutable.lock().unwrap();
        let (lock, _) = self
            .abort_cv
            .wait_timeout(lock, Duration::from_millis(400))
            .unwrap();
        let should_abort = lock.abort;
        if should_abort {
            Err(())
        } else {
            Ok(self.work_done.load(Ordering::SeqCst))
        }
    }

    fn get_results(self: Arc<Self>) -> Result<Vec<I>, E> {
        let nested: Result<Vec<Vec<_>>, _> = Arc::try_unwrap(self)
            .unwrap_or_else(|_| panic!("stray references")) // Safe because this is called after all other references are dealt with.
            .mutable
            .into_inner()
            .unwrap()
            .results
            .into_iter()
            .collect();
        Ok(nested?.into_iter().flatten().collect())
    }
}

pub fn parallel<'a, Arg, I, E>(
    callback: Arc<dyn Fn(&'a Arg) -> Result<I, E> + Sync + Send + 'a>,
    values: &'a [Arg],
    progress_text: &str,
) -> Result<Vec<I>, E>
where
    I: Send + Sync + 'a,
    E: Send + Sync + 'a,
    Arg: Sync,
{
    let threads = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(4);

    let work_per_thread = values.len() / threads;
    let state = SharedState::<I, E>::new();

    let start = Instant::now();
    std::thread::scope(|s| {
        #[allow(clippy::needless_collect)]
        let threads = values
            .chunks(work_per_thread)
            .map(|chunk| {
                let f = Arc::clone(&callback);
                let state = Arc::clone(&state);

                s.spawn(move || {
                    let mut thread_results = vec![];
                    for item in chunk.iter() {
                        match f(item) {
                            Ok(v) => thread_results.push(v),
                            Err(e) => {
                                state.add_result(Err(e));
                                break;
                            }
                        }
                        state.did_work();
                    }
                    state.add_result(Ok(thread_results));
                })
            })
            .collect::<Vec<_>>();

        let mut should_abort = false;
        while !should_abort {
            should_abort = match state.wait_for_finish() {
                Ok(progress) => {
                    print!(
                        "{}: {}/{}... ({:.02}s)\r",
                        progress_text,
                        progress,
                        values.len(),
                        start.elapsed().as_secs_f64()
                    );
                    let _ = std::io::stdout().flush();
                    progress == values.len()
                }
                Err(_) => true,
            };
        }

        for thread in threads.into_iter() {
            thread.join().unwrap();
        }
    });

    println!(
        "{}: done in {:.02}s                          ",
        progress_text,
        start.elapsed().as_secs_f64()
    );

    state.get_results()
}
