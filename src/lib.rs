use std::path::PathBuf;

/// Unix socket that niri-zoomctl uses to talk to niri-zoomd.
pub fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir).join("niri-zoomd.sock")
}

pub const ZOOM_STEP: f32 = 0.25;
pub const MIN_ZOOM: f32 = 1.0;
pub const MAX_ZOOM: f32 = 6.0;

/// A minimal 3x5 bitmap font, just enough glyphs to render "2.75x" style badges.
/// Each glyph is 5 rows of a 3-bit-wide bitmask (bit 2 = leftmost column).
pub fn glyph(c: char) -> [u8; 5] {
    match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        'x' => [0b000, 0b101, 0b010, 0b101, 0b000],
        _ => [0, 0, 0, 0, 0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn zoom_constants_are_sane() {
        assert!(MIN_ZOOM < MAX_ZOOM);
        assert!(ZOOM_STEP > 0.0);
        assert_eq!(MIN_ZOOM, 1.0);
        assert!(ZOOM_STEP * 20.0 >= MAX_ZOOM - MIN_ZOOM);
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir() {
        let dir = "/run/user/1000";
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", dir);
        }
        assert_eq!(
            socket_path().to_string_lossy(),
            format!("{dir}/niri-zoomd.sock")
        );
    }

    #[test]
    fn socket_path_falls_back_to_tmp() {
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        assert_eq!(socket_path().to_string_lossy(), "/tmp/niri-zoomd.sock");
    }

    #[test]
    fn glyph_mask_height_is_five() {
        for c in "0123456789.xF".chars() {
            assert_eq!(glyph(c).len(), 5, "glyph for {c:?} must be 5 rows");
        }
    }

    #[test]
    fn known_glyph_bitmaps() {
        assert_eq!(glyph('8'), [0b111, 0b101, 0b111, 0b101, 0b111]);
        assert_eq!(glyph('.'), [0b000, 0b000, 0b000, 0b000, 0b010]);
        assert_eq!(glyph('x'), [0b000, 0b101, 0b010, 0b101, 0b000]);
        assert_eq!(glyph('?'), [0, 0, 0, 0, 0]);
    }
}
