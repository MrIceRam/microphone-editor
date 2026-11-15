// В релизной сборке не показываем чёрное консольное окно.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod engine;
mod gui;
mod hotkey;
mod keys;
mod lag;
mod render;
mod resample;
mod state;

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::lag::LagMode;

const HELP: &str = "\
Microphone Editor — микрофон с эффектом лага для Discord.

Без параметров запускается окно программы.

Параметры для проверки:
  --list-devices                 показать звуковые устройства
  --render ВХОД.wav ВЫХОД.wav    прогнать WAV-файл через эффект лага
      --mode mix|robot|dropouts|spikes|stutter   (по умолчанию mix)
      --intensity 0..100                         (по умолчанию 60)
      --catchup speedup|skip                     (по умолчанию speedup)
      --catchup-speed 1.1..3.0                   (по умолчанию 1.6)
      --bitrate БИТ/С                            (по умолчанию 64000)
      --clean-start СЕКУНДЫ                      (по умолчанию 1.5)
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        attach_console();
        if let Err(e) = run_cli(&args) {
            eprintln!("Ошибка: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = run_gui() {
        eprintln!("Ошибка: {e:#}");
        std::process::exit(1);
    }
}

fn run_gui() -> Result<()> {
    let cfg_path = config::config_path();
    let (cfg, warning) = config::load(&cfg_path);

    let shared = Arc::new(state::Shared::new());
    gui::apply_to_shared(&cfg, &shared);
    let engine = engine::spawn(shared.clone(), cfg.audio_settings());
    hotkey::spawn(shared.clone());

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Microphone Editor")
            .with_inner_size([560.0, 840.0])
            .with_min_inner_size([480.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Microphone Editor",
        options,
        Box::new(move |cc| Ok(Box::new(gui::App::new(cc, shared, engine, cfg, cfg_path, warning)))),
    )
    .map_err(|e| anyhow!("не удалось открыть окно: {e}"))
}

fn run_cli(args: &[String]) -> Result<()> {
    match args[0].as_str() {
        "--help" | "-h" | "/?" => {
            print!("{HELP}");
            Ok(())
        }
        "--list-devices" => {
            let (inputs, outputs) = engine::list_devices();
            println!("Микрофоны (входы):");
            for n in inputs {
                println!("  {n}");
            }
            println!("Выходы:");
            for n in outputs {
                println!("  {n}");
            }
            Ok(())
        }
        "--render" => {
            if args.len() < 3 {
                bail!("нужно: --render ВХОД.wav ВЫХОД.wav");
            }
            let mut o = render::RenderOptions {
                input: args[1].clone(),
                output: args[2].clone(),
                mode: LagMode::Mix,
                intensity: 0.6,
                catchup_skip: false,
                catchup_speed: 1.6,
                bitrate: 64_000,
                clean_start: 1.5,
            };
            let mut it = args[3..].iter();
            while let Some(flag) = it.next() {
                let val = it.next().ok_or_else(|| anyhow!("нет значения для {flag}"))?;
                match flag.as_str() {
                    "--mode" => {
                        o.mode = LagMode::ALL
                            .into_iter()
                            .find(|m| m.config_name() == val)
                            .ok_or_else(|| anyhow!("неизвестный режим {val}"))?
                    }
                    "--intensity" => o.intensity = val.parse::<f32>()? / 100.0,
                    "--catchup" => o.catchup_skip = val == "skip",
                    "--catchup-speed" => o.catchup_speed = val.parse()?,
                    "--bitrate" => o.bitrate = val.parse()?,
                    "--clean-start" => o.clean_start = val.parse()?,
                    _ => bail!("неизвестный параметр {flag}"),
                }
            }
            render::render(&o)?;
            println!("Готово: {}", o.output);
            Ok(())
        }
        other => bail!("неизвестный параметр {other}\n\n{HELP}"),
    }
}

/// В релизной сборке у программы нет своей консоли — подключаемся к консоли,
/// из которой её запустили, чтобы было видно вывод команд.
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
