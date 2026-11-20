//! Чтение чата Twitch без входа в аккаунт.
//!
//! Когда зритель берёт награду за баллы канала с вводом текста, его сообщение
//! приходит в обычный чат с пометкой `custom-reward-id`. Чат любого канала
//! можно читать анонимно (ник вида justinfan12345), поэтому программе не нужны
//! ни логин, ни доступы к аккаунту стримера. Награду стример создаёт сам,
//! а программа запоминает её ID: «Привязать награду» → взять награду один раз.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crate::state::LockExt;
use crate::state::Shared;
use crate::tts;

const SERVER: &str = "irc.chat.twitch.tv:6667";
/// Сколько последних сообщений из наград показывать в окне.
const RECENT: usize = 6;

/// Сообщение чата, нужное программе.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub user: String,
    pub text: String,
    /// ID награды, если сообщение пришло из награды за баллы.
    pub reward_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum ChatState {
    #[default]
    Off,
    Connecting,
    Connected,
    Error(String),
}

/// Последнее сообщение из награды (для окна).
#[derive(Debug, Clone)]
pub struct RecentReward {
    pub user: String,
    pub text: String,
    /// Озвучено (награда наша) или пропущено (другая награда).
    pub spoken: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TwitchStatus {
    pub state: ChatState,
    pub channel: String,
    pub recent: VecDeque<RecentReward>,
}

/// "https://www.twitch.tv/Ник" / "#ник" / "Ник" -> "ник".
pub fn normalize_channel(input: &str) -> String {
    let s = input.trim().trim_end_matches('/');
    let s = s.rsplit('/').next().unwrap_or(s);
    s.trim_start_matches('#').trim().to_lowercase()
}

fn unescape_tag(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('s') => out.push(' '),
                Some(':') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some('r') => out.push('\r'),
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Разбирает строку IRC. Возвращает только сообщения чата (PRIVMSG).
pub fn parse_line(line: &str) -> Option<ChatMessage> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (tags, rest) = match line.strip_prefix('@') {
        Some(r) => {
            let (t, rest) = r.split_once(' ')?;
            (Some(t), rest)
        }
        None => (None, line),
    };
    // ":ник!ник@ник.tmi.twitch.tv PRIVMSG #канал :текст"
    let rest = rest.strip_prefix(':')?;
    let (prefix, rest) = rest.split_once(' ')?;
    let (command, rest) = rest.split_once(' ')?;
    if command != "PRIVMSG" {
        return None;
    }
    let (_channel, text) = rest.split_once(" :")?;
    let login = prefix.split('!').next().unwrap_or(prefix).to_string();

    let mut user = login;
    let mut reward_id = None;
    if let Some(tags) = tags {
        for kv in tags.split(';') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k {
                "display-name" if !v.is_empty() => user = unescape_tag(v),
                "custom-reward-id" if !v.is_empty() => reward_id = Some(v.to_string()),
                _ => {}
            }
        }
    }
    Some(ChatMessage {
        user,
        text: text.to_string(),
        reward_id,
    })
}

pub fn spawn(shared: Arc<Shared>) {
    thread::Builder::new()
        .name("twitch".into())
        .spawn(move || run(shared))
        .expect("не удалось запустить поток Twitch");
}

fn set_state(shared: &Shared, channel: &str, state: ChatState) {
    let mut st = shared.twitch_status.lock_ok();
    st.state = state;
    st.channel = channel.to_string();
}

