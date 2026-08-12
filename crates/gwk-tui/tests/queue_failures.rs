mod common;

use common::dump_frame;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::input::HitMap;
use gwk_tui::queue::{self, QueueTarget};

#[test]
fn queue_surfaces_dead_lettered_and_rejected_mail_with_delivery_context() {
    let state = common::estate::estate_queue_state();
    let rendered = dump_frame(
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        |area, buf, tier, glyphs| {
            let mut hits = HitMap::<QueueTarget>::new();
            queue::render(area, buf, &state, None, tier, glyphs, &mut hits);
        },
    );
    assert!(rendered.contains("dead 16:45"), "{rendered}");
    assert!(
        rendered.contains("3 attempts -- nobody listening"),
        "{rendered}"
    );
    assert!(rendered.contains("rejected 17:06"), "{rendered}");
    assert!(
        rendered.contains("1 attempt -- no reason recorded"),
        "{rendered}"
    );
}
