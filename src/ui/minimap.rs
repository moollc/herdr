//! Solid pane blocks. Drawing and pointer targeting share the same split walk.
use crate::layout::{Node, PaneId};
use ratatui::{
    buffer::Buffer,
    layout::{Direction, Rect},
    style::{Color, Modifier, Style},
};

/// Each pane starts with two rows. Neighbours have a full-cell gutter
/// matching the card background, horizontally and vertically.
pub(crate) fn block_rows(root: &Node) -> u16 {
    minimum_span(root, Direction::Vertical)
}

fn minimum_span(root: &Node, axis: Direction) -> u16 {
    match root {
        Node::Pane(_) => {
            if axis == Direction::Vertical {
                2
            } else {
                1
            }
        }
        Node::Split {
            direction,
            first,
            second,
            ..
        } => {
            let a = minimum_span(first, axis);
            let b = minimum_span(second, axis);
            if *direction == axis {
                a.saturating_add(1).saturating_add(b)
            } else {
                a.max(b)
            }
        }
    }
}

fn blocks(root: &Node, area: Rect, visit: &mut impl FnMut(PaneId, Rect)) {
    if area.is_empty() {
        return;
    }
    let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = root
    else {
        if let Node::Pane(id) = root {
            visit(*id, area);
        }
        return;
    };
    let size = if *direction == Direction::Horizontal {
        area.width
    } else {
        area.height
    };
    // At widths too narrow for a gap, the first visible subtree owns the cell.
    if size < 3 {
        blocks(first, area, visit);
        return;
    }
    let room = size - 1;
    let least = minimum_span(first, *direction).min(room - 1);
    let most = room
        .saturating_sub(minimum_span(second, *direction))
        .max(least);
    let a = ((f32::from(room) * ratio).round() as u16).clamp(least, most);
    let b = room - a;
    let (one, two) = if *direction == Direction::Horizontal {
        (
            Rect::new(area.x, area.y, a, area.height),
            Rect::new(area.x + a + 1, area.y, b, area.height),
        )
    } else {
        (
            Rect::new(area.x, area.y, area.width, a),
            Rect::new(area.x, area.y + a + 1, area.width, b),
        )
    };
    blocks(first, one, visit);
    blocks(second, two, visit);
}

/// Convert faint foregrounds to a dimmer fill too: SGR DIM affects text,
/// but terminals do not dim cell backgrounds. Keep the original hue.
fn fill_color(style: Style) -> Color {
    let color = style.fg.unwrap_or(Color::Reset);
    if style.add_modifier.contains(Modifier::DIM) {
        if let Some((r, g, b)) = super::panes::color_to_rgb(color) {
            return Color::Rgb(r / 2, g / 2, b / 2);
        }
    }
    color
}

pub(crate) fn render_blocks(
    root: &Node,
    area: Rect,
    mut style: impl FnMut(PaneId) -> Style,
    buf: &mut Buffer,
) {
    blocks(root, area, &mut |id, rect| {
        let color = fill_color(style(id));
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    // Background spaces cover the complete cell, including
                    // font leading. Gutters retain the card's own background.
                    cell.set_symbol(" ").set_style(Style::default().bg(color));
                }
            }
        }
    });
}

