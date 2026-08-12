//! Shared list-row grammar.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use crate::theme;

/// Paint a right-aligned tail when it fits. If it does not, drop the tail as
/// one column and leave the ruled `+` omission mark at the row's right edge.
pub fn paint_tail(
    buffer: &mut Buffer,
    area: Rect,
    y: u16,
    text_end: u16,
    tail: &str,
    style: Style,
) {
    if area.width == 0 || area.height == 0 || y < area.y || y >= area.y.saturating_add(area.height)
    {
        return;
    }
    let (lines, omitted) = theme::safe_text_lines(tail, area.width as usize, 1);
    let safe = lines.first().map_or("", String::as_str);
    if omitted {
        buffer.set_stringn(area.x.saturating_add(area.width - 1), y, "+", 1, style);
        return;
    }
    let width = UnicodeWidthStr::width(safe);
    let x = area
        .x
        .saturating_add(area.width)
        .saturating_sub(u16::try_from(width).unwrap_or(u16::MAX));
    if x > text_end {
        buffer.set_stringn(x, y, safe, width, style);
    } else {
        buffer.set_stringn(area.x.saturating_add(area.width - 1), y, "+", 1, style);
    }
}
