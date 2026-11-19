//! Scriptable CLI: list / status / render / print.

use crate::job::{PrintJob, PrintSettings, run_job};
use crate::label::render::{Renderer, label_length_mm, preview_rgba};
use crate::label::{Block, Label, TextBlock, Vars};
use crate::proto::Tape;
use crate::transport::{PrinterId, discovery};
use anyhow::{Context, Result, bail};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const USAGE: &str = "\
ptlabel                         open the app
ptlabel list                    find printers (Bluetooth + Wi-Fi)
ptlabel status <target>         read status (target = IP/host or /dev/cu.PT-...)
ptlabel render [opts] [text]    render to PNG (--out FILE, --tape MM)
ptlabel print <target> [opts] [text]
options: --label FILE.json  --copies N  --no-cut  --half-cut  --chain  --hires
         --mirror  --margin MM  --cut-every N  --tape MM  --out FILE
text: words are joined with spaces, \\n starts a new line";

struct Args {
    rest: Vec<String>,
    label: Option<String>,
    out: String,
    tape: u8,
    settings: PrintSettings,
}

fn parse(args: &[String]) -> Result<Args> {
    let mut a = Args {
        rest: vec![],
        label: None,
        out: "label.png".into(),
        tape: 12,
        settings: PrintSettings::default(),
    };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let mut val = || it.next().cloned().with_context(|| format!("{x} needs a value"));
        match x.as_str() {
            "--label" => a.label = Some(val()?),
            "--out" => a.out = val()?,
            "--tape" => a.tape = val()?.parse()?,
            "--copies" => a.settings.copies = val()?.parse()?,
            "--margin" => a.settings.opts.margin_mm = val()?.parse()?,
            "--cut-every" => a.settings.opts.cut_every = val()?.parse()?,
            "--no-cut" => a.settings.opts.auto_cut = false,
            "--half-cut" => a.settings.opts.half_cut = true,
            "--chain" => a.settings.opts.chain = true,
            "--hires" => a.settings.opts.high_res = true,
            "--mirror" => a.settings.opts.mirror = true,
            _ => a.rest.push(x.clone()),
        }
    }
    Ok(a)
}

fn label_from(a: &Args, words: &[String]) -> Result<Label> {
    if let Some(p) = &a.label {
        return Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?);
    }
    let text = words.join(" ").replace("\\n", "\n");
    if text.is_empty() {
        bail!("no text or --label given");
    }
    Ok(Label {
        blocks: vec![Block::Text(TextBlock { text, ..Default::default() })],
        ..Default::default()
    })
}

pub fn run(args: Vec<String>) -> Result<()> {
    let a = parse(&args[1..])?;
    match args[0].as_str() {
        "list" => {
            let found = Arc::new(Mutex::new(vec![]));
            let f2 = found.clone();
            discovery::spawn(move |v| *f2.lock().unwrap() = v);
            std::thread::sleep(Duration::from_secs(4));
            let v = found.lock().unwrap();
            if v.is_empty() {
                println!("no printers found (pair the PT-P710BT in System Settings > Bluetooth)");
            }
            for p in v.iter() {
                let target = match &p.link {
                    crate::transport::Link::Bluetooth(s) | crate::transport::Link::Network(s) => s,
                };
                println!("{:<40} {}", p.display(), target);
            }
        }
        "status" => {
            let target = a.rest.first().context("status <target>")?;
            let id = PrinterId::from_target(target);
            let st = id.open().status()?;
            println!("raw     {}", st.raw.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "));
            println!("model   {} (0x{:02X})", st.model_name().unwrap_or("?"), st.model_code());
            println!("state   {}", st.summary());
            match st.tape() {
                Some(t) => println!("tape    {} ({} printable pins)", t.describe(), t.print_pins),
                None => println!("tape    none"),
            }
        }
        "render" => {
            let label = label_from(&a, &a.rest)?;
            let tape = Tape::tze(a.tape).with_context(|| format!("unknown tape width {}", a.tape))?;
            let t0 = std::time::Instant::now();
            let mut r = Renderer::new();
            let t1 = t0.elapsed();
            let out = r.render(&label, &tape, &Vars { n: a.settings.num_start, ..Default::default() }, &a.settings.opts);
            let t2 = t0.elapsed() - t1;
            for w in &out.warnings {
                eprintln!("warning: {w}");
            }
            if out.overflow {
                eprintln!("warning: content is longer than the fixed length");
            }
            let (w, h, rgba) = preview_rgba(&out.bitmap, &tape, &a.settings.opts);
            image::RgbaImage::from_raw(w as u32, h as u32, rgba)
                .context("image")?
                .save(&a.out)?;
            println!(
                "{} -> {}x{} dots, {:.1} mm long (fonts {:?}, render {:?})",
                a.out,
                out.bitmap.w,
                out.bitmap.h,
                label_length_mm(&out.bitmap, &a.settings.opts),
                t1,
                t2
            );
        }
        "print" => {
            let target = a.rest.first().context("print <target> ...")?;
            let label = label_from(&a, &a.rest[1..])?;
            let id = PrinterId::from_target(target);
            let mut t = id.open();
            let renderer = Mutex::new(Renderer::new());
            let job = PrintJob { label, settings: a.settings.clone(), caps: id.caps(), rows: vec![] };
            let stop = AtomicBool::new(false);
            let n = run_job(t.as_mut(), &renderer, &job, &stop, &mut |d, n| {
                eprintln!("printed {d}/{}", n.map_or("∞".into(), |n| n.to_string()))
            })?;
            println!("done: {n} label(s)");
        }
        "-h" | "--help" | "help" => println!("{USAGE}"),
        other => bail!("unknown command {other}\n\n{USAGE}"),
    }
    Ok(())
}
