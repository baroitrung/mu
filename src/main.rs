mod cli;
mod tui;
// Port keeps Go-faithful API surfaces that are not all consumed by the CLI;
// dead_code allowed on the leaf modules so clippy -D warnings stays clean.
#[allow(dead_code)]
mod analyze;
#[allow(dead_code)]
mod audit;
#[allow(dead_code)]
mod clean;
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod error;
#[allow(dead_code)]
mod history;
#[allow(dead_code)]
mod installer;
#[allow(dead_code)]
mod oplog;
#[allow(dead_code)]
mod optimize;
#[allow(dead_code)]
mod paths;
#[allow(dead_code)]
mod purge;
#[allow(dead_code)]
mod runner;
mod runtext;
#[allow(dead_code)]
mod size;
mod status;
#[allow(dead_code)]
mod trash;
#[allow(dead_code)]
mod uninstall;
#[allow(dead_code)]
mod whitelist;
mod xdg;

fn main() {
    cli::run();
}