pub(crate) fn minimap_pane_at(root: &Node, area: Rect, col: u16, row: u16) -> Option<PaneId> {
    let mut found = None;
    blocks(root, area, &mut |id, rect| {
        if rect.contains((col, row).into()) {
            found = Some(id);
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pane(id: u32) -> Box<Node> {
        Box::new(Node::Pane(PaneId::from_raw(id)))
    }
    fn split(axis: Direction, first: Box<Node>, second: Box<Node>) -> Box<Node> {
        Box::new(Node::Split {
            direction: axis,
            ratio: 0.5,
            first,
            second,
        })
    }
    fn solid(buf: &Buffer, x: u16, y: u16) -> &str {
        if buf[(x, y)].bg != Color::Reset {
            "█"
        } else {
            " "
        }
    }
    fn draw(root: &Node, width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, block_rows(root));
        let mut buf = Buffer::empty(area);
        render_blocks(
            root,
            area,
            |id| {
                Style::default().fg(if id.raw() == 0 {
                    Color::Yellow
                } else {
                    Color::Gray
                })
            },
            &mut buf,
        );
        buf
    }
    #[test]
    fn h24_solid_blocks_and_spanning_pane_share_pointer_geometry() {
        let root = split(
            Direction::Horizontal,
            pane(0),
            split(Direction::Vertical, pane(1), pane(2)),
        );
        let buf = draw(&root, 11);
        assert_eq!(buf.area.height, 5);
        let rows: Vec<String> = (0..5)
            .map(|y| (0..11).map(|x| solid(&buf, x, y)).collect())
            .collect();
        assert_eq!(
            rows,
            [
                "█████ █████",
                "█████ █████",
                "█████      ",
                "█████ █████",
                "█████ █████"
            ]
        );
        for y in 0..5 {
            for x in 0..11 {
                let id = minimap_pane_at(&root, buf.area, x, y);
                assert_eq!(id.is_some(), solid(&buf, x, y) != " ");
                if let Some(id) = id {
                    assert_eq!(
                        if buf[(x, y)].bg == Color::Reset {
                            buf[(x, y)].fg
                        } else {
                            buf[(x, y)].bg
                        },
                        if id.raw() == 0 {
                            Color::Yellow
                        } else {
                            Color::Gray
                        }
                    );
                }
            }
        }
        assert_eq!(minimap_pane_at(&root, buf.area, 5, 0), None);
        assert_eq!(minimap_pane_at(&root, buf.area, 11, 0), None);
    }
    #[test]
    fn h24_one_two_and_eight_panes_have_two_rows_per_band() {
        assert_eq!(draw(&pane(0), 21).area.height, 2);
        let two = split(Direction::Horizontal, pane(0), pane(1));
        assert_eq!(draw(&two, 21).area.height, 2);
        let four = |base| {
            split(
                Direction::Horizontal,
                split(Direction::Horizontal, pane(base), pane(base + 1)),
                split(Direction::Horizontal, pane(base + 2), pane(base + 3)),
            )
        };
        let eight = split(Direction::Vertical, four(0), four(4));
        let buf = draw(&eight, 21);
        assert_eq!(buf.area.height, 5);
        let mut ids = std::collections::HashSet::new();
        for y in 0..5 {
            for x in 0..21 {
                if let Some(id) = minimap_pane_at(&eight, buf.area, x, y) {
                    ids.insert(id);
                }
            }
        }
        assert_eq!(ids.len(), 8);
        assert!((0..21).all(|x| solid(&buf, x, 2) == " "));
    }
    #[test]
    fn h25_stacked_background_blocks_have_full_row_gutters_and_offset_hits() {
        let root = split(
            Direction::Vertical,
            pane(0),
            split(Direction::Vertical, pane(1), pane(2)),
        );
        let area = Rect::new(3, 7, 9, block_rows(&root));
        assert_eq!(area.height, 8);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 20));
        render_blocks(
            &root,
            area,
            |id| Style::default().fg(Color::Rgb(40 + id.raw() as u8, 80, 120)),
            &mut buf,
        );
        let mut rects = Vec::new();
        blocks(&root, area, &mut |id, rect| rects.push((id, rect)));
        for pair in rects.windows(2) {
            assert_eq!(pair[1].1.y - pair[0].1.bottom(), 1);
            for x in area.x..area.right() {
                assert_eq!(buf[(x, pair[0].1.bottom())].bg, Color::Reset);
                assert_eq!(minimap_pane_at(&root, area, x, pair[0].1.bottom()), None);
            }
        }
        for (id, rect) in rects {
            for y in rect.y..rect.bottom() {
                let cell = &buf[(area.x, y)];
                assert_eq!(cell.symbol(), " ");
                assert_eq!(cell.bg, Color::Rgb(40 + id.raw() as u8, 80, 120));
                assert_eq!(minimap_pane_at(&root, area, area.x, y), Some(id));
            }
        }
        assert_eq!(minimap_pane_at(&root, area, area.x, area.y - 1), None);
        assert_eq!(minimap_pane_at(&root, area, area.x, area.bottom()), None);
    }

    #[test]
    fn h25_clipped_and_narrow_previews_target_only_painted_cells() {
        let roots = [
            pane(0),
            split(Direction::Horizontal, pane(0), pane(1)),
            split(
                Direction::Horizontal,
                pane(0),
                split(Direction::Vertical, pane(1), pane(2)),
            ),
            split(
                Direction::Vertical,
                split(Direction::Horizontal, pane(0), pane(1)),
                split(
                    Direction::Horizontal,
                    pane(2),
                    split(Direction::Horizontal, pane(3), pane(4)),
                ),
            ),
        ];
        for root in roots {
            for width in 0..24 {
                for height in 0..8 {
                    let area = Rect::new(2, 3, width, height);
                    let mut buf = Buffer::empty(Rect::new(0, 0, 30, 15));
                    render_blocks(
                        &root,
                        area,
                        |id| Style::default().fg(Color::Rgb(id.raw() as u8 + 1, 80, 120)),
                        &mut buf,
                    );
                    for y in 0..15 {
                        for x in 0..30 {
                            let c = &buf[(x, y)];
                            let color = if c.bg != Color::Reset { c.bg } else { c.fg };
                            let hit = minimap_pane_at(&root, area, x, y);
                            assert_eq!(
                                hit.map(|id| Color::Rgb(id.raw() as u8 + 1, 80, 120)),
                                (color != Color::Reset).then_some(color),
                                "at {x},{y} in {area:?}"
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn h25_gutters_keep_the_card_background_and_every_cell_is_a_space() {
        let root = split(
            Direction::Horizontal,
            pane(0),
            split(Direction::Vertical, pane(1), pane(2)),
        );
        let area = Rect::new(0, 0, 11, block_rows(&root));
        for background in [Color::Reset, Color::Rgb(24, 24, 37)] {
            let mut buf = Buffer::filled(
                area,
                ratatui::buffer::Cell::default().set_bg(background).clone(),
            );
            render_blocks(
                &root,
                area,
                |_| Style::default().fg(Color::Yellow),
                &mut buf,
            );
            for y in 0..area.height {
                for x in 0..area.width {
                    let cell = &buf[(x, y)];
                    assert_eq!(cell.symbol(), " ");
                    assert_eq!(cell.modifier, Modifier::empty());
                    assert_eq!(
                        cell.bg,
                        if minimap_pane_at(&root, area, x, y).is_some() {
                            Color::Yellow
                        } else {
                            background
                        }
                    );
                }
            }
        }
    }
}
