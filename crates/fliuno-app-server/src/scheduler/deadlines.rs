use super::*;

const MAX_CLOCK_RECHECK_MS: u64 = 250;

pub(super) fn start(
    kernel: Arc<Mutex<Kernel>>,
    active: ActiveTasks,
) -> (Sender<()>, JoinHandle<()>) {
    let (sender, receiver) = bounded(1);
    let worker = thread::Builder::new()
        .name("fliuno-run-deadlines".into())
        .spawn(move || {
            loop {
                let delay = {
                    let Ok(mut kernel) = kernel.lock() else {
                        return;
                    };
                    match kernel.expire_next_run() {
                        Ok(Some(ids)) => {
                            if let Err(error) =
                                signal_cancellations(&active, ids.iter().map(String::as_str))
                            {
                                eprintln!("fliuno: signalling expired run: {error}");
                            }
                            Ok(Some(0))
                        }
                        Ok(None) => kernel.next_run_deadline_delay_ms(),
                        Err(error) => Err(error),
                    }
                };
                let delay = match delay {
                    Ok(delay) => delay,
                    Err(error) => {
                        eprintln!("fliuno: checking run deadlines: {error}");
                        Some(MAX_CLOCK_RECHECK_MS)
                    }
                };
                match delay {
                    Some(ms) => {
                        let wait = Duration::from_millis(ms.min(MAX_CLOCK_RECHECK_MS));
                        if receiver.recv_timeout(wait)
                            == Err(crossbeam_channel::RecvTimeoutError::Disconnected)
                        {
                            return;
                        }
                    }
                    None => {
                        if receiver.recv().is_err() {
                            return;
                        }
                    }
                }
            }
        })
        .expect("cannot start run deadline worker");
    (sender, worker)
}
