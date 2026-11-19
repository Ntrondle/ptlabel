//! One worker thread per selected printer: status polling (tape detection) and a
//! print queue. Jobs are snapshots taken when Print is pressed; the worker starts
//! the next one whenever the printer reports ready (tape in, no error, idle).

use crate::job::{PrintJob, run_job};
use crate::label::render::Renderer;
use crate::proto::Status;
use crate::transport::{PrinterId, Transport};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub enum Cmd {
    /// Re-read status now (and start the next queued job if ready).
    Refresh,
    Close,
}

pub enum Event {
    Status(Status),
    Offline(String),
    Started { id: u64, title: String, total: Option<u32> },
    Progress { id: u64, done: u32, total: Option<u32> },
    Done { id: u64, printed: u32 },
    Failed { id: u64, error: String },
}

pub struct Queued {
    pub id: u64,
    pub title: String,
    pub total: Option<u32>,
    pub job: PrintJob,
}

pub struct PrinterHandle {
    pub id: PrinterId,
    tx: Sender<Cmd>,
    pub rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
    queue: Arc<Mutex<VecDeque<Queued>>>,
    next_id: u64,
}

fn ready(s: &Status) -> bool {
    s.errors().is_empty() && s.tape().is_some() && !s.is_printing()
}

impl PrinterHandle {
    pub fn spawn(id: PrinterId, renderer: Arc<Mutex<Renderer>>, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        let t = id.open();
        Self::spawn_with(id, t, renderer, notify)
    }

    pub fn spawn_with(
        id: PrinterId,
        mut t: Box<dyn Transport>,
        renderer: Arc<Mutex<Renderer>>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (tx, crx) = unbounded::<Cmd>();
        let (etx, rx) = unbounded::<Event>();
        let stop = Arc::new(AtomicBool::new(false));
        let queue: Arc<Mutex<VecDeque<Queued>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (wstop, wqueue) = (stop.clone(), queue.clone());
        std::thread::Builder::new()
            .name(format!("printer {}", id.model))
            .spawn(move || {
                let send = |e: Event| {
                    let _ = etx.send(e);
                    notify();
                };
                let mut next_poll = Instant::now();
                loop {
                    let wait = next_poll.saturating_duration_since(Instant::now());
                    match crx.recv_timeout(wait) {
                        Ok(Cmd::Refresh) => next_poll = Instant::now(),
                        Ok(Cmd::Close) | Err(RecvTimeoutError::Disconnected) => {
                            t.close();
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            let interval = t.poll_interval();
                            let st = match t.status() {
                                Ok(s) => s,
                                Err(e) => {
                                    send(Event::Offline(format!("{e:#}")));
                                    next_poll = Instant::now() + interval.max(Duration::from_secs(5));
                                    continue;
                                }
                            };
                            let go = ready(&st);
                            send(Event::Status(st));
                            next_poll = Instant::now() + interval;
                            if !go {
                                continue;
                            }
                            let Some(q) = wqueue.lock().unwrap().pop_front() else { continue };
                            wstop.store(false, Ordering::SeqCst);
                            send(Event::Started { id: q.id, title: q.title.clone(), total: q.total });
                            let r = run_job(t.as_mut(), &renderer, &q.job, &wstop, &mut |done, total| {
                                send(Event::Progress { id: q.id, done, total })
                            });
                            match r {
                                Ok(printed) => send(Event::Done { id: q.id, printed }),
                                Err(e) => send(Event::Failed { id: q.id, error: format!("{e:#}") }),
                            }
                            // Re-check right away; the next job starts as soon as the printer is idle.
                            next_poll = Instant::now();
                        }
                    }
                }
            })
            .expect("spawn printer worker");
        Self { id, tx, rx, stop, queue, next_id: 1 }
    }

