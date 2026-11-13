//! Bound frame copies received from Chromium's paint callback.
pub fn byte_len(width: i32, height: i32) -> Option<usize> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    (bytes <= 64 * 1024 * 1024).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_or_excessive_frames_are_rejected_before_copying() {
        assert_eq!(byte_len(720, 500), Some(1_440_000));
        for (w, h) in [
            (0, 500),
            (-1, 500),
            (720, 0),
            (i32::MAX, i32::MAX),
            (16384, 16384),
        ] {
            assert_eq!(byte_len(w, h), None);
        }
    }
    #[test]
    fn exact_copy_limit_is_accepted_but_one_column_over_is_not() {
        assert_eq!(byte_len(4096, 4096), Some(64 * 1024 * 1024));
        assert_eq!(byte_len(4097, 4096), None);
        assert_eq!(byte_len(4096, 4097), None);
        assert_eq!(byte_len(1, 1), Some(4));
        assert_eq!(byte_len(1, -1), None);
    }

}
