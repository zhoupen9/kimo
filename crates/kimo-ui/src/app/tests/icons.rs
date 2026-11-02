//! The embedded folder icons: every kind has one white-on-transparent PNG.

#[test]
fn every_kind_has_a_white_icon() {
    for (kind, open, png) in crate::app::icons::FOLDER_ICONS {
        let decoded = image::load_from_memory(png)
            .unwrap_or_else(|error| panic!("{kind:?} open={open}: {error}"))
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (48, 48), "{kind:?} open={open}");
        // White (egui tints by multiplying) with transparency around it.
        assert!(
            decoded
                .pixels()
                .any(|pixel| pixel.0 == [255, 255, 255, 255]),
            "{kind:?} open={open} has no white pixels"
        );
        assert!(
            decoded.pixels().any(|pixel| pixel.0[3] == 0),
            "{kind:?} open={open} has no transparent pixels"
        );
    }
}
