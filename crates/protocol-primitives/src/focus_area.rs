//! Tap-to-AF area packing for `0x9026 LockS1Lock`.
//!
//! A screen tap maps to a cell of the camera's AF grid (dimensions are manifest
//! data — the GFX100 II stills grid is 9×6), packed into the u32 param:
//!
//! ```text
//! bits 24–31  aspect width numerator  (u8)
//! bits 16–23  aspect height numerator (u8)
//! bits  8–15  column (u8, 1-indexed)
//! bits  0–7   row    (u8, 1-indexed)
//! ```
//!
//! The aspect ratio comes from the prior `0xD17C` S1-lock state (its high 16
//! bits), defaulting to 4:3. Byte-exact against wire values `0x04030504` (cell
//! 5,4) and `0x04030606` (cell 6,6) — see `FUJI_PTP_PROP_REFERENCE.md` §5. This
//! replaces client application's `FujiFocusArea` so the app carries no focus math (#135).

/// Extract the aspect (width, height) numerators from a prior `0xD17C` lock
/// value (high 16 bits). `None` if absent or either numerator is zero.
fn aspect_from_lock(prior_lock_state: Option<u32>) -> Option<(u8, u8)> {
    let raw = prior_lock_state?;
    let w = ((raw >> 24) & 0xff) as u8;
    let h = ((raw >> 16) & 0xff) as u8;
    (w > 0 && h > 0).then_some((w, h))
}

/// Pack a normalized tap `(x, y)` (each in `0.0..=1.0`; non-finite → 0.5, the
/// center) into the `0x9026` AF-area u32 for a `columns`×`rows` grid. The cell is
/// 1-indexed and clamped to the grid; grid dimensions are clamped to `1..=255`
/// (the u8 field). Aspect comes from `prior_lock_state` (`0xD17C`), default 4:3.
pub fn pack_af_area(x: f64, y: f64, columns: u32, rows: u32, prior_lock_state: Option<u32>) -> u32 {
    let columns = columns.clamp(1, 255);
    let rows = rows.clamp(1, 255);
    let x = if x.is_finite() {
        x.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let y = if y.is_finite() {
        y.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let column = (((x * columns as f64).floor() as u32) + 1).min(columns);
    let row = (((y * rows as f64).floor() as u32) + 1).min(rows);
    let (aw, ah) = aspect_from_lock(prior_lock_state).unwrap_or((4, 3));
    ((aw as u32) << 24) | ((ah as u32) << 16) | (column << 8) | row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_the_wire_confirmed_cells() {
        // Cell (5,4) on the 9×6 grid, default 4:3 aspect -> 0x04030504.
        assert_eq!(pack_af_area(0.45, 0.5, 9, 6, None), 0x0403_0504);
        // Cell (6,6): bottom-right region.
        assert_eq!(pack_af_area(0.6, 0.9, 9, 6, None), 0x0403_0606);
    }

    #[test]
    fn corners_and_center_clamp_into_the_grid() {
        assert_eq!(pack_af_area(0.0, 0.0, 9, 6, None), 0x0403_0101); // top-left
        assert_eq!(pack_af_area(1.0, 1.0, 9, 6, None), 0x0403_0906); // bottom-right, clamped
        assert_eq!(pack_af_area(f64::NAN, f64::NAN, 9, 6, None), 0x0403_0504); // center (0.5,0.5)
    }

    #[test]
    fn aspect_comes_from_the_prior_lock_else_defaults_to_4_3() {
        // A prior 16:9 lock carries its aspect into the next pack.
        let packed = pack_af_area(0.0, 0.0, 9, 6, Some(0x1009_0101));
        assert_eq!(packed >> 16, 0x1009);
        // A zero-aspect prior is ignored -> default 4:3.
        assert_eq!(
            pack_af_area(0.0, 0.0, 9, 6, Some(0x0000_0101)) >> 16,
            0x0403
        );
    }

    #[test]
    fn grid_dimensions_clamp_to_the_u8_field() {
        // 0 columns clamps to 1; a huge grid clamps to 255 (never overflows the field).
        assert_eq!(pack_af_area(0.5, 0.5, 0, 0, None) & 0xffff, 0x0101);
        let packed = pack_af_area(1.0, 1.0, 1000, 1000, None);
        assert_eq!(packed & 0xff, 255);
        assert_eq!((packed >> 8) & 0xff, 255);
    }
}
