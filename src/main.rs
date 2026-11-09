// Removed once decide.rs and policy.rs use the model.
#![allow(dead_code)]

mod boat;
mod github;
mod model;
mod run;

fn main() {
    eprintln!("usage: sw-boat-runner check|plan|run");
    std::process::exit(2);
}
