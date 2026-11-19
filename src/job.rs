use crate::label::render::{Renderer, label_length_mm};
use crate::data::Record;
use crate::label::{Label, SequenceBlock, Vars};
use crate::proto::raster::encode_page;
use crate::proto::{Bitmap, Caps, PageOpts};
use crate::transport::Transport;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrintSettings {
    pub copies: u32,
    /// Keep printing until Stop (or the tape runs out).
    pub infinite: bool,
    pub opts: PageOpts,
    pub num_start: i64,
    pub num_step: i64,
}

impl Default for PrintSettings {
    fn default() -> Self {
        Self {
            copies: 1,
            infinite: false,
            opts: PageOpts::default(),
            num_start: 1,
            num_step: 1,
        }
    }
}

/// Maps a page index to its substitution values. A job prints `records` distinct
/// labels (selected spreadsheet rows, else the sequence count, else 1), each `copies` times.
pub struct Pager {
    copies: u32,
    start: i64,
    step: i64,
    seq: Option<SequenceBlock>,
    rows: Vec<Arc<Record>>,
    pub total: Option<u32>,
}

impl Pager {
    pub fn new(label: &Label, s: &PrintSettings, rows: Vec<Arc<Record>>) -> Self {
        let total = Self::counts(label, s, rows.len()).1;
        Self { copies: s.copies.max(1), start: s.num_start, step: s.num_step, seq: label.sequence().cloned(), rows, total }
    }

    /// (distinct labels, total labels) for `rows` selected spreadsheet rows; None = until stopped.
    pub fn counts(label: &Label, s: &PrintSettings, rows: usize) -> (Option<u32>, Option<u32>) {
        let seq_count = label
            .blocks
            .iter()
            .filter_map(|b| if let crate::label::Block::Sequence(q) = b { Some(q.count.max(1)) } else { None })
            .max();
        let records = if s.infinite {
            None
        } else if rows > 0 {
            Some(rows as u32)
        } else {
            Some(seq_count.unwrap_or(1))
        };
        (records, records.map(|r| r.saturating_mul(s.copies.max(1))))
    }

    /// Values for page `i` (copies of one record are consecutive pages).
    pub fn vars(&self, i: u32) -> Vars {
        let mut v = self.record_vars((i / self.copies) as u64);
        v.n = self.start + i as i64 * self.step;
        v
    }

    /// Values for record `index` (its first copy).
    pub fn record_vars(&self, index: u64) -> Vars {
        let row = (!self.rows.is_empty()).then(|| self.rows[index as usize % self.rows.len()].clone());
        let n = self.start + (index * self.copies as u64) as i64 * self.step;
        let mut v = Vars { n, index, seq: String::new(), row };
        if let Some(q) = &self.seq {
            v.seq = q.format(index, &v);
        }
        v
    }
}

#[derive(Clone)]
pub struct PrintJob {
    pub label: Label,
    pub settings: PrintSettings,
    pub caps: Caps,
    /// Snapshot of the selected spreadsheet rows (one label each).
    pub rows: Vec<Arc<Record>>,
}

/// Stream a job page by page. `stop` makes the next page the last one (fed + cut).
pub fn run_job(
    t: &mut dyn Transport,
    renderer: &Mutex<Renderer>,
    job: &PrintJob,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(u32, Option<u32>),
) -> Result<u32> {
    let st = t.begin()?;
    let errs = st.errors();
    if !errs.is_empty() {
        t.abort();
        bail!("{}", errs.join(", "));
    }
    let Some(tape) = st.tape() else {
        t.abort();
        bail!("no tape loaded");
    };
    let caps = job.caps;
    let pager = Pager::new(&job.label, &job.settings, job.rows.clone());
    let opts: &PageOpts = &job.settings.opts;
    let static_page = !job.label.uses_vars();
    let mut cached: Option<Bitmap> = None;

    let mut run = || -> Result<u32> {
        let mut i = 0u32;
        let mut buf = Vec::new();
        loop {
            let last = stop.load(Ordering::Relaxed) || pager.total.is_some_and(|t| i + 1 >= t);
            let fresh;
            let bm: &Bitmap = if static_page && cached.is_some() {
                cached.as_ref().unwrap()
            } else {
                let r = renderer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .render(&job.label, &tape, &pager.vars(i), opts);
                if !r.warnings.is_empty() && i == 0 {
                    bail!("{}", r.warnings.join("; "));
                }
                if static_page {
                    cached = Some(r.bitmap);
                    cached.as_ref().unwrap()
                } else {
                    fresh = r.bitmap;
                    &fresh
                }
            };
            if bm.w as f32 / crate::proto::raster::feed_dpmm(opts.high_res) > 1000.0 {
                bail!("label longer than 1 m");
            }
            buf.clear();
            encode_page(&mut buf, bm, &tape, opts, caps, i, last);
            t.send_page(&buf, label_length_mm(bm, opts)).with_context(|| format!("label {}", i + 1))?;
            i += 1;
            progress(i, pager.total);
            if last {
                break;
            }
        }
        t.finish()?;
        Ok(i)
    };
    let res = run();
    if res.is_err() {
        t.abort();
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pager_sequence_rows_and_numbering() {
        use crate::label::{Block, SequenceBlock};
        let s = PrintSettings { copies: 2, num_start: 10, num_step: 5, ..Default::default() };
        let mut label = Label::default();
        label.blocks.push(Block::Sequence(SequenceBlock { prefix: "A-".into(), count: 3, digits: 2, ..Default::default() }));

        // Sequence drives the count: 3 values x 2 copies.
        let p = Pager::new(&label, &s, vec![]);
        assert_eq!(p.total, Some(6));
        assert_eq!(Pager::counts(&label, &s, 0), (Some(3), Some(6)));
        assert_eq!([p.vars(0).seq, p.vars(1).seq, p.vars(2).seq, p.vars(5).seq], ["A-01", "A-01", "A-02", "A-03"]);
        assert_eq!(p.vars(3).n, 25);

        // Selected rows drive the count; the sequence numbers them.
        let h = Arc::new(vec!["Part".to_string()]);
        let rows: Vec<_> = ["R1", "C7"].iter().map(|v| Arc::new(Record { headers: h.clone(), values: vec![v.to_string()] })).collect();
        let p = Pager::new(&label, &s, rows);
        assert_eq!(p.total, Some(4));
        let v = p.vars(2);
        assert_eq!((v.row.as_ref().unwrap().get("Part"), v.seq.as_str()), (Some("C7"), "A-02"));

        let inf = Pager::new(&label, &PrintSettings { infinite: true, ..Default::default() }, vec![]);
        assert_eq!(inf.total, None);
        assert_eq!(Pager::new(&Label::default(), &s, vec![]).total, Some(2));
    }
}