fn run(shared: Arc<Shared>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        let channel = normalize_channel(&shared.twitch_channel.lock_ok());
        if !shared.twitch_enabled.load(Ordering::Relaxed) || channel.is_empty() {
            set_state(&shared, &channel, ChatState::Off);
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        set_state(&shared, &channel, ChatState::Connecting);
        match session(&shared, &channel) {
            Ok(()) => backoff = Duration::from_secs(2), // вышли сами: сменили канал / выключили
            Err(e) => {
                set_state(&shared, &channel, ChatState::Error(e));
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// Одно подключение к чату. Ok — вышли сами (настройки поменялись), Err — обрыв.
fn session(shared: &Shared, channel: &str) -> Result<(), String> {
    let stream = TcpStream::connect(SERVER).map_err(|e| format!("нет связи с Twitch: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| e.to_string())?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let nick = format!("justinfan{}", 10_000 + crate::state::now_ms() % 80_000);
    write!(
        writer,
        "CAP REQ :twitch.tv/tags twitch.tv/commands\r\nNICK {nick}\r\nJOIN #{channel}\r\n"
    )
    .map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut last_data = Instant::now();
    loop {
        // Настройки поменялись — переподключаемся к другому каналу / выключаемся.
        let wanted = normalize_channel(&shared.twitch_channel.lock_ok());
        if !shared.twitch_enabled.load(Ordering::Relaxed) || wanted != channel {
            return Ok(());
        }
        if last_data.elapsed() > Duration::from_secs(360) {
            return Err("Twitch долго молчит, переподключаюсь".into());
        }

        match reader.read_line(&mut line) {
            Ok(0) => return Err("Twitch закрыл соединение".into()),
            Ok(_) => {
                last_data = Instant::now();
                let l = std::mem::take(&mut line);
                if l.starts_with("PING") {
                    let _ = writer.write_all(l.replacen("PING", "PONG", 1).as_bytes());
                } else if l.contains(" RECONNECT") {
                    return Err("Twitch попросил переподключиться".into());
                } else if l.contains(&format!(" JOIN #{channel}")) || l.contains(" ROOMSTATE ") {
                    set_state(shared, channel, ChatState::Connected);
                } else if let Some(msg) = parse_line(&l) {
                    handle(shared, msg);
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(format!("обрыв связи с Twitch: {e}")),
        }
    }
}

fn handle(shared: &Shared, msg: ChatMessage) {
    let Some(reward) = msg.reward_id.clone() else { return };

    // Режим «Привязать награду»: запоминаем первую пришедшую награду.
    if shared.twitch_learning.swap(false, Ordering::Relaxed) {
        *shared.twitch_learned.lock_ok() = Some(reward.clone());
        *shared.twitch_reward.lock_ok() = reward.clone();
    }

    let ours = *shared.twitch_reward.lock_ok() == reward;
    let spoken = ours && {
        let max = shared.tts_max_len.load(Ordering::Relaxed) as usize;
        let text = tts::prepare_text(&msg.text, max);
        let speech = if shared.tts_read_name.load(Ordering::Relaxed) {
            format!("{} пишет: {text}", msg.user)
        } else {
            text.clone()
        };
        !text.is_empty()
            && tts::enqueue(
                shared,
                tts::Job {
                    text: speech,
                    label: format!("{}: {text}", msg.user),
                },
            )
    };

    let mut st = shared.twitch_status.lock_ok();
    st.recent.push_front(RecentReward {
        user: msg.user,
        text: msg.text,
        spoken,
    });
    st.recent.truncate(RECENT);
}

/// Проверка из командной строки: слушает чаты каналов и печатает,
/// сколько пришло сообщений и какие были награды.
pub fn watch(channels: &[String], seconds: u64) -> Result<(), String> {
    let stream = TcpStream::connect(SERVER).map_err(|e| format!("нет связи с Twitch: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_millis(500))).map_err(|e| e.to_string())?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let list: Vec<String> = channels.iter().map(|c| format!("#{}", normalize_channel(c))).collect();
    write!(writer, "CAP REQ :twitch.tv/tags twitch.tv/commands\r\nNICK justinfan{}\r\nJOIN {}\r\n", 10_000 + crate::state::now_ms() % 80_000, list.join(","))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let started = Instant::now();
    let (mut messages, mut rewards, mut joined, mut tagged, mut highlighted) = (0, 0, 0, 0, 0);
    while started.elapsed() < Duration::from_secs(seconds) {
        match reader.read_line(&mut line) {
            Ok(0) => return Err("Twitch закрыл соединение".into()),
            Ok(_) => {
                let l = std::mem::take(&mut line);
                if l.contains(" CAP * ") {
                    println!("{}", l.trim_end());
                }
                if l.starts_with("PING") {
                    let _ = writer.write_all(l.replacen("PING", "PONG", 1).as_bytes());
                } else if l.contains(" JOIN #") {
                    joined += 1;
                } else if let Some(m) = parse_line(&l) {
                    messages += 1;
                    if l.starts_with('@') {
                        tagged += 1;
                    }
                    if l.contains("msg-id=highlighted-message") {
                        highlighted += 1;
                    }
                    if let Some(id) = &m.reward_id {
                        rewards += 1;
                        println!("награда {id}: {}: {}", m.user, m.text.chars().take(60).collect::<String>());
                    }
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    println!(
        "каналов подключено: {joined}, сообщений: {messages} (с тегами: {tagged}), \
         из своих наград: {rewards}, «выделить сообщение»: {highlighted}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reward_message() {
        let line = "@badge-info=;badges=;color=#FF0000;custom-reward-id=1b2c3d4e-0000-1111-2222-333344445555;\
                    display-name=Вася\\sТест;emotes=;id=abc;room-id=1;user-id=2 \
                    :vasya!vasya@vasya.tmi.twitch.tv PRIVMSG #streamer :привет всем :)\r\n";
        let m = parse_line(line).unwrap();
        assert_eq!(m.user, "Вася Тест");
        assert_eq!(m.text, "привет всем :)");
        assert_eq!(m.reward_id.as_deref(), Some("1b2c3d4e-0000-1111-2222-333344445555"));
    }

    #[test]
    fn plain_chat_has_no_reward() {
        let m = parse_line("@display-name=Petya;id=1 :petya!petya@petya.tmi.twitch.tv PRIVMSG #s :hi").unwrap();
        assert_eq!(m.reward_id, None);
        assert_eq!(m.user, "Petya");
        assert!(parse_line(":tmi.twitch.tv 001 justinfan123 :Welcome, GLHF!").is_none());
        assert!(parse_line("PING :tmi.twitch.tv").is_none());
    }

    #[test]
    fn channel_names_are_normalized() {
        assert_eq!(normalize_channel("https://www.twitch.tv/MrIce/"), "mrice");
        assert_eq!(normalize_channel("#Stream_er"), "stream_er");
        assert_eq!(normalize_channel("  Name "), "name");
    }
}
