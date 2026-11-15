// В релизной сборке не показываем чёрное консольное окно.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod engine;
mod gui;
mod hotkey;
mod keys;
mod keysend;
mod lag;
mod midi;
mod notes;
mod piano;
mod render;
mod resample;
mod state;
mod theme;
mod virtual_mic;

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::lag::LagMode;

const HELP: &str = "\
Microphone Editor — микрофон с эффектом лага для Discord.

Без параметров запускается окно программы.

Параметры для проверки:
  --list-devices                 показать звуковые устройства
  --loopback-test                проверить виртуальные микрофоны: проходит ли
                                 звук и на сколько дБ кабель его усиливает
  --diagnose [СЕКУНДЫ] [--config ПУТЬ]
                                 запустить звук с настройками из config.toml
                                 (выходы пишут тишину) и раз в секунду
                                 печатать статистику: размеры порций,
                                 паузы, опустошения буфера
  --render ВХОД.wav ВЫХОД.wav    прогнать WAV-файл через эффект лага
      --mode mix|robot|dropouts|spikes|stutter   (по умолчанию mix)
      --intensity 0..100                         (по умолчанию 60)
      --catchup speedup|skip                     (по умолчанию speedup)
      --catchup-speed 1.1..3.0                   (по умолчанию 1.6)
      --bitrate БИТ/С                            (по умолчанию 64000)
      --clean-start СЕКУНДЫ                      (по умолчанию 1.5)
  --piano-demo ВЫХОД.wav [--instrument N]
                                 сыграть короткую мелодию синтезатором
                                 (проверка SoundFont без MIDI-клавиатуры)
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
    midi::spawn(shared.clone());

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Microphone Editor")
            .with_inner_size([700.0, 760.0])
            .with_min_inner_size([560.0, 560.0]),
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
            for n in &inputs {
                println!("  {n}");
            }
            println!("Выходы:");
            for n in &outputs {
                println!("  {n}");
            }
            println!("Виртуальные микрофоны:");
            for m in virtual_mic::pair_virtual_mics(&inputs, &outputs) {
                println!("  {} -> {}", m.render, m.capture);
            }
            Ok(())
        }
        "--loopback-test" => {
            let (inputs, outputs) = engine::list_devices();
            let mics = virtual_mic::pair_virtual_mics(&inputs, &outputs);
            if mics.is_empty() {
                bail!("виртуальных микрофонов не найдено");
            }
            for m in mics {
                let r = virtual_mic::loopback_test(&m.render, &m.capture)?;
                let verdict = if r.works() { "звук проходит" } else { "звук НЕ проходит" };
                println!("{} -> {}: пик {:.4}, усиление {:+.1} дБ, {verdict}", m.render, m.capture, r.peak, r.gain_db);
            }
            Ok(())
        }
        "--piano-demo" => {
            let out = args.get(1).ok_or_else(|| anyhow!("нужно: --piano-demo ВЫХОД.wav"))?;
            let mut instrument = 0u8;
            if args.get(2).map(String::as_str) == Some("--instrument") {
                instrument = args.get(3).ok_or_else(|| anyhow!("нет номера инструмента"))?.parse()?;
            }
            render::piano_demo(out, instrument)?;
            println!("Готово: {out}");
            Ok(())
        }
        "--diagnose" => {
            let mut seconds = 10u64;
            let mut cfg_path = config::config_path();
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                if a == "--config" {
                    cfg_path = it.next().ok_or_else(|| anyhow!("нет пути после --config"))?.into();
                } else {
                    seconds = a.parse()?;
                }
            }
            run_diagnose(&cfg_path, seconds)
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

fn run_diagnose(cfg_path: &std::path::Path, seconds: u64) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let (cfg, warning) = config::load(cfg_path);
    if let Some(w) = warning {
        println!("{w}");
    }
    println!("Настройки: {}", cfg_path.display());
    let shared = Arc::new(state::Shared::new());
    gui::apply_to_shared(&cfg, &shared);
    shared.mute_outputs.store(true, Ordering::Relaxed);
    let engine = engine::spawn(shared.clone(), cfg.audio_settings());

    std::thread::sleep(Duration::from_millis(1500));
    let st = shared.status();
    if let Some(e) = &st.error {
        bail!("звук не запустился: {e}");
    }
    println!("Микрофон: {}", st.input_name);
    println!("Выход:    {}", st.output_name.as_deref().unwrap_or("-"));
    println!("Прослушка: {}", st.monitor_name.as_deref().unwrap_or("-"));
    for w in &st.warnings {
        println!("! {w}");
    }
    // Сбрасываем то, что накопилось за запуск.
    shared.input_stats.take();
    shared.main_stats.take();
    shared.monitor_stats.take();

    let ms = |us: u64| us as f64 / 1000.0;
    let frames = |s: &state::StatsSnapshot| {
        if s.callbacks == 0 { "-".to_string() } else { format!("{}..{}", s.min_frames, s.max_frames) }
    };
    for sec in 1..=seconds {
        // На 2-й секунде зажимаем аккорд, на 4-й отпускаем — проверка, что
        // синтезатор пианино успевает считать звук.
        let chord = [60u8, 64, 67, 72];
        if sec == 2 {
            println!("     (зажат аккорд C-E-G-C на пианино)");
            for n in chord {
                midi::handle_message(&shared, &[0x90, n, 100], false);
            }
        }
        if sec == 4 {
            for n in chord {
                midi::handle_message(&shared, &[0x80, n, 0], false);
            }
        }
        std::thread::sleep(Duration::from_secs(1));
        let i = shared.input_stats.take();
        println!(
            "[{sec:>2}] вход   {} Гц {} кан | вызовов {:>3} | порция {:>9} | пауза макс {:>6.1} мс | обработка макс {:>5.2} мс | сбоев Windows {}",
            i.sample_rate, i.channels, i.callbacks, frames(&i), ms(i.max_gap_us), ms(i.max_work_us), i.xruns
        );
        for (name, stats) in [("кабель", &shared.main_stats), ("наушн.", &shared.monitor_stats)] {
            let o = stats.take();
            if o.sample_rate == 0 {
                continue;
            }
            let min_fill = if o.min_fill == u64::MAX { 0 } else { o.min_fill };
            println!(
                "     {name} {} Гц {} кан | вызовов {:>3} | порция {:>9} | пауза макс {:>6.1} мс | мин. буфер {:>5} | обрывов {} | тишина {} | выкинуто {} | сбоев Windows {}",
                o.sample_rate, o.channels, o.callbacks, frames(&o), ms(o.max_gap_us), min_fill, o.underruns, o.silent_callbacks, o.dropped, o.xruns
            );
        }
    }
    let _ = engine.send(engine::Cmd::Shutdown);
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

/// В релизной сборке у программы нет своей консоли — подключаемся к консоли,
/// из которой её запустили, чтобы было видно вывод команд.
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
