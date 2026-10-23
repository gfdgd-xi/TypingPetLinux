//! 应用状态机：按键 → 挑图 → 弹跳 → 0.75s 回待机。
//!
//! 这一层刻意不依赖任何 GUI/输入实现，便于 headless 全链路验证。

use crate::keys::{KeyReactionStore, KeyStroke};
use crate::picker::Rng;
use std::path::{Path, PathBuf};

/// 回到待机图的去抖窗口（秒）。
///
/// 仅用于避免"快速打字"时每次抬起都闪一下待机图：只要两次按键间隔小于本值，
/// 宠物就保持在反应图；超过本值（即你停下打字）才切回待机图。本值刻意做得很小，
/// 让停下打字后几乎立刻回待机，不再有旧版 0.75s 的拖沓感。
pub const IDLE_DELAY: f64 = 0.2;

/// 判定为"长按"的阈值（秒）。按住超过此时长的按键，松开后**立即**回待机图（不等
/// `IDLE_DELAY`）；短于它的视作快速输入，保留 `IDLE_DELAY` 去抖。
pub const HOLD_THRESHOLD: f64 = 0.3;

/// 判定"同时按两个键"的时间窗（秒）。
///
/// 输入层只上报"按下"事件（自动重复已过滤），无法区分键的物理按住状态，
/// 因此用两次按下之间的时间差近似：间隔小于此阈值且键码不同，即视为同时按下。
/// 普通打字两键之间通常 > 100ms，而真正同时按下的两键间隔 < 10ms，40ms 足够区分。
const SIMULTANEOUS_WINDOW: f64 = 0.04;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 显示某张图片
    ShowImage(PathBuf),
    /// 播放弹跳动画，参数为振幅（像素）
    Bounce(f64),
}

#[derive(Debug)]
pub struct PetState {
    idle_url: Option<PathBuf>,
    /// 所有反应图（含 exclamation），仅用于计数/兼容。
    reaction_urls: Vec<PathBuf>,
    /// 单键轮播池：除 exclamation 之外的反应图，按 left→right→left→right 顺序循环。
    sequence_urls: Vec<PathBuf>,
    /// 文件名含 "exclamation" 的图，仅在同时按两个键时显示。
    exclamation_url: Option<PathBuf>,
    /// 文件名含 "mousepress" 的图，按下鼠标时显示，松开鼠标切回待机。
    mousepress_url: Option<PathBuf>,
    /// 轮播池当前下标（每次单键按下前进一位）。
    sequence_index: usize,
    /// 上一次按下事件：键码 + 时间戳，用于"同时按两键"判定。
    last_press: Option<(u32, f64)>,
    current_image: Option<PathBuf>,
    idle_deadline: Option<f64>,
    shake_amplitude: f64,
    reaction_count: usize,
    /// 当前被按住（未抬起）的非修饰键数量。>0 时长按期间不回待机，
    /// 直到全部抬起才开始计时回待机图。
    held_count: usize,
    /// 从"无键按住"进入"有键按住"的时刻，用于区分长按/短按。
    held_since: Option<f64>,
    /// 当前被按住的鼠标按键数量。>0 时显示 mousepress 且长按期间不回待机，
    /// 直到全部抬起才切回待机图。
    mouse_held: usize,
}

/// 文件名（stem，大小写不敏感）是否含 "exclamation"。
fn is_exclamation(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.to_lowercase().contains("exclamation"))
        .unwrap_or(false)
}

/// 文件名（stem，大小写不敏感）是否含 "mousepress"。
fn is_mousepress(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.to_lowercase().contains("mousepress"))
        .unwrap_or(false)
}

impl PetState {
    pub fn new(
        idle_url: Option<PathBuf>,
        reaction_urls: Vec<PathBuf>,
        shake_amplitude: f64,
    ) -> Self {
        let (sequence_urls, exclamation_url, mousepress_url) = split_reactions(&reaction_urls);
        Self {
            reaction_count: reaction_urls.len(),
            idle_url,
            reaction_urls,
            sequence_urls,
            exclamation_url,
            mousepress_url,
            sequence_index: 0,
            last_press: None,
            current_image: None,
            idle_deadline: None,
            shake_amplitude,
            held_count: 0,
            held_since: None,
            mouse_held: 0,
        }
    }

