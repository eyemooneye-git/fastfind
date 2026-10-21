// Release builds are GUI only; debug builds keep a console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod elevate;
mod i18n;
mod index;
mod log;
mod mft;
mod png;
mod search;
mod selftest;
mod ui;
mod volume;
mod wildcard;
mod winutil;

use std::path::PathBuf;

fn main() {
    log::init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(selftest::run());
    }

    let mut shot: Option<PathBuf> = None;
    let mut win_shot: Option<PathBuf> = None;
    let mut copy_test: Option<String> = None;
    let mut shot_pattern = String::new();
    let mut shot_dir = String::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--screenshot" if i + 1 < args.len() => {
                shot = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--window-shot" if i + 1 < args.len() => {
                win_shot = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--copy-test" if i + 1 < args.len() => {
                copy_test = Some(args[i + 1].clone());
                i += 2;
            }
            "--pattern" if i + 1 < args.len() => {
                shot_pattern = args[i + 1].clone();
                i += 2;
            }
            "--dir" if i + 1 < args.len() => {
                shot_dir = args[i + 1].clone();
                i += 2;
            }
            _ => i += 1,
        }
    }

    if shot.is_some() && shot_pattern.is_empty() {
        // Screenshot mode shows a populated window by default.
        shot_pattern = "*.dll".to_owned();
    }

    if shot.is_some() || win_shot.is_some() || copy_test.is_some() {
        // Never hang a headless run waiting for a paint that may not come.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(60));
            std::process::exit(3);
        });
    }

    std::process::exit(ui::run(ui::Options {
        shot,
        win_shot,
        copy_test,
        shot_pattern,
        shot_dir,
    }));
}
