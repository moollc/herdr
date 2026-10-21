//! Solid pane blocks. Drawing and pointer targeting share the same split walk.
use crate::layout::{Node, PaneId};
use ratatui::{
    buffer::Buffer,
    layout::{Direction, Rect},
    style::Style,
};

/// Each pane starts with two text rows. A pane beside stacked panes spans
/// their rows and gaps, just as it does in the full layout.
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

pub(crate) fn render_blocks(
    root: &Node,
    area: Rect,
    mut style: impl FnMut(PaneId) -> Style,
    buf: &mut Buffer,
) {
    blocks(root, area, &mut |id, rect| {
        let pane_style = style(id);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol("█").set_style(pane_style);
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
    use ratatui::style::Color;
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
            .map(|y| (0..11).map(|x| buf[(x, y)].symbol()).collect())
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
                assert_eq!(id.is_some(), buf[(x, y)].symbol() == "█");
                if let Some(id) = id {
                    assert_eq!(
                        buf[(x, y)].fg,
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
        assert!((0..21).all(|x| buf[(x, 2)].symbol() == " "));
    }
}