    pub fn update_sources(
        &mut self,
        idle_url: Option<PathBuf>,
        reaction_urls: Vec<PathBuf>,
        shake_amplitude: f64,
    ) {
        let (sequence_urls, exclamation_url, mousepress_url) = split_reactions(&reaction_urls);
        self.reaction_count = reaction_urls.len();
        self.idle_url = idle_url;
        self.reaction_urls = reaction_urls;
        self.sequence_urls = sequence_urls;
        self.exclamation_url = exclamation_url;
        self.mousepress_url = mousepress_url;
        self.sequence_index = 0;
        self.last_press = None;
        self.shake_amplitude = shake_amplitude;
        self.current_image = None;
        self.idle_deadline = None;
        self.held_count = 0;
        self.held_since = None;
        self.mouse_held = 0;
    }

    pub fn current_image(&self) -> Option<&PathBuf> {
        self.current_image.as_ref()
    }

    pub fn reaction_count(&self) -> usize {
        self.reaction_count
    }

    pub fn is_showing_idle(&self) -> bool {
        match (&self.current_image, &self.idle_url) {
            (Some(current), Some(idle)) => current == idle,
            (None, _) => true,
            _ => false,
        }
    }

    /// 待机图 URL（用于启动时显示）。
    pub fn idle_url(&self) -> Option<&PathBuf> {
        self.idle_url.as_ref()
    }

    /// 首次显示：返回待机图动作。
    pub fn initial_actions(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(idle) = self.idle_url.clone() {
            self.current_image = Some(idle.clone());
            actions.push(Action::ShowImage(idle));
        }
        actions
    }

    /// 键输入处理（按下）。`stroke` 为 `None` 时表示"测试按钮"，按单键逻辑轮播。
    ///
    /// 按下时记一个"按住"计数；长按期间 `tick` 会据此抑制回待机，
    /// 直到所有键抬起（`on_key_up`）才开始计时。
    pub fn on_key(
        &mut self,
        stroke: Option<KeyStroke>,
        rules: &KeyReactionStore,
        _rng: &mut impl Rng,
        now: f64,
    ) -> Vec<Action> {
        let chosen = self.choose_image(stroke, rules, now);

        let Some(chosen) = chosen else {
            return Vec::new();
        };

        let mut actions = Vec::new();
        self.current_image = Some(chosen.clone());
        actions.push(Action::ShowImage(chosen));
        if self.shake_amplitude > 0.0 {
            actions.push(Action::Bounce(self.shake_amplitude));
        }
        if self.held_count == 0 {
            // 从"无键按住"进入"有键按住"：记录起始时刻，用于区分长按/短按
            self.held_since = Some(now);
        }
        self.held_count = self.held_count.saturating_add(1);
        self.idle_deadline = Some(now + IDLE_DELAY);
        actions
    }

    /// 键抬起处理。`stroke` 暂未用于选图，仅用于未来按具体键的精细控制。
    ///
    /// 每抬起一个按住的非修饰键，计数减一；当计数归零（最后一个键松开），
    /// 安排 `IDLE_DELAY` 后回待机图。长按期间不会回待机。
    pub fn on_key_up(&mut self, _stroke: Option<KeyStroke>, now: f64) -> Vec<Action> {
        if self.held_count == 0 {
            return Vec::new();
        }
        self.held_count -= 1;
        if self.held_count > 0 {
            return Vec::new();
        }
        // 最后一个键松开：根据按住时长决定回待机时机
        let was_long_press = self
            .held_since
            .map(|start| now - start >= HOLD_THRESHOLD)
            .unwrap_or(false);
        self.held_since = None;
        // 长按（按住较久）：松开后**立即**回待机，不等 0.75s；
        // 短按/快速连打：保留 IDLE_DELAY 去抖，避免两次按键之间闪回待机图。
        self.idle_deadline = Some(if was_long_press { now } else { now + IDLE_DELAY });
        Vec::new()
    }

