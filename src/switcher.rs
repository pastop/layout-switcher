use evdev::{Device, EventType, KeyCode};
use log::info;
use serde::Deserialize;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use uinput::event::keyboard::{Key, KeyPad, Keyboard as UiKey, Misc};

fn default_retry_delay() -> u64 {
    1000
}
fn default_buffer_size() -> usize {
    100
}

#[derive(Deserialize, Clone)]
pub struct Config {
    device_path: Option<String>,
    trigger_key: String,
    clipboard_timeout_ms: u64,
    layout_switch_combo: Vec<String>,
    ignored_keywords: Vec<String>,
    #[serde(default = "default_buffer_size")]
    buffer_size: usize,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_ms: u64,
}

pub struct Switcher {
    pub config: Config,
    buffer: Vec<(KeyCode, bool)>,
    shift_pressed: bool,
    ctrl_pressed: bool,
    last_event_time: Option<SystemTime>,
    all_selected: bool,
    trigger_keycode: KeyCode,
    switch_keys: Vec<UiKey>,
}

impl Switcher {
    pub fn new(config: Config) -> Result<Self, Box<dyn Error>> {
        let trigger_keycode = parse_evdev_key(&config.trigger_key).unwrap_or(KeyCode::KEY_PAUSE);
        let mut switch_keys = Vec::new();
        for key_str in &config.layout_switch_combo {
            if let Some(key) = parse_ui_key(key_str) {
                switch_keys.push(key);
            }
        }
        Ok(Switcher {
            config,
            buffer: Vec::new(),
            shift_pressed: false,
            ctrl_pressed: false,
            last_event_time: None,
            all_selected: false,
            trigger_keycode,
            switch_keys,
        })
    }

