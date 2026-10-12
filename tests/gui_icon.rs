//! The app icon: a rounded square with a centred accent stream, lit from above.

use streampipe::gui::app_icon;

/// The icon is square, its centre sits on the accent stream, its corners are
/// transparent (rounded), and its edge-centres are opaque.
#[test]
fn the_app_icon_is_a_square_with_a_centred_stream() {
    let icon = app_icon();
    assert_eq!(icon.width, icon.height, "the icon is square");
    assert!(!icon.is_empty(), "the icon has pixels");
    assert_eq!(
        icon.rgba.len(),
        (icon.width * icon.height * 4) as usize,
        "the rgba buffer is exactly width*height*4"
    );
    let size = icon.width as usize;
    let px = |x: usize, y: usize| -> (u8, u8, u8, u8) {
        let i = (y * size + x) * 4;
        (icon.rgba[i], icon.rgba[i + 1], icon.rgba[i + 2], icon.rgba[i + 3])
    };
    // The centre sits on the stream: the accent green.
    let (r, g, b, a) = px(size / 2, size / 2);
    assert!(a > 0, "the centre is opaque, got alpha {a}");
    assert!(g > r && g > b, "the centre is the accent green (green leads), got ({r},{g},{b})");
    // Corners are outside the rounded square: fully transparent.
    assert_eq!(px(0, 0).3, 0, "the top-left corner is transparent");
    assert_eq!(px(size - 1, size - 1).3, 0, "the bottom-right corner is transparent");
    // The top edge-centre is inside the rounded square: opaque.
    assert!(px(size / 2, 1).3 > 0, "the top edge-centre is inside the icon and opaque");
}

/// The icon's ground is lit from above: the top edge-centre is lighter than the bottom.
#[test]
fn the_app_icon_ground_is_lit_from_above() {
    let icon = app_icon();
    let size = icon.width as usize;
    // Sample a column just off the centre so the ground gradient, not the stream,
    // is measured.
    let lum = |y: usize| -> u32 {
        let x = size / 2 + size / 4;
        let i = (y * size + x) * 4;
        (icon.rgba[i] as u32) + (icon.rgba[i + 1] as u32) + (icon.rgba[i + 2] as u32)
    };
    let top = lum(size / 8);
    let bottom = lum(size - size / 8);
    assert!(
        top > bottom,
        "the icon's top ({top}) should be lighter than its bottom ({bottom})"
    );
}
