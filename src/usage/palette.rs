use super::{Provider, WindowKind};
pub const PINK: (u8, u8, u8) = (0xff, 0x7a, 0xb6);
pub const RED: (u8, u8, u8) = (0xe5, 0x48, 0x4d);
pub const EMPTY: (u8, u8, u8) = (0x34, 0x34, 0x3c);
pub fn window_rgb(provider: Provider, kind: WindowKind) -> (u8, u8, u8) {
    match (provider, kind) {
        (Provider::Claude, WindowKind::FiveHour) => (0xf6, 0xb2, 0x6b),
        (Provider::Claude, _) => (0xc8, 0x64, 0x1e),
        (Provider::Codex, WindowKind::FiveHour) => (0x6f, 0xd3, 0xb8),
        (Provider::Codex, _) => (0x14, 0x90, 0x7a),
        (Provider::Cursor, _) => (0x7a, 0x8c, 0xa8),
    }
}
pub fn level_rgb(provider: Provider, kind: WindowKind, left_pct: Option<u8>) -> (u8, u8, u8) {
    match left_pct {
        Some(0..=10) => RED,
        Some(11..=20) => PINK,
        _ => window_rgb(provider, kind),
    }
}