    pub fn debug_buffer_slice(&self, slice: &[(KeyCode, bool)]) -> String {
        slice
            .iter()
            .map(|(k, _)| format!("{:?}", k).replace("KEY_", "").to_lowercase())
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn fix_text(&mut self, dev: &mut uinput::Device) -> Result<(), Box<dyn Error>> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        let start_idx = if self.all_selected {
            0
        } else {
            let last_non_space = self
                .buffer
                .iter()
                .rposition(|(k, _)| *k != KeyCode::KEY_SPACE);

            match last_non_space {
                Some(end_of_word_idx) => { 
                    self.buffer[..end_of_word_idx]
                        .iter()
                        .rposition(|(k, _)| *k == KeyCode::KEY_SPACE)
                        .map(|pos| pos + 1) // Начало слова сразу после пробела
                        .unwrap_or(0)
                }
                None => 0, 
            }
        };

        let target_slice = &self.buffer[start_idx..];
        if target_slice.is_empty() {
            return Ok(());
        }

        info!("Исправляю: [{}]", self.debug_buffer_slice(target_slice));

        let bs = UiKey::Key(Key::BackSpace);
        for _ in 0..target_slice.len() {
            let _ = dev.click(&bs);
        }
        let _ = dev.synchronize();

        for k in &self.switch_keys {
            let _ = dev.press(k);
        }
        let _ = dev.synchronize();
        thread::sleep(Duration::from_millis(100));
        for k in &self.switch_keys {
            let _ = dev.release(k);
        }
        let _ = dev.synchronize();

        thread::sleep(Duration::from_millis(250));

        let l_shift = UiKey::Key(Key::LeftShift);
        for (key, shifted) in target_slice.iter() {
            if let Some(ui_key) = evdev_to_ui_key(*key) {
                if *shifted {
                    let _ = dev.press(&l_shift);
                    let _ = dev.click(&ui_key);
                    let _ = dev.release(&l_shift);
                } else {
                    let _ = dev.click(&ui_key);
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = dev.synchronize();

        if self.all_selected {
            self.buffer.clear();
            self.all_selected = false;
        } else {
            self.buffer.truncate(start_idx);
        }
        Ok(())
    }
}

pub fn load_config() -> Config {
    let path = "config.toml";
    let content = fs::read_to_string(path).expect("config.toml not found!");
    toml::from_str(&content).expect("config.toml parse error")
}

pub fn find_keyboard(config: &Config) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(ref path) = config.device_path {
        if !path.is_empty() {
            let p = PathBuf::from(path);
            if p.exists() {
                return Ok(p);
            }
        }
    }
    let mut candidates = Vec::new();
    for entry in glob::glob("/dev/input/event*")? {
        let path = entry?;
        if let Ok(dev) = Device::open(&path) {
            let name = dev.name().unwrap_or("").to_lowercase();
            if name.contains("rust switcher virtual")
                || config
                    .ignored_keywords
                    .iter()
                    .any(|k| name.contains(&k.to_lowercase()))
            {
                continue;
            }
            if dev.supported_keys().map_or(false, |k| {
                k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_SPACE)
            }) {
                let mut score = 0;
                if name.contains("keyboard") {
                    score += 100;
                }
                candidates.push((score, path));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .first()
        .map(|c| c.1.clone())
        .ok_or_else(|| "No keyboard found".into())
}

pub fn run_main_loop(device_path: &Path, state: &mut Switcher) -> Result<(), Box<dyn Error>> {
    let mut device = Device::open(device_path)?;
    device.grab()?;

    let mut v_dev = uinput::default()?
        .name("Rust Switcher Virtual Device")?
        .event(uinput::event::Keyboard::All)?
        .create()?;

    loop {
        for event in device.fetch_events()? {
            
            let now = event.timestamp();
            let duration = state.last_event_time
                .and_then(|t| now.duration_since(t).ok()) // Получаем Option<Duration>
                .unwrap_or_default();
            
            if duration.as_millis() > (state.config.clipboard_timeout_ms as u128) {
                state.buffer.clear();
            }
            state.last_event_time = Some(now);
                    
            if event.event_type() == EventType::KEY {
                let key_code = KeyCode(event.code());
                let value = event.value();

                match key_code {
                    KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT => {
                        state.shift_pressed = value != 0;
                    }
                    KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL => {
                        state.ctrl_pressed = value != 0;
                    }
                    _ => {}
                }

                if value == 1 {
                    // Нажатие
                    if key_code == state.trigger_keycode {
                        let _ = state.fix_text(&mut v_dev);
                        continue;
                    }

                    if state.ctrl_pressed && key_code == KeyCode::KEY_A {
                        state.all_selected = true;
                    } else if is_printable(key_code) {
                        if !state.ctrl_pressed {
                            state.all_selected = false;
                        }
                        if state.buffer.len() >= state.config.buffer_size {
                            state.buffer.remove(0);
                        }
                        state.buffer.push((key_code, state.shift_pressed));
                    } else if is_reset_key(key_code) {
                        state.buffer.clear();
                        state.all_selected = false;
                    }
                }

                if let Some(ui_key) = evdev_to_ui_key(key_code) {
                    match value {
                        1 | 2 => {
                            let _ = v_dev.press(&ui_key);
                        }
                        0 => {
                            let _ = v_dev.release(&ui_key);
                        }
                        _ => {}
                    }
                    let _ = v_dev.synchronize();
                }
            }
        }
    }
}

pub fn is_printable(key: KeyCode) -> bool {
    let c = key.code();
    (c >= KeyCode::KEY_1.code() && c <= KeyCode::KEY_SLASH.code())
        || (c >= KeyCode::KEY_A.code() && c <= KeyCode::KEY_Z.code())
        || key == KeyCode::KEY_SPACE
}

pub fn is_reset_key(key: KeyCode) -> bool {
    key == KeyCode::KEY_ENTER
        || key == KeyCode::KEY_KPENTER
        || key == KeyCode::KEY_ESC
        || key == KeyCode::KEY_TAB
}

pub fn parse_evdev_key(s: &str) -> Option<KeyCode> {
    match s {
        "KEY_PAUSE" => Some(KeyCode::KEY_PAUSE),
        "KEY_F12" => Some(KeyCode::KEY_F12),
        "KEY_CAPSLOCK" => Some(KeyCode::KEY_CAPSLOCK),
        _ => Some(KeyCode::KEY_PAUSE),
    }
}

fn parse_ui_key(s: &str) -> Option<UiKey> {
    match s {
        "KEY_LEFTMETA" => Some(UiKey::Key(Key::LeftMeta)),
        "KEY_SPACE" => Some(UiKey::Key(Key::Space)),
        "KEY_LEFTSHIFT" => Some(UiKey::Key(Key::LeftShift)),
        "KEY_LEFTALT" => Some(UiKey::Key(Key::LeftAlt)),
        "KEY_LEFTCTRL" => Some(UiKey::Key(Key::LeftControl)),
        _ => None,
    }
}

// маппер из evdev в uinput
pub fn evdev_to_ui_key(key: KeyCode) -> Option<UiKey> {
    match key {
        KeyCode::KEY_A => Some(UiKey::Key(Key::A)),
        KeyCode::KEY_B => Some(UiKey::Key(Key::B)),
        KeyCode::KEY_C => Some(UiKey::Key(Key::C)),
        KeyCode::KEY_D => Some(UiKey::Key(Key::D)),
        KeyCode::KEY_E => Some(UiKey::Key(Key::E)),
        KeyCode::KEY_F => Some(UiKey::Key(Key::F)),
        KeyCode::KEY_G => Some(UiKey::Key(Key::G)),
        KeyCode::KEY_H => Some(UiKey::Key(Key::H)),
        KeyCode::KEY_I => Some(UiKey::Key(Key::I)),
        KeyCode::KEY_J => Some(UiKey::Key(Key::J)),
        KeyCode::KEY_K => Some(UiKey::Key(Key::K)),
        KeyCode::KEY_L => Some(UiKey::Key(Key::L)),
        KeyCode::KEY_M => Some(UiKey::Key(Key::M)),
        KeyCode::KEY_N => Some(UiKey::Key(Key::N)),
        KeyCode::KEY_O => Some(UiKey::Key(Key::O)),
        KeyCode::KEY_P => Some(UiKey::Key(Key::P)),
        KeyCode::KEY_Q => Some(UiKey::Key(Key::Q)),
        KeyCode::KEY_R => Some(UiKey::Key(Key::R)),
        KeyCode::KEY_S => Some(UiKey::Key(Key::S)),
        KeyCode::KEY_T => Some(UiKey::Key(Key::T)),
        KeyCode::KEY_U => Some(UiKey::Key(Key::U)),
        KeyCode::KEY_V => Some(UiKey::Key(Key::V)),
        KeyCode::KEY_W => Some(UiKey::Key(Key::W)),
        KeyCode::KEY_X => Some(UiKey::Key(Key::X)),
        KeyCode::KEY_Y => Some(UiKey::Key(Key::Y)),
        KeyCode::KEY_Z => Some(UiKey::Key(Key::Z)),
        KeyCode::KEY_0 => Some(UiKey::Key(Key::_0)),
        KeyCode::KEY_1 => Some(UiKey::Key(Key::_1)),
        KeyCode::KEY_2 => Some(UiKey::Key(Key::_2)),
        KeyCode::KEY_3 => Some(UiKey::Key(Key::_3)),
        KeyCode::KEY_4 => Some(UiKey::Key(Key::_4)),
        KeyCode::KEY_5 => Some(UiKey::Key(Key::_5)),
        KeyCode::KEY_6 => Some(UiKey::Key(Key::_6)),
        KeyCode::KEY_7 => Some(UiKey::Key(Key::_7)),
        KeyCode::KEY_8 => Some(UiKey::Key(Key::_8)),
        KeyCode::KEY_9 => Some(UiKey::Key(Key::_9)),
        KeyCode::KEY_ESC => Some(UiKey::Key(Key::Esc)),
        KeyCode::KEY_ENTER => Some(UiKey::Key(Key::Enter)),
        KeyCode::KEY_BACKSPACE => Some(UiKey::Key(Key::BackSpace)),
        KeyCode::KEY_TAB => Some(UiKey::Key(Key::Tab)),
        KeyCode::KEY_SPACE => Some(UiKey::Key(Key::Space)),
        KeyCode::KEY_MINUS => Some(UiKey::Key(Key::Minus)),
        KeyCode::KEY_EQUAL => Some(UiKey::Key(Key::Equal)),
        KeyCode::KEY_LEFTBRACE => Some(UiKey::Key(Key::LeftBrace)),
        KeyCode::KEY_RIGHTBRACE => Some(UiKey::Key(Key::RightBrace)),
        KeyCode::KEY_BACKSLASH => Some(UiKey::Key(Key::BackSlash)),
        KeyCode::KEY_SEMICOLON => Some(UiKey::Key(Key::SemiColon)),
        KeyCode::KEY_APOSTROPHE => Some(UiKey::Key(Key::Apostrophe)),
        KeyCode::KEY_GRAVE => Some(UiKey::Key(Key::Grave)),
        KeyCode::KEY_COMMA => Some(UiKey::Key(Key::Comma)),
        KeyCode::KEY_DOT => Some(UiKey::Key(Key::Dot)),
        KeyCode::KEY_SLASH => Some(UiKey::Key(Key::Slash)),
        KeyCode::KEY_LEFTSHIFT => Some(UiKey::Key(Key::LeftShift)),
        KeyCode::KEY_RIGHTSHIFT => Some(UiKey::Key(Key::RightShift)),
        KeyCode::KEY_LEFTCTRL => Some(UiKey::Key(Key::LeftControl)),
        KeyCode::KEY_RIGHTCTRL => Some(UiKey::Key(Key::RightControl)),
        KeyCode::KEY_LEFTALT => Some(UiKey::Key(Key::LeftAlt)),
        KeyCode::KEY_RIGHTALT => Some(UiKey::Key(Key::RightAlt)),
        KeyCode::KEY_LEFTMETA => Some(UiKey::Key(Key::LeftMeta)),
        KeyCode::KEY_RIGHTMETA => Some(UiKey::Key(Key::RightMeta)),
        KeyCode::KEY_CAPSLOCK => Some(UiKey::Key(Key::CapsLock)),
        KeyCode::KEY_NUMLOCK => Some(UiKey::Key(Key::NumLock)),
        KeyCode::KEY_SCROLLLOCK => Some(UiKey::Key(Key::ScrollLock)),
        KeyCode::KEY_UP => Some(UiKey::Key(Key::Up)),
        KeyCode::KEY_DOWN => Some(UiKey::Key(Key::Down)),
        KeyCode::KEY_LEFT => Some(UiKey::Key(Key::Left)),
        KeyCode::KEY_RIGHT => Some(UiKey::Key(Key::Right)),
        KeyCode::KEY_HOME => Some(UiKey::Key(Key::Home)),
        KeyCode::KEY_END => Some(UiKey::Key(Key::End)),
        KeyCode::KEY_PAGEUP => Some(UiKey::Key(Key::PageUp)),
        KeyCode::KEY_PAGEDOWN => Some(UiKey::Key(Key::PageDown)),
        KeyCode::KEY_INSERT => Some(UiKey::Key(Key::Insert)),
        KeyCode::KEY_DELETE => Some(UiKey::Key(Key::Delete)),
        KeyCode::KEY_KP0 => Some(UiKey::KeyPad(KeyPad::_0)),
        KeyCode::KEY_KP1 => Some(UiKey::KeyPad(KeyPad::_1)),
        KeyCode::KEY_KP2 => Some(UiKey::KeyPad(KeyPad::_2)),
        KeyCode::KEY_KP3 => Some(UiKey::KeyPad(KeyPad::_3)),
        KeyCode::KEY_KP4 => Some(UiKey::KeyPad(KeyPad::_4)),
        KeyCode::KEY_KP5 => Some(UiKey::KeyPad(KeyPad::_5)),
        KeyCode::KEY_KP6 => Some(UiKey::KeyPad(KeyPad::_6)),
        KeyCode::KEY_KP7 => Some(UiKey::KeyPad(KeyPad::_7)),
        KeyCode::KEY_KP8 => Some(UiKey::KeyPad(KeyPad::_8)),
        KeyCode::KEY_KP9 => Some(UiKey::KeyPad(KeyPad::_9)),
        KeyCode::KEY_KPASTERISK => Some(UiKey::KeyPad(KeyPad::Asterisk)),
        KeyCode::KEY_KPPLUS => Some(UiKey::KeyPad(KeyPad::Plus)),
        KeyCode::KEY_KPMINUS => Some(UiKey::KeyPad(KeyPad::Minus)),
        KeyCode::KEY_KPDOT => Some(UiKey::KeyPad(KeyPad::Dot)),
        KeyCode::KEY_KPENTER => Some(UiKey::KeyPad(KeyPad::Enter)),
        KeyCode::KEY_KPSLASH => Some(UiKey::KeyPad(KeyPad::Slash)),
        KeyCode::KEY_KPEQUAL => Some(UiKey::KeyPad(KeyPad::Equal)),
        KeyCode::KEY_KPCOMMA => Some(UiKey::KeyPad(KeyPad::Comma)),
        KeyCode::KEY_PAUSE => Some(UiKey::Misc(Misc::Pause)),
        KeyCode::KEY_SYSRQ => Some(UiKey::Key(Key::SysRq)),
        KeyCode::KEY_MUTE => Some(UiKey::Misc(Misc::Mute)),
        KeyCode::KEY_VOLUMEUP => Some(UiKey::Misc(Misc::VolumeUp)),
        KeyCode::KEY_VOLUMEDOWN => Some(UiKey::Misc(Misc::VolumeDown)),
        KeyCode::KEY_SLEEP => Some(UiKey::Misc(Misc::Sleep)),
        KeyCode::KEY_WAKEUP => Some(UiKey::Misc(Misc::WakeUp)),
        KeyCode::KEY_F1 => Some(UiKey::Key(Key::F1)),
        KeyCode::KEY_F2 => Some(UiKey::Key(Key::F2)),
        KeyCode::KEY_F3 => Some(UiKey::Key(Key::F3)),
        KeyCode::KEY_F4 => Some(UiKey::Key(Key::F4)),
        KeyCode::KEY_F5 => Some(UiKey::Key(Key::F5)),
        KeyCode::KEY_F6 => Some(UiKey::Key(Key::F6)),
        KeyCode::KEY_F7 => Some(UiKey::Key(Key::F7)),
        KeyCode::KEY_F8 => Some(UiKey::Key(Key::F8)),
        KeyCode::KEY_F9 => Some(UiKey::Key(Key::F9)),
        KeyCode::KEY_F10 => Some(UiKey::Key(Key::F10)),
        KeyCode::KEY_F11 => Some(UiKey::Key(Key::F11)),
        KeyCode::KEY_F12 => Some(UiKey::Key(Key::F12)),
        _ => None,
    }
}