    /// Add a job to the end of the queue; returns its id.
    pub fn enqueue(&mut self, job: PrintJob, title: String, total: Option<u32>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.queue.lock().unwrap().push_back(Queued { id, title, total, job });
        let _ = self.tx.send(Cmd::Refresh);
        id
    }

    /// Waiting jobs (not the one printing): (id, title, total).
    pub fn queued(&self) -> Vec<(u64, String, Option<u32>)> {
        self.queue.lock().unwrap().iter().map(|q| (q.id, q.title.clone(), q.total)).collect()
    }

    pub fn cancel(&self, id: u64) {
        self.queue.lock().unwrap().retain(|q| q.id != id);
    }

    pub fn clear(&self) {
        self.queue.lock().unwrap().clear();
    }

    pub fn refresh(&self) {
        let _ = self.tx.send(Cmd::Refresh);
    }

    /// Graceful stop of the running job: the next label is the last one (fed + cut).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for PrinterHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::PrintSettings;
    use crate::label::Label;
    use crate::proto::Caps;
    use crate::transport::Link;
    use anyhow::Result;

    const READY: [u8; 32] = [
        0x80, 0x20, 0x42, 0x30, 0x66, 0x30, 0, 0, 0, 0, 12, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 8, 0, 0, 0,
        0, 0, 0,
    ];

    /// Fake printer: cover open for the first `cover_polls` status reads, then ready.
    struct Mock {
        cover_polls: Arc<Mutex<u32>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Transport for Mock {
        fn status(&mut self) -> Result<Status> {
            let mut b = READY;
            let mut c = self.cover_polls.lock().unwrap();
            if *c > 0 {
                *c -= 1;
                b[9] = 0x10;
            }
            Status::parse(&b)
        }
        fn begin(&mut self) -> Result<Status> {
            self.log.lock().unwrap().push("begin".into());
            Status::parse(&READY)
        }
        fn send_page(&mut self, data: &[u8], _: f32) -> Result<()> {
            let last = *data.last().unwrap() == 0x1A;
            self.log.lock().unwrap().push(if last { "page-last".into() } else { "page".into() });
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            self.log.lock().unwrap().push("finish".into());
            Ok(())
        }
        fn abort(&mut self) {}
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(10)
        }
    }

    fn job(copies: u32) -> PrintJob {
        PrintJob {
            label: Label::default(),
            settings: PrintSettings { copies, ..Default::default() },
            caps: Caps::default(),
            rows: vec![],
        }
    }

    #[test]
    fn queues_jobs_waits_for_ready_and_runs_in_order() {
        let cover = Arc::new(Mutex::new(3));
        let log = Arc::new(Mutex::new(vec![]));
        let id = PrinterId { model: "PT-E550W".into(), link: Link::Network("mock".into()) };
        let mock = Box::new(Mock { cover_polls: cover.clone(), log: log.clone() });
        let mut h = PrinterHandle::spawn_with(id, mock, Arc::new(Mutex::new(Renderer::new())), Arc::new(|| {}));
        let a = h.enqueue(job(2), "A".into(), Some(2));
        let b = h.enqueue(job(1), "B".into(), Some(1));
        let c = h.enqueue(job(1), "C".into(), Some(1));
        h.cancel(c);

        let mut order = vec![];
        let deadline = Instant::now() + Duration::from_secs(10);
        while order.len() < 4 && Instant::now() < deadline {
            match h.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Event::Started { id, .. }) => order.push(format!("start{id}")),
                Ok(Event::Done { id, printed }) => order.push(format!("done{id}x{printed}")),
                Ok(Event::Failed { error, .. }) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(order, vec![format!("start{a}"), format!("done{a}x2"), format!("start{b}"), format!("done{b}x1")]);
        assert_eq!(*cover.lock().unwrap(), 0, "waited through cover-open polls");
        assert_eq!(
            *log.lock().unwrap(),
            vec!["begin", "page", "page-last", "finish", "begin", "page-last", "finish"]
        );
        assert!(h.queued().is_empty());
    }
}
