/// A rectangle in physical screen pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
}

struct Column<W> {
    window: W,
    width: i32,
}

/// A row of full-height columns that can be wider than the screen. The viewport
/// stays put and the row scrolls under it to keep the focused column visible.
///
/// Generic over the window handle so the layout can be tested without Win32.
pub struct Strip<W> {
    viewport: Rect,
    gap: i32,
    columns: Vec<Column<W>>,
    focused: usize,
    /// How far the row has scrolled left under the viewport.
    scroll: i32,
}

impl<W: Copy + PartialEq> Strip<W> {
    pub fn new(viewport: Rect, gap: i32) -> Self {
        Strip {
            viewport,
            gap,
            columns: Vec::new(),
            focused: 0,
            scroll: 0,
        }
    }

    pub fn contains(&self, window: W) -> bool {
        self.position(window).is_some()
    }

    /// Adds a half-width column to the right of the focused one and focuses it.
    pub fn insert(&mut self, window: W) {
        // Two of these plus their gaps fill the viewport exactly.
        let width = (self.viewport.width - self.gap) / 2 - self.gap;
        self.focused = if self.columns.is_empty() {
            0
        } else {
            self.focused + 1
        };
        self.columns.insert(self.focused, Column { window, width });
        self.reveal_focused();
    }

    /// Removes the window's column. Focus stays on the same window, or moves to
    /// a neighbour if the focused column was the one removed.
    pub fn remove(&mut self, window: W) -> bool {
        let Some(index) = self.position(window) else {
            return false;
        };
        self.columns.remove(index);
        if index < self.focused || self.focused == self.columns.len() {
            self.focused = self.focused.saturating_sub(1);
        }
        self.reveal_focused();
        true
    }

    /// Focuses the window's column. Returns `false` if it isn't in the strip.
    pub fn focus(&mut self, window: W) -> bool {
        let Some(index) = self.position(window) else {
            return false;
        };
        self.focused = index;
        self.reveal_focused();
        true
    }

    /// Focuses the neighbouring column and returns its window, or `None` at
    /// the end of the strip.
    pub fn focus_toward(&mut self, direction: Direction) -> Option<W> {
        let target = match direction {
            Direction::Left => self.focused.checked_sub(1)?,
            Direction::Right => self.focused + 1,
        };
        let window = self.columns.get(target)?.window;
        self.focused = target;
        self.reveal_focused();
        Some(window)
    }

    /// Where every window goes on screen. Columns scrolled out of view get
    /// rectangles beyond the viewport's edges.
    pub fn placements(&self) -> impl Iterator<Item = (W, Rect)> {
        let mut left = self.viewport.x + self.gap - self.scroll;
        self.columns.iter().map(move |column| {
            let rect = Rect {
                x: left,
                y: self.viewport.y + self.gap,
                width: column.width,
                height: self.viewport.height - 2 * self.gap,
            };
            left += column.width + self.gap;
            (column.window, rect)
        })
    }

    fn position(&self, window: W) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.window == window)
    }

    /// Scrolls the least distance that puts the focused column, with a gap on
    /// both sides, inside the viewport.
    fn reveal_focused(&mut self) {
        let Some(column) = self.columns.get(self.focused) else {
            self.scroll = 0;
            return;
        };
        let before: i32 = self.columns[..self.focused]
            .iter()
            .map(|c| c.width + self.gap)
            .sum();
        let after = before + column.width + 2 * self.gap;
        // A column wider than the viewport aligns to its left edge.
        self.scroll = self.scroll.max(after - self.viewport.width).min(before);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: Rect = Rect {
        x: 0,
        y: 0,
        width: 1000,
        height: 600,
    };

    fn strip(windows: impl IntoIterator<Item = u32>) -> Strip<u32> {
        let mut strip = Strip::new(VIEWPORT, 10);
        windows.into_iter().for_each(|window| strip.insert(window));
        strip
    }

    fn xs(strip: &Strip<u32>) -> Vec<i32> {
        strip.placements().map(|(_, rect)| rect.x).collect()
    }

    #[test]
    fn two_columns_fill_the_viewport() {
        let placements: Vec<_> = strip([1, 2]).placements().collect();
        let column = |x| Rect {
            x,
            y: 10,
            width: 485,
            height: 580,
        };
        assert_eq!(placements, [(1, column(10)), (2, column(505))]);
    }

    #[test]
    fn scrolls_only_far_enough_to_show_the_focused_column() {
        let mut strip = strip([1, 2, 3]);
        assert_eq!(xs(&strip), [-485, 10, 505]);

        strip.focus_toward(Direction::Left);
        assert_eq!(
            xs(&strip),
            [-485, 10, 505],
            "already on screen, so nothing moves"
        );

        strip.focus_toward(Direction::Left);
        assert_eq!(xs(&strip), [10, 505, 1000]);
    }

    #[test]
    fn focus_stops_at_the_ends() {
        let mut strip = strip([1, 2]);
        assert_eq!(strip.focus_toward(Direction::Right), None);
        assert_eq!(strip.focus_toward(Direction::Left), Some(1));
        assert_eq!(strip.focus_toward(Direction::Left), None);
    }

    #[test]
    fn removing_a_column_keeps_focus_on_the_same_window() {
        let mut strip = strip([1, 2, 3]);
        strip.remove(1);
        assert_eq!(strip.focus_toward(Direction::Left), Some(2));

        strip.remove(2);
        assert_eq!(
            strip.focus_toward(Direction::Left),
            None,
            "3 is the only column left"
        );
    }
}
