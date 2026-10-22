//! X11 只读查询 + 窗口移动。
//!
//! 用 x11rb 的纯 Rust 后端，不需要 libX11 头文件；未连接 X11 时全部返回 `None`。
//!
//! 连接是**复用**的：光标轮询是 30Hz，如果每次都重连会白白创建大量连接。

use crate::{PointerPosition, ScreenSize};
use std::cell::RefCell;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask,
};
use x11rb::rust_connection::RustConnection;

thread_local! {
    static CONNECTION: RefCell<Option<(RustConnection, usize)>> = const { RefCell::new(None) };
}

/// 在复用的连接上执行操作；首次调用时建立连接。
fn with_connection<T>(action: impl FnOnce(&RustConnection, usize) -> T) -> Option<T> {
    CONNECTION.with(|cell| {
        if cell.borrow().is_none() {
            match x11rb::connect(None) {
                Ok(pair) => *cell.borrow_mut() = Some(pair),
                Err(_) => return None,
            }
        }
        let borrowed = cell.borrow();
        let (connection, screen_number) = borrowed.as_ref()?;
        Some(action(connection, *screen_number))
    })
}

/// 是否有可用的 X11 连接。
pub fn is_available() -> bool {
    with_connection(|_, _| ()).is_some()
}

pub fn pointer_position() -> Option<PointerPosition> {
    with_connection(|connection, screen_number| {
        let root = connection.setup().roots.get(screen_number)?.root;
        let reply = connection.query_pointer(root).ok()?.reply().ok()?;
        Some(PointerPosition {
            x: f64::from(reply.root_x),
            y: f64::from(reply.root_y),
        })
    })
    .flatten()
}

pub fn screen_size() -> Option<ScreenSize> {
    with_connection(|connection, screen_number| {
        let screen = connection.setup().roots.get(screen_number)?;
        Some(ScreenSize {
            width: f64::from(screen.width_in_pixels),
            height: f64::from(screen.height_in_pixels),
        })
    })
    .flatten()
}

/// 把 X11 光标移动到屏幕绝对坐标。
///
/// 仅用于本地测试悬停/躲避（Wayland 协议不允许移动光标）。
/// 注意：在 Wayland 会话里经 XWayland 运行时，KWin 会忽略这个请求（调用不报错但光标不动）。
pub fn warp_pointer(x: i16, y: i16) -> Option<()> {
    with_connection(|connection, screen_number| {
        let root = connection.setup().roots.get(screen_number)?.root;
        connection
            .warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, x, y)
            .ok()?;
        connection.flush().ok()?;
        Some(())
    })
    .flatten()
}

/// 移动一个已映射的 X11 窗口（坐标：root 坐标系，物理像素）。
///
/// GTK4 没有任何窗口定位 API，X11 下只能直接发 `ConfigureWindow`；
/// layer-shell 那条路（`set_margin`）只在 Wayland 下可用。
pub fn move_window(window_id: u32, x: i32, y: i32) -> bool {
    with_connection(|connection, _| {
        let request = ConfigureWindowAux::new().x(x).y(y);
        connection.configure_window(window_id, &request).is_ok()
            && connection.flush().is_ok()
    })
    .unwrap_or(false)
}

