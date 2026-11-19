mod app;
mod cli;
mod data;
mod icons;
mod job;
mod label;
mod menu;
mod printer;
mod proto;
mod spell;
mod transport;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("-psn")).collect();
    if args.is_empty() {
        return app::run();
    }
    cli::run(args)
}
