//! 统一的输入事件模型。
//!
//! 客户端把触摸、笔、鼠标都归一化成这里的事件；主机端再映射到
//! `SendInput` / `InjectTouchInput` / `InjectSyntheticPointerInput`。
//!
//! 坐标一律用**归一化 0.0 ~ 1.0**，相对于被采集的那块屏幕。
//! 这样客户端换分辨率、主机换显示器，都不需要重新换算。

/// 主机侧可注入的设备类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// 鼠标模式：单指拖动等于移动鼠标。
    Mouse,
    /// 直接触摸：手指位置就是触点。
    Touch,
    /// 数位板：带压感和倾角。
    Pen,
}

#[derive(Debug, Clone)]
pub enum InputEvent {
    PointerMove { x: f32, y: f32 },

    PointerButton { button: MouseButton, pressed: bool },

    PointerScroll { dx: f32, dy: f32 },

    /// 扫描码输入。适合快捷键和英文。
    Key { code: u32, pressed: bool },

    /// 文本输入。中文必须走这条，不要发按键序列。
    Text(String),

    /// 多点触摸。`id` 是客户端分配的触点编号。
    Touch {
        id: u8,
        x: f32,
        y: f32,
        pressure: f32,
    },

    /// 数位板。
    Pen {
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        /// 笔杆旋转，单位度。
        rotation: f32,
        barrel_button: bool,
        eraser: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}