    /// 鼠标按下：若图集含 `mousepress` 图，切换到该图并弹跳；否则无操作。
    ///
    /// 按住期间保持该图（与键盘长按一样不会回待机），直到 `on_mouse_up` 切回待机。
    pub fn on_mouse_down(
        &mut self,
        _button: crate::keys::MouseButton,
        _now: f64,
    ) -> Vec<Action> {
        let Some(url) = self.mousepress_url.clone() else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        self.current_image = Some(url.clone());
        actions.push(Action::ShowImage(url));
        if self.shake_amplitude > 0.0 {
            actions.push(Action::Bounce(self.shake_amplitude));
        }
        self.mouse_held = self.mouse_held.saturating_add(1);
        // 鼠标按住期间不回待机：清掉待机截止时间，交给 tick 的 mouse_held 守卫。
        self.idle_deadline = None;
        actions
    }

    /// 鼠标抬起：计数减一；最后一个鼠标键松开且没有任何键按住时，立即切回待机图。
    pub fn on_mouse_up(
        &mut self,
        _button: crate::keys::MouseButton,
        now: f64,
    ) -> Vec<Action> {
        if self.mouse_held == 0 {
            return Vec::new();
        }
        self.mouse_held -= 1;
        if self.mouse_held > 0 {
            return Vec::new();
        }
        // 鼠标松开且键盘也没有键按住 → 立即回待机（"松开鼠标则变回 idle"）。
        if self.held_count == 0 {
            self.idle_deadline = Some(now);
        }
        Vec::new()
    }

    /// 选图：精确规则 > 同时按 >=2 键(exclamation) > 单键按顺序轮播。
    fn choose_image(
        &mut self,
        stroke: Option<KeyStroke>,
        rules: &KeyReactionStore,
        now: f64,
    ) -> Option<PathBuf> {
        let Some(stroke) = stroke else {
            // 测试按钮：按单键逻辑轮播下一张
            return self.next_sequence();
        };

        // 1) 精确规则优先
        if let Some(rule_image) = rules.image_path(stroke) {
            self.last_press = Some((stroke.code, now));
            return Some(rule_image);
        }

        // 2) 与"上一次按下"间隔极短且键码不同 → 视为同时按多个键（和弦）。
        //    判定为 >= 2 个键同时按即切换到 exclamation，不仅限于成对：
        //    不清空 last_press，让第 2、3、… 个同和弦键都触发 exclamation。
        if let Some((last_code, last_time)) = self.last_press {
            if last_code != stroke.code && now - last_time <= SIMULTANEOUS_WINDOW {
                if let Some(exclamation) = &self.exclamation_url {
                    self.last_press = Some((stroke.code, now));
                    return Some(exclamation.clone());
                }
            }
        }
        self.last_press = Some((stroke.code, now));

        // 3) 普通单键：按 left→right→left→right 顺序轮播
        self.next_sequence()
    }

    /// 轮播下一张：依次取 sequence_urls[sequence_index]，然后下标前进。
    fn next_sequence(&mut self) -> Option<PathBuf> {
        if self.sequence_urls.is_empty() {
            return None;
        }
        let path = self.sequence_urls[self.sequence_index % self.sequence_urls.len()].clone();
        self.sequence_index = (self.sequence_index + 1) % self.sequence_urls.len();
        Some(path)
    }

    /// 定时推进：超时后回待机。长按（仍有键按住）期间不回待机。
    pub fn tick(&mut self, now: f64) -> Vec<Action> {
        if self.held_count > 0 || self.mouse_held > 0 {
            // 仍有键或鼠标按住：保持当前反应图，不回待机
            return Vec::new();
        }
        let Some(deadline) = self.idle_deadline else {
            return Vec::new();
        };
        if now < deadline {
            return Vec::new();
        }
        self.idle_deadline = None;

        let Some(idle) = self.idle_url.clone() else {
            return Vec::new();
        };
        if self.current_image.as_ref() == Some(&idle) {
            return Vec::new();
        }
        self.current_image = Some(idle.clone());
        vec![Action::ShowImage(idle)]
    }
}

