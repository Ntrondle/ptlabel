//! Scriptable CLI: list / status / render / print.

use crate::job::{Pager, PrintJob, PrintSettings, run_job};
use crate::label::render::{Renderer, label_length_mm, save_png};
use crate::label::{Block, Label, Rotation, TextBlock};
use crate::proto::Tape;
use crate::transport::{PrinterId, discovery};
use anyhow::{Context, Result, bail};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const USAGE: &str = "\
ptlabel                         open the app
ptlabel list                    find printers (Bluetooth + Wi-Fi)
ptlabel status <target>         read status (target = IP/host, /dev/cu.PT-..., bt:PT-... or COM#)
ptlabel render [opts] [text]    render to PNG (--out FILE, --tape MM)
ptlabel print <target> [opts] [text]
options: --label FILE.ptlabel  --copies N  --no-cut  --half-cut  --chain  --hires
         --mirror  --margin MM  --cut-every N  --tape MM  --out FILE
         --rotate 90|180|270 (text only, not --label)
text: words are joined with spaces, \\n starts a new line, -- ends the options";

struct Args {
    rest: Vec<String>,
    label: Option<String>,
    out: String,
    tape: u8,
    rotation: Rotation,
    settings: PrintSettings,
}

fn parse(args: &[String]) -> Result<Args> {
    let mut a = Args {
        rest: vec![],
        label: None,
        out: "label.png".into(),
        tape: 12,
        rotation: Rotation::R0,
        settings: PrintSettings::default(),
    };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let mut val = || it.next().cloned().with_context(|| format!("{x} needs a value"));
        match x.as_str() {
            "--label" => a.label = Some(val()?),
            "--out" => a.out = val()?,
            "--tape" => a.tape = match val()?.as_str() {
                "3.5" => 4, // the 3.5 mm cassette's width code
                v => num(x, v)?,
            },
            "--rotate" => a.rotation = Rotation::from_degrees(num(x, &val()?)?).context("--rotate: 0, 90, 180 or 270")?,
            "--copies" => a.settings.copies = num(x, &val()?)?,
            "--margin" => a.settings.opts.margin_mm = num(x, &val()?)?,
            "--cut-every" => a.settings.opts.cut_every = num(x, &val()?)?,
            "--no-cut" => a.settings.opts.auto_cut = false,
            "--half-cut" => a.settings.opts.half_cut = true,
            "--chain" => a.settings.opts.chain = true,
            "--hires" => a.settings.opts.high_res = true,
            "--mirror" => a.settings.opts.mirror = true,
            "--" => {
                a.rest.extend(it.cloned());
                break;
            }
            // A mistyped option must not end up printed as label text.
            o if o.starts_with("--") && o[2..].starts_with(|c: char| c.is_ascii_alphabetic()) => {
                bail!("unknown option {o}\n\n{USAGE}")
            }
            _ => a.rest.push(x.clone()),
        }
    }
    Ok(a)
}

fn num<T: std::str::FromStr>(flag: &str, v: &str) -> Result<T> {
    v.trim().parse().ok().with_context(|| format!("{flag} needs a number, got {v:?}"))
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
        blocks: vec![Block::Text(TextBlock { text, rotation: a.rotation, ..Default::default() })],
        ..Default::default()
    })
}

pub fn run(args: Vec<String>) -> Result<()> {
    if args.iter().take_while(|a| *a != "--").any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let a = parse(&args[1..])?;
    match args[0].as_str() {
        "list" => {
            let found = Arc::new(Mutex::new(vec![]));
            let f2 = found.clone();
            discovery::spawn(move |v| *f2.lock().unwrap() = v);
            std::thread::sleep(Duration::from_secs(4));
            let v = found.lock().unwrap();
            if v.is_empty() {
                #[cfg(target_os = "windows")]
                println!("no printers found (pair the PT-P710BT in Settings > Bluetooth & devices)");
                #[cfg(not(target_os = "windows"))]
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
            let tape = Tape::tze(a.tape).with_context(|| format!("unknown tape width {} (3.5, 6, 9, 12, 18 or 24)", a.tape))?;
            let t0 = std::time::Instant::now();
            let mut r = Renderer::new();
            let t1 = t0.elapsed();
            let vars = Pager::new(&label, &a.settings, vec![]).vars(0);
            let out = r.render(&label, &tape, &vars, &a.settings.opts);
            let t2 = t0.elapsed() - t1;
            for w in &out.warnings {
                eprintln!("warning: {w}");
            }
            if out.overflow {
                eprintln!("warning: content is longer than the fixed length");
            }
            save_png(&out.bitmap, &tape, &a.settings.opts, std::path::Path::new(&a.out))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn parses_options_and_text() {
        let a = parse(&args("192.168.1.160 --copies 3 --half-cut --tape 3.5 --margin 5 Hello\\nworld")).unwrap();
        assert_eq!((a.settings.copies, a.settings.opts.half_cut, a.tape, a.settings.opts.margin_mm), (3, true, 4, 5.0));
        assert_eq!(a.rest, ["192.168.1.160", "Hello\\nworld"]);
        assert_eq!(label_from(&a, &a.rest[1..]).unwrap().blocks[0], Block::Text(TextBlock { text: "Hello\nworld".into(), ..Default::default() }));
        assert_eq!(parse(&args("-5V --> -- --copies")).unwrap().rest, ["-5V", "-->", "--copies"]);
    }

    #[test]
    fn rejects_bad_options() {
        let err = |s: &str| parse(&args(s)).err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(err("x --copis 3").starts_with("unknown option --copis"), "typo was printed as text");
        assert_eq!(err("x --copies abc"), "--copies needs a number, got \"abc\"");
        assert_eq!(err("x --tape"), "--tape needs a value");
        assert!(run(args("render --help")).is_ok(), "help, not a label saying --help");
    }
}