/// 让窗口管理器把窗口置于最上层（X11 的 `_NET_WM_STATE_ABOVE`）。
///
/// 这是 layer-shell 之外，在普通 X11 会话里实现"置顶"的唯一办法。
/// 通过发送 `_NET_WM_STATE` ClientMessage 给窗口，由窗口管理器代为设置状态，
/// 因此 `above=false` 时只是撤销置顶（不影响窗口原本的层级）。
pub fn set_above(window_id: u32, above: bool) -> bool {
    with_connection(|connection, _screen_number| -> Option<()> {
        let net_wm_state = intern_atom(connection, b"_NET_WM_STATE")?;
        let net_wm_state_above = intern_atom(connection, b"_NET_WM_STATE_ABOVE")?;

        // data[0]: 动作（1=ADD, 0=REMOVE）；data[1]: 要切换的状态原子
        let data = [if above { 1 } else { 0 }, net_wm_state_above, 0, 0, 0];
        let event = ClientMessageEvent::new(32, window_id, net_wm_state, data);
        connection
            .send_event(
                true,
                window_id,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .ok()?;
        connection.flush().ok()?;
        Some(())
    })
    .is_some()
}

/// 让窗口管理器把窗口从任务栏/分页器里隐藏（X11 的 `_NET_WM_STATE_SKIP_TASKBAR` / `SKIP_PAGER`）。
///
/// GTK4 已移除 `skip-taskbar-hint` 属性，普通 X11 窗口只能直接发 `_NET_WM_STATE` ClientMessage。
pub fn set_skip_taskbar(window_id: u32, skip: bool) -> bool {
    with_connection(|connection, _screen_number| -> Option<()> {
        let net_wm_state = intern_atom(connection, b"_NET_WM_STATE")?;
        let skip_taskbar = intern_atom(connection, b"_NET_WM_STATE_SKIP_TASKBAR")?;
        let skip_pager = intern_atom(connection, b"_NET_WM_STATE_SKIP_PAGER")?;

        // data[0]: 动作（1=ADD, 0=REMOVE）；data[1..2]: 要切换的状态原子
        let data = [if skip { 1 } else { 0 }, skip_taskbar, skip_pager, 0, 0];
        let event = ClientMessageEvent::new(32, window_id, net_wm_state, data);
        connection
            .send_event(
                true,
                window_id,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .ok()?;
        connection.flush().ok()?;
        Some(())
    })
    .is_some()
}

/// 一次性把窗口的常驻状态设对：始终跳过任务栏/分页器，并按需置顶。
///
/// GTK4 不追踪 `_NET_WM_STATE`（相关类型化方法已移除），窗口管理器在重新评估窗口时
/// 可能把整窗口状态清掉，于是"置顶丢了、任务栏图标又回来"会间歇性出现。把多个变更
/// 合并到同一次连接里发送，并且由调用方周期性重申（见 gui.rs），即可自愈。
pub fn apply_states(window_id: u32, above: bool) -> bool {
    with_connection(|connection, _screen_number| -> Option<()> {
        let net_wm_state = intern_atom(connection, b"_NET_WM_STATE")?;
        let skip_taskbar = intern_atom(connection, b"_NET_WM_STATE_SKIP_TASKBAR")?;
        let skip_pager = intern_atom(connection, b"_NET_WM_STATE_SKIP_PAGER")?;
        let above_atom = intern_atom(connection, b"_NET_WM_STATE_ABOVE")?;

        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;

        // 1) 始终跳过任务栏/分页器（先确保，避免置顶切换把它带丢）
        let skip_data = [1, skip_taskbar, skip_pager, 0, 0];
        connection
            .send_event(
                true,
                window_id,
                mask,
                ClientMessageEvent::new(32, window_id, net_wm_state, skip_data),
            )
            .ok()?;

        // 2) 置顶/取消置顶（动作 1=ADD, 0=REMOVE）
        let above_data = [if above { 1 } else { 0 }, above_atom, 0, 0, 0];
        connection
            .send_event(
                true,
                window_id,
                mask,
                ClientMessageEvent::new(32, window_id, net_wm_state, above_data),
            )
            .ok()?;

        connection.flush().ok()?;
        Some(())
    })
    .is_some()
}

/// 取已 intern 的原子；失败返回 `None`。
fn intern_atom(connection: &RustConnection, name: &[u8]) -> Option<Atom> {
    connection
        .intern_atom(false, name)
        .ok()?
        .reply()
        .ok()
        .map(|reply| reply.atom)
}

/// 读回窗口在 root 坐标系里的真实位置与尺寸（用于自检/日志）。
pub fn window_geometry(window_id: u32) -> Option<(i32, i32, u32, u32)> {
    with_connection(|connection, _| {
        let geometry = connection.get_geometry(window_id).ok()?.reply().ok()?;
        let translated = connection
            .translate_coordinates(window_id, geometry.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some((
            i32::from(translated.dst_x),
            i32::from(translated.dst_y),
            u32::from(geometry.width),
            u32::from(geometry.height),
        ))
    })
    .flatten()
}