/// 把反应图拆成"轮播池"、"exclamation（双键）"与"mousepress（鼠标按下）"。
fn split_reactions(urls: &[PathBuf]) -> (Vec<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
    let mut exclamation: Option<PathBuf> = None;
    let mut mousepress: Option<PathBuf> = None;
    let sequence: Vec<PathBuf> = urls
        .iter()
        .filter(|path| {
            if is_exclamation(path) {
                if exclamation.is_none() {
                    exclamation = Some((*path).clone());
                }
                false
            } else if is_mousepress(path) {
                if mousepress.is_none() {
                    mousepress = Some((*path).clone());
                }
                false
            } else {
                true
            }
        })
        .cloned()
        .collect();
    (sequence, exclamation, mousepress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::FixedRng;

    fn state() -> PetState {
        PetState::new(
            Some(PathBuf::from("/img/pet-idle.png")),
            vec![
                PathBuf::from("/img/pet-left.png"),
                PathBuf::from("/img/pet-right.png"),
                PathBuf::from("/img/pet-question.png"),
            ],
            9.0,
        )
    }

    #[test]
    fn initial_actions_show_idle_image() {
        let mut state = state();
        assert_eq!(
            state.initial_actions(),
            vec![Action::ShowImage(PathBuf::from("/img/pet-idle.png"))]
        );
        assert!(state.is_showing_idle());
    }

    #[test]
    fn keypress_shows_reaction_then_returns_to_idle() {
        let mut state = state();
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);

        let actions = state.on_key(None, &rules, &mut rng, 100.0);
        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], Action::ShowImage(_)));
        assert_eq!(actions[1], Action::Bounce(9.0));
        assert!(!state.is_showing_idle());

        // 键仍按住时（未抬起）不回待机：模拟长按
        assert!(state.tick(100.7).is_empty());
        assert!(state.tick(200.0).is_empty());
        assert!(!state.is_showing_idle());

        // 抬起（长按：从 100.0 按住到 200.0）后应立即回待机，不等 0.75s
        state.on_key_up(None, 200.0);
        assert_eq!(
            state.tick(200.01),
            vec![Action::ShowImage(PathBuf::from("/img/pet-idle.png"))]
        );
        assert!(state.is_showing_idle());
        // 已回待机后不再重复
        assert!(state.tick(201.0).is_empty());
    }

    #[test]
    fn short_press_keeps_debounce_between_keys() {
        let mut state = state();
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);

        // 短按：按下 0.0、抬起 0.1（< HOLD_THRESHOLD）→ 仅保留很短的去抖窗口
        state.on_key(None, &rules, &mut rng, 0.0);
        state.on_key_up(None, 0.1);
        // 0.1 + IDLE_DELAY(0.2) = 0.3 之前不回待机
        assert!(state.tick(0.2).is_empty());
        assert_eq!(
            state.tick(0.31),
            vec![Action::ShowImage(PathBuf::from("/img/pet-idle.png"))]
        );
        assert!(state.is_showing_idle());

        // 快速连打两键：中间不闪回待机图
        state.on_key(None, &rules, &mut rng, 1.0);
        state.on_key_up(None, 1.1);
        state.on_key(None, &rules, &mut rng, 1.2);
        // 第二次按下后当前是反应图，去抖期内不回待机
        assert!(!state.is_showing_idle());
        assert!(state.tick(1.35).is_empty()); // 1.35 < 1.2+0.2
        state.on_key_up(None, 1.3);
        // 1.3 + 0.2 = 1.5 之后才回待机
        assert!(state.tick(1.4).is_empty());
        assert_eq!(
            state.tick(1.51),
            vec![Action::ShowImage(PathBuf::from("/img/pet-idle.png"))]
        );
    }

    #[test]
    fn hold_keeps_reaction_until_release() {
        let mut state = state();
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);
        let stroke = KeyStroke::new(37, crate::keys::KeyModifiers::empty());

        // 按下
        state.on_key(Some(stroke), &rules, &mut rng, 0.0);
        assert!(!state.is_showing_idle());

        // 长按 10 秒也不回待机
        for t in [1.0, 5.0, 10.0] {
            assert!(state.tick(t).is_empty());
        }
        assert!(!state.is_showing_idle());

        // 按住期间再按一个新键 → 切换到下一张反应图，仍不回待机
        let other = KeyStroke::new(38, crate::keys::KeyModifiers::empty());
        let actions = state.on_key(Some(other), &rules, &mut rng, 10.1);
        assert!(matches!(actions.first(), Some(Action::ShowImage(_))));
        assert!(!state.is_showing_idle());
        assert!(state.tick(11.0).is_empty());

        // 抬起第一个键：仍有一个键按住，继续不回待机
        state.on_key_up(Some(stroke), 11.5);
        assert!(state.tick(12.0).is_empty());

        // 抬起最后一个键：这是一次长达 12s 的长按，松开后立即回待机
        state.on_key_up(Some(other), 12.0);
        assert_eq!(
            state.tick(12.01),
            vec![Action::ShowImage(PathBuf::from("/img/pet-idle.png"))]
        );
        assert!(state.is_showing_idle());
    }

    #[test]
    fn reaction_never_repeats_the_image_shown_right_before() {
        let mut state = state();
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);
        let mut previous = state.current_image().cloned();

        for _ in 0..50 {
            let actions = state.on_key(None, &rules, &mut rng, 200.0);
            let shown = match actions.first() {
                Some(Action::ShowImage(path)) => path.clone(),
                _ => panic!("expected ShowImage"),
            };
            assert_ne!(Some(shown.clone()), previous, "连续两次显示了同一张反应图");
            previous = Some(shown);
        }
    }

    #[test]
    fn exact_rule_wins_over_random_reaction() {
        let root = std::env::temp_dir().join(format!("typingpet-state-{}", crate::unique_id()));
        let source = root.join("special.png");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(&source, b"\x89PNG\r\n\x1a\n").expect("write");

        let store_dir = root.join("rules");
        let mut store = KeyReactionStore::new(&store_dir);
        let stroke = KeyStroke::new(37, crate::keys::KeyModifiers::empty());
        store.set_rule(stroke, &source).expect("set rule");
        let expected = store.image_path(stroke).expect("rule image");

        let mut state = state();
        state.initial_actions();
        let mut rng = FixedRng(0);
        let actions = state.on_key(Some(stroke), &store, &mut rng, 0.0);
        assert_eq!(actions[0], Action::ShowImage(expected));

        // 未注册的按键走随机反应
        let other = KeyStroke::new(38, crate::keys::KeyModifiers::empty());
        let actions = state.on_key(Some(other), &store, &mut rng, 0.0);
        assert!(matches!(actions.first(), Some(Action::ShowImage(_))));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn zero_shake_level_skips_bounce() {
        let mut state = PetState::new(
            Some(PathBuf::from("/img/pet-idle.png")),
            vec![PathBuf::from("/img/a.png")],
            0.0,
        );
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);
        let actions = state.on_key(None, &rules, &mut rng, 0.0);
        assert_eq!(actions.len(), 1);
    }

    /// 默认反应集：left/right 轮播，exclamation 留给双键。
    fn chord_state() -> PetState {
        PetState::new(
            Some(PathBuf::from("/img/pet-idle.png")),
            vec![
                PathBuf::from("/img/pet-left.png"),
                PathBuf::from("/img/pet-right.png"),
                PathBuf::from("/img/pet-exclamation.png"),
            ],
            9.0,
        )
    }

    #[test]
    fn single_key_cycles_left_right_in_order() {
        let mut state = chord_state();
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);
        let stroke = KeyStroke::new(37, crate::keys::KeyModifiers::empty());
        let expected_order = [
            PathBuf::from("/img/pet-left.png"),
            PathBuf::from("/img/pet-right.png"),
            PathBuf::from("/img/pet-left.png"),
            PathBuf::from("/img/pet-right.png"),
        ];
        for expected in expected_order {
            let actions = state.on_key(Some(stroke), &rules, &mut rng, 1.0);
            assert_eq!(actions[0], Action::ShowImage(expected));
        }
    }

    #[test]
    fn two_simultaneous_keys_show_exclamation() {
        let mut state = chord_state();
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);
        let first = KeyStroke::new(37, crate::keys::KeyModifiers::empty());
        let second = KeyStroke::new(38, crate::keys::KeyModifiers::empty());
        // 第一个键：普通反应（left），并记录时间戳
        state.on_key(Some(first), &rules, &mut rng, 1.0);
        // 极短时间内第二个不同键：视为同时按下 → exclamation
        let actions = state.on_key(Some(second), &rules, &mut rng, 1.02);
        assert_eq!(
            actions[0],
            Action::ShowImage(PathBuf::from("/img/pet-exclamation.png"))
        );
    }

    #[test]
    fn three_or_more_simultaneous_keys_all_show_exclamation() {
        let mut state = chord_state();
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);
        let a = KeyStroke::new(37, crate::keys::KeyModifiers::empty());
        let b = KeyStroke::new(38, crate::keys::KeyModifiers::empty());
        let c = KeyStroke::new(39, crate::keys::KeyModifiers::empty());
        // 第一个键：普通反应（left）
        state.on_key(Some(a), &rules, &mut rng, 1.0);
        // 第二个键（同时按）→ exclamation
        assert_eq!(
            state.on_key(Some(b), &rules, &mut rng, 1.01)[0],
            Action::ShowImage(PathBuf::from("/img/pet-exclamation.png"))
        );
        // 第三个键（仍视为同时按的和弦）→ 也 exclamation（>= 2 键均触发）
        assert_eq!(
            state.on_key(Some(c), &rules, &mut rng, 1.02)[0],
            Action::ShowImage(PathBuf::from("/img/pet-exclamation.png"))
        );
    }

    #[test]
    fn not_simultaneous_when_keys_are_far_apart() {
        let mut state = chord_state();
        state.initial_actions();
        let rules = KeyReactionStore::new("/nonexistent-rules");
        let mut rng = FixedRng(0);
        let first = KeyStroke::new(37, crate::keys::KeyModifiers::empty());
        let second = KeyStroke::new(38, crate::keys::KeyModifiers::empty());
        state.on_key(Some(first), &rules, &mut rng, 1.0);
        // 200ms 后按下：远超同时判定窗，仍按单键轮播（right）
        let actions = state.on_key(Some(second), &rules, &mut rng, 1.20);
        assert_eq!(
            actions[0],
            Action::ShowImage(PathBuf::from("/img/pet-right.png"))
        );
    }

    #[test]
    fn mouse_press_shows_mousepress_and_release_returns_idle() {
        let mut state = PetState::new(
            Some(PathBuf::from("/img/pet-idle.png")),
            vec![
                PathBuf::from("/img/pet-left.png"),
                PathBuf::from("/img/pet-right.png"),
                PathBuf::from("/img/pet-mousepress.png"),
            ],
            9.0,
        );
        state.initial_actions();

        // 鼠标按下（集内含 pet-mousepress）→ 显示 mousepress
        let actions = state.on_mouse_down(crate::keys::MouseButton::Left, 1.0);
        assert_eq!(
            actions[0],
            Action::ShowImage(PathBuf::from("/img/pet-mousepress.png"))
        );
        // 鼠标按住期间不回待机
        assert!(state.tick(1.5).is_empty());
        assert!(!state.is_showing_idle());

        // 松开鼠标 → 立即回待机
        state.on_mouse_up(crate::keys::MouseButton::Left, 2.0);
        assert_eq!(
            state.tick(2.01),
            vec![Action::ShowImage(PathBuf::from("/img/pet-idle.png"))]
        );
        assert!(state.is_showing_idle());
    }

    #[test]
    fn mouse_press_without_mousepress_image_is_noop() {
        let mut state = state(); // 集内不含 mousepress 图
        let actions = state.on_mouse_down(crate::keys::MouseButton::Left, 1.0);
        assert!(actions.is_empty());
    }
}
