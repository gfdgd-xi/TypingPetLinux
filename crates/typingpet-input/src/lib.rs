//! 全局输入后端。
//!
//! Linux 现实：
//! - **evdev**（`/dev/input/event*`）是唯一能在 X11 与 Wayland 下都拿到"任意按键"的途径，
//!   需要 udev `uaccess` 规则或把用户加入 `input` 组；
//! - **X11**（XRecord/XInput2）在 KDE Wayland 下默认只能看到按住修饰键的按键，
//!   因此本 crate 用它只做"全局光标位置"这类 X11 仍开放的能力；
//! - Wayland 下客户端拿不到全局光标坐标，只能靠宠物窗口自身的输入区域做"局部躲避"。

pub mod evdev_source;
#[cfg(feature = "x11")]
pub mod x11;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;
use typingpet_core::keys::{KeyModifiers, KeyStroke, MouseButton};

/// 一次按键事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub stroke: KeyStroke,
    /// 来自哪个设备（便于日志/排查）
    pub device_index: usize,
    /// `true` = 按下，`false` = 抬起。抬起事件用于让上层知道"键仍被按住"，
    /// 从而长按期间不回待机，直到抬起才切换（见 typingpet-core 的 PetState）。
    pub pressed: bool,
}

/// 一次鼠标按键事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: MouseButton,
    /// `true` = 按下，`false` = 抬起。抬起后上层切回待机图。
    pub pressed: bool,
}

/// 统一的输入事件：键盘或鼠标，经同一 channel 投递（见 gui 的事件分发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
}

/// 光标位置（X11 root 坐标系：原点左上、y 向下）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerPosition {
    pub x: f64,
    pub y: f64,
}

/// 屏幕尺寸（像素）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenSize {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Evdev,
    Mock,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Evdev => "evdev",
            Self::Mock => "mock",
        }
    }
}

/// 运行中的输入源：一个后台线程 + 一个 channel。
pub struct InputHandle {
    receiver: Receiver<InputEvent>,
    backend: Backend,
    description: String,
}

impl InputHandle {
    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn try_recv(&self) -> Option<InputEvent> {
        self.receiver.try_recv().ok()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<InputEvent> {
        match self.receiver.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

/// 打开 evdev 全局键盘+鼠标监听；没有任何可用键盘设备时返回错误（调用方可降级）。
pub fn open_evdev() -> anyhow::Result<InputHandle> {
    let (sender, receiver) = mpsc::channel();
    let devices = evdev_source::spawn_keyboard_listeners(sender.clone())?;
    // 鼠标监听失败不致命：没有权限/设备时键盘仍可正常工作。
    let mice = evdev_source::spawn_mouse_listeners(sender).unwrap_or(0);
    Ok(InputHandle {
        receiver,
        backend: Backend::Evdev,
        description: format!("evdev: {devices} keyboard device(s), {mice} mouse device(s)"),
    })
}

/// 用于测试/演示的 mock 输入源：按脚本回放按键。
pub fn open_mock(script: Vec<KeyStroke>, interval: Duration) -> InputHandle {
    let (sender, receiver) = mpsc::channel();
    let count = script.len();
    thread::spawn(move || {
        for (index, stroke) in script.into_iter().enumerate() {
            // 模拟一次"按下 + 抬起"的轻点，避免上层一直认为键被按住。
            if sender
                .send(InputEvent::Key(KeyEvent {
                    stroke,
                    device_index: usize::MAX,
                    pressed: true,
                }))
                .is_err()
                || sender
                    .send(InputEvent::Key(KeyEvent {
                        stroke,
                        device_index: usize::MAX,
                        pressed: false,
                    }))
                    .is_err()
            {
                return;
            }
            thread::sleep(interval);
            let _ = index;
        }
    });
    InputHandle {
        receiver,
        backend: Backend::Mock,
        description: format!("mock: {count} scripted keystroke(s)"),
    }
}

/// 判断当前是否运行在 X11 会话下（有 DISPLAY 且没有 Wayland socket）。
pub fn is_x11_session() -> bool {
    #[cfg(feature = "x11")]
    {
        x11::is_available()
    }
    #[cfg(not(feature = "x11"))]
    {
        false
    }
}

/// 当前会话类型，仅用于日志/UI 提示。
pub fn session_kind() -> &'static str {
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if session_type.eq_ignore_ascii_case("wayland") {
        "wayland"
    } else if session_type.eq_ignore_ascii_case("x11") {
        "x11"
    } else if is_x11_session() {
        "x11"
    } else {
        "unknown"
    }
}

/// 全局光标位置。Wayland 下返回 `None`（协议不允许客户端查询全局光标）。
pub fn pointer_position() -> Option<PointerPosition> {
    if session_kind() == "wayland" && std::env::var_os("DISPLAY").is_none() {
        return None;
    }
    #[cfg(feature = "x11")]
    {
        x11::pointer_position()
    }
    #[cfg(not(feature = "x11"))]
    {
        None
    }
}

/// 屏幕尺寸（像素）。
pub fn screen_size() -> Option<ScreenSize> {
    #[cfg(feature = "x11")]
    {
        x11::screen_size()
    }
    #[cfg(not(feature = "x11"))]
    {
        None
    }
}

/// 修饰键位判定，供各后端共用。
pub fn modifier_bit_for(code: u32) -> Option<u8> {
    use typingpet_core::keys::KeyModifiers as M;
    match code {
        29 | 97 => Some(M::CONTROL),
        56 | 100 | 99 => Some(M::ALT),
        42 | 54 => Some(M::SHIFT),
        125 | 126 => Some(M::SUPER),
        _ => None,
    }
}

/// 更新修饰键状态（按下置位、松开清位）。
pub fn update_modifiers(modifiers: &mut KeyModifiers, code: u32, pressed: bool) {
    if let Some(bit) = modifier_bit_for(code) {
        modifiers.set(bit, pressed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_bits_match_evdev_codes() {
        assert_eq!(modifier_bit_for(29), Some(KeyModifiers::CONTROL));
        assert_eq!(modifier_bit_for(97), Some(KeyModifiers::CONTROL));
        assert_eq!(modifier_bit_for(56), Some(KeyModifiers::ALT));
        assert_eq!(modifier_bit_for(42), Some(KeyModifiers::SHIFT));
        assert_eq!(modifier_bit_for(125), Some(KeyModifiers::SUPER));
        assert_eq!(modifier_bit_for(37), None);
    }

    #[test]
    fn modifiers_track_press_and_release() {
        let mut modifiers = KeyModifiers::empty();
        update_modifiers(&mut modifiers, 29, true);
        assert!(modifiers.contains(KeyModifiers::CONTROL));
        update_modifiers(&mut modifiers, 29, false);
        assert!(!modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn mock_source_replays_script() {
        let handle = open_mock(
            vec![
                KeyStroke::new(37, KeyModifiers::NONE),
                KeyStroke::new(38, KeyModifiers::empty().with(KeyModifiers::CONTROL)),
            ],
            Duration::from_millis(1),
        );
        // mock 每个键都会发"按下+抬起"，按脚本顺序回放，收集到的键码应包含 37 与 38
        let mut codes = Vec::new();
        while let Some(event) = handle.recv_timeout(Duration::from_secs(1)) {
            if let InputEvent::Key(key) = event {
                codes.push(key.stroke.code);
            }
        }
        assert!(codes.contains(&37));
        assert!(codes.contains(&38));
        assert_eq!(handle.backend(), Backend::Mock);
    }
}
