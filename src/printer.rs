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

    fn spawn_with(
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
                // Last status or error sent: an unchanged poll doesn't wake (repaint) the UI.
                let mut last: Option<Result<Status, String>> = None;
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
                                    let e = format!("{e:#}");
                                    if last.as_ref() != Some(&Err(e.clone())) {
                                        send(Event::Offline(e.clone()));
                                        last = Some(Err(e));
                                    }
                                    next_poll = Instant::now() + interval.max(Duration::from_secs(5));
                                    continue;
                                }
                            };
                            let go = ready(&st);
                            if last.as_ref() != Some(&Ok(st.clone())) {
                                last = Some(Ok(st.clone()));
                                send(Event::Status(st));
                            }
                            next_poll = Instant::now() + interval;
                            if !go {
                                continue;
                            }
                            // Reset before taking the job, so a Stop from Drop can't be lost in between.
                            wstop.store(false, Ordering::SeqCst);
                            let Some(q) = wqueue.lock().unwrap().pop_front() else { continue };
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
        // Nobody is left to see or stop the jobs: drop the queue and end the running job
        // after its current label (fed + cut), then the worker exits.
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.stop();
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
    use anyhow::{Result, anyhow};

    const READY: [u8; 32] = [
        0x80, 0x20, 0x42, 0x30, 0x66, 0x30, 0, 0, 0, 0, 12, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 8, 0, 0, 0,
        0, 0, 0,
    ];

    /// Runs at each sent page (1-based, counted across jobs); an error fails that page.
    type OnPage = Box<dyn FnMut(usize) -> Result<()> + Send>;

    /// Fake printer: cover open for the first `cover_polls` status reads, then ready.
    struct Mock {
        cover_polls: Arc<Mutex<u32>>,
        log: Arc<Mutex<Vec<String>>>,
        on_page: Option<OnPage>,
        pages: usize,
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
            self.pages += 1;
            if let Some(f) = &mut self.on_page {
                f(self.pages)?;
            }
            let last = *data.last().unwrap() == 0x1A;
            self.log.lock().unwrap().push(if last { "page-last".into() } else { "page".into() });
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            self.log.lock().unwrap().push("finish".into());
            Ok(())
        }
        fn abort(&mut self) {
            self.log.lock().unwrap().push("abort".into());
        }
        fn close(&mut self) {
            self.log.lock().unwrap().push("close".into());
        }
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
        let mock = Box::new(Mock { cover_polls: cover.clone(), log: log.clone(), on_page: None, pages: 0 });
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

    fn spawn_mock(log: &Arc<Mutex<Vec<String>>>, on_page: Option<OnPage>) -> PrinterHandle {
        let id = PrinterId { model: "PT-P710BT".into(), link: Link::Bluetooth("mock".into()) };
        let mock = Box::new(Mock { cover_polls: Arc::new(Mutex::new(0)), log: log.clone(), on_page, pages: 0 });
        PrinterHandle::spawn_with(id, mock, Arc::new(Mutex::new(Renderer::new())), Arc::new(|| {}))
    }

    fn wait_for(log: &Arc<Mutex<Vec<String>>>, entry: &str) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !log.lock().unwrap().iter().any(|e| e == entry) {
            std::thread::sleep(Duration::from_millis(10));
        }
        log.lock().unwrap().clone()
    }

    #[test]
    fn dropping_handle_ends_running_job_cleanly_and_exits() {
        let log = Arc::new(Mutex::new(vec![]));
        let mut h = spawn_mock(&log, None);
        let mut inf = job(1);
        inf.settings.infinite = true;
        h.enqueue(inf, "inf".into(), None);
        h.enqueue(job(1), "next".into(), Some(1));
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Event::Progress { done: 2, .. }) = h.rx.recv_timeout(Duration::from_millis(200)) {
                break;
            }
        }
        drop(h); // UI gone (printer switched / app closed)
        let log = wait_for(&log, "close");
        assert_eq!(log.last().map(String::as_str), Some("close"), "worker exited: {log:?}");
        assert_eq!(&log[log.len() - 3..], ["page-last", "finish", "close"], "last label fed + cut");
        assert_eq!(log.iter().filter(|e| *e == "begin").count(), 1, "queued job not started");
    }

    #[test]
    fn error_mid_job_fails_it_cleanly_and_queue_continues() {
        let log = Arc::new(Mutex::new(vec![]));
        let on_page: OnPage = Box::new(|page| if page == 2 { Err(anyhow!("printer error: No tape")) } else { Ok(()) });
        let mut h = spawn_mock(&log, Some(on_page));
        let a = h.enqueue(job(3), "A".into(), Some(3));
        let b = h.enqueue(job(1), "B".into(), Some(1));
        let mut ev = vec![];
        let deadline = Instant::now() + Duration::from_secs(10);
        while ev.len() < 4 && Instant::now() < deadline {
            match h.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Event::Started { id, .. }) => ev.push(format!("start{id}")),
                Ok(Event::Done { id, printed }) => ev.push(format!("done{id}x{printed}")),
                Ok(Event::Failed { id, error }) => ev.push(format!("failed{id}: {error}")),
                _ => {}
            }
        }
        assert_eq!(ev, [format!("start{a}"), format!("failed{a}: label 2: printer error: No tape"), format!("start{b}"), format!("done{b}x1")]);
        assert_eq!(*log.lock().unwrap(), ["begin", "page", "abort", "begin", "page-last", "finish"]);
    }

    #[test]
    fn stop_while_rendering_makes_that_label_the_last() {
        use crate::label::{Block, TextBlock};
        let renderer = Arc::new(Mutex::new(Renderer::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, s) = (renderer.clone(), stop.clone());
        let log = Arc::new(Mutex::new(vec![]));
        let on_page: OnPage = Box::new(move |page| {
            if page == 1 {
                // Hold the renderer so label 2 is mid-render when Stop is pressed.
                let (tx, rx) = crossbeam_channel::bounded(0);
                let (r, s) = (r.clone(), s.clone());
                std::thread::spawn(move || {
                    let _g = r.lock().unwrap();
                    tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                    s.store(true, Ordering::SeqCst);
                });
                rx.recv().unwrap();
            }
            Ok(())
        });
        let mut t = Mock { cover_polls: Arc::new(Mutex::new(0)), log: log.clone(), on_page: Some(on_page), pages: 0 };
        let mut j = job(1);
        j.settings.infinite = true;
        j.label.blocks = vec![Block::Text(TextBlock { text: "#{n}".into(), ..Default::default() })];
        let n = run_job(&mut t, &renderer, &j, &stop, &mut |_, _| {}).unwrap();
        assert_eq!(n, 2);
        assert_eq!(*log.lock().unwrap(), ["begin", "page", "page-last", "finish"]);
    }

    #[test]
    fn unchanged_status_does_not_wake_the_ui() {
        use std::sync::atomic::AtomicUsize;
        let cover = Arc::new(Mutex::new(3));
        let wakes = Arc::new(AtomicUsize::new(0));
        let id = PrinterId { model: "PT-E550W".into(), link: Link::Network("mock".into()) };
        let mock = Box::new(Mock { cover_polls: cover.clone(), log: Arc::new(Mutex::new(vec![])), on_page: None, pages: 0 });
        let w = wakes.clone();
        let notify = Arc::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        let h = PrinterHandle::spawn_with(id, mock, Arc::new(Mutex::new(Renderer::new())), notify);
        // 3 identical cover-open polls, then ready: one event (and one wake) per change.
        let mut seen = vec![];
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.last() != Some(&false) && Instant::now() < deadline {
            if let Ok(Event::Status(s)) = h.rx.recv_timeout(Duration::from_millis(200)) {
                seen.push(!s.errors().is_empty());
            }
        }
        std::thread::sleep(Duration::from_millis(200)); // ~20 more identical polls
        assert!(h.rx.try_iter().next().is_none(), "unchanged polls send nothing");
        assert_eq!(seen, vec![true, false]);
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }
}
