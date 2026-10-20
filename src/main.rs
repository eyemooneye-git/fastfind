// Release builds are GUI only; debug builds keep a console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod elevate;
mod i18n;
mod icon;
mod index;
mod log;
mod mft;
mod png;
mod search;
mod selftest;
mod volume;
mod wildcard;
mod winutil;

use std::path::PathBuf;

fn main() -> eframe::Result {
    log::init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(selftest::run());
    }
    let mut shot: Option<PathBuf> = None;
    let mut win_shot: Option<PathBuf> = None;
    let mut copy_test: Option<String> = None;
    let mut shot_pattern = "*.dll".to_owned();
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
    if shot.is_some() || win_shot.is_some() || copy_test.is_some() {
        // Never hang a headless run waiting for a paint that may not come.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(60));
            std::process::exit(3);
        });
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([760.0, 420.0])
            .with_title(i18n::app_title())
            .with_icon(std::sync::Arc::new(icon::app_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "FastFind",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::FastFindApp::new(
                cc,
                shot.clone(),
                win_shot.clone(),
                copy_test.clone(),
                shot_pattern.clone(),
                shot_dir.clone(),
            )))
        }),
    )
}
