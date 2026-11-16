mod workspaces;

pub use workspaces::Workspaces;

/// A rectangle in physical screen pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// The rectangle with `margin` taken off every side.
    pub fn inset(self, margin: i32) -> Rect {
        Rect {
            x: self.x + margin,
            y: self.y + margin,
            width: self.width - 2 * margin,
            height: self.height - 2 * margin,
        }
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        (self.x..self.x + self.width).contains(&x) && (self.y..self.y + self.height).contains(&y)
    }
}

/// Left and right act on columns, up and down on workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// How a window is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A column in a strip.
    Tiled,
    /// Left wherever it is, above the strip of one workspace.
    Floating,
    /// Floating on every workspace.
    Sticky,
    /// Sticky, and above every other window.
    OnTop,
}

impl Mode {
    /// The mode a window in this one goes to when the key for `target` is
    /// pressed. A tiled window goes into the target. The floating key tiles
    /// any other window, and the rest turn themselves back off, which leaves
    /// the window floating.
    pub fn toggled(self, target: Mode) -> Mode {
        match (self, target) {
            (Mode::Tiled, _) => target,
            (_, Mode::Floating) => Mode::Tiled,
            _ if self == target => Mode::Floating,
            _ => target,
        }
    }
}

/// Where a window goes, and whether it should be drawn there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement<W> {
    pub window: W,
    /// The workspace it belongs to, counted from zero at the top. A window
    /// that shows on every workspace reports the one on screen.
    pub workspace: usize,
    /// `None` for a floating window, which stays wherever it was put.
    pub rect: Option<Rect>,
    pub visible: bool,
}

/// One place along a row of columns or a stack of workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Back,
    Forward,
}

impl Step {
    /// The index one step away from `index` among `count` items, wrapping
    /// round at the ends. `None` when there is nowhere else to go.
    fn from(self, index: usize, count: usize, wrap: bool) -> Option<usize> {
        if !wrap {
            return match self {
                Step::Back => index.checked_sub(1),
                Step::Forward => (index + 1 < count).then_some(index + 1),
            };
        }
        let step = match self {
            Step::Back => count.checked_sub(1)?,
            Step::Forward => 1,
        };
        (count > 1).then(|| (index + step) % count)
    }
}

/// A column width as a fraction of the viewport: numerator, then denominator.
pub type Fraction = (i32, i32);

/// The widths a column cycles through, narrowest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub widths: Vec<Fraction>,
    pub default_width: Fraction,
    pub min_width_percent: i32,
    pub wrap_focus: bool,
    pub wrap_move: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            widths: vec![(1, 3), (1, 2), (2, 3)],
            default_width: (1, 2),
            min_width_percent: 10,
            wrap_focus: true,
            wrap_move: true,
        }
    }
}

/// How wide a column is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Width {
    pub pixels: i32,
    /// Fills the viewport for now, whatever `pixels` says, and returns to it.
    pub full: bool,
}

struct Column<W> {
    window: W,
    width: Width,
}

/// A row of full-height columns that can be wider than the screen. The viewport
/// stays put and the row scrolls under it to keep the focused column visible.
///
/// Generic over the window handle so the layout can be tested without Win32.
struct Strip<W> {
    viewport: Rect,
    gap: i32,
    options: Options,
    columns: Vec<Column<W>>,
    focused: usize,
    /// How far the row has scrolled left under the viewport.
    scroll: i32,
}

impl<W: Copy + PartialEq> Strip<W> {
    fn new(viewport: Rect, gap: i32) -> Self {
        Strip {
            viewport,
            gap,
            options: Options::default(),
            columns: Vec::new(),
            focused: 0,
            scroll: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    fn contains(&self, window: W) -> bool {
        self.position(window).is_some()
    }

    fn focused(&self) -> Option<W> {
        self.columns.get(self.focused).map(|column| column.window)
    }

    /// The width of the window's column, or `None` if it isn't in the strip.
    fn width(&self, window: W) -> Option<Width> {
        let index = self.position(window)?;
        Some(self.columns[index].width)
    }

    /// Adds a column for the window at the given width, kept within what
    /// fits, or at the default one. See `insert_column`.
    fn insert(&mut self, window: W, width: Option<Width>) {
        let (narrowest, widest) = self.limits();
        let width = match width {
            Some(width) => Width {
                pixels: width.pixels.clamp(narrowest, widest),
                ..width
            },
            None => Width {
                pixels: self
                    .preset(self.options.default_width)
                    .clamp(narrowest, widest),
                full: false,
            },
        };
        self.insert_column(Column { window, width });
    }

    /// Adds the column to the right of the focused one and focuses it.
    fn insert_column(&mut self, column: Column<W>) {
        self.focused = if self.columns.is_empty() {
            0
        } else {
            self.focused + 1
        };
        self.columns.insert(self.focused, column);
        self.reveal_focused();
    }

    /// Removes the window's column. Returns `false` if it isn't in the strip.
    fn remove(&mut self, window: W) -> bool {
        self.position(window)
            .map(|index| self.take(index))
            .is_some()
    }

    fn take_focused(&mut self) -> Option<Column<W>> {
        (!self.is_empty()).then(|| self.take(self.focused))
    }

    /// Focuses the window's column. Returns `false` if it isn't in the strip.
    fn focus(&mut self, window: W) -> bool {
        let Some(index) = self.position(window) else {
            return false;
        };
        self.focused = index;
        self.reveal_focused();
        true
    }

    /// Focuses the neighbouring column, wrapping round at the ends. Returns
    /// `false` when there is no other column.
    fn focus_toward(&mut self, step: Step) -> bool {
        let Some(target) = step.from(self.focused, self.columns.len(), self.options.wrap_focus)
        else {
            return false;
        };
        self.focused = target;
        self.reveal_focused();
        true
    }

    /// Moves the focused column past its neighbour, keeping it focused. At an
    /// end it wraps round to the other one. Returns `false` when there is no
    /// other column.
    fn move_toward(&mut self, step: Step) -> bool {
        let Some(target) = step.from(self.focused, self.columns.len(), self.options.wrap_move)
        else {
            return false;
        };
        let column = self.columns.remove(self.focused);
        self.columns.insert(target, column);
        self.focused = target;
        self.reveal_focused();
        true
    }

    /// Steps the focused column to the next wider preset, wrapping round to
    /// the narrowest. Returns `false` if the strip is empty.
    fn cycle_width(&mut self) -> bool {
        let (min, max) = self.limits();
        let presets: Vec<_> = self
            .options
            .widths
            .iter()
            .map(|fraction| self.preset(*fraction).clamp(min, max))
            .collect();
        self.rewidth(|pixels| {
            presets
                .iter()
                .copied()
                .find(|preset| *preset > pixels)
                .unwrap_or(presets[0])
        })
    }

    /// Widens the focused column by a percentage of the viewport, or narrows
    /// it when negative, between a tenth of the viewport and all of it.
    /// Returns `false` if the width didn't change.
    fn resize(&mut self, percent: i32) -> bool {
        let (narrowest, widest) = self.limits();
        let step = self.viewport.width * percent / 100;
        self.rewidth(|pixels| (pixels + step).clamp(narrowest, widest))
    }

    /// Makes the focused column fill the viewport, or puts it back to the
    /// width it had. Returns `false` if the strip is empty.
    fn toggle_full(&mut self) -> bool {
        let Some(column) = self.columns.get_mut(self.focused) else {
            return false;
        };
        column.width.full = !column.width.full;
        self.reveal_focused();
        true
    }

    /// Where every window goes. Columns scrolled out of view get rectangles
    /// beyond the viewport's edges and aren't visible. A strip doesn't know
    /// its place in the stack, so `workspace` is left for `Workspaces` to set.
    fn placements(&self) -> impl Iterator<Item = Placement<W>> {
        let (start, end) = (self.viewport.x, self.viewport.x + self.viewport.width);
        let mut left = start + self.gap - self.scroll;

        self.columns.iter().map(move |column| {
            let rect = Rect {
                x: left,
                y: self.viewport.y + self.gap,
                width: self.span(column),
                height: self.viewport.height - 2 * self.gap,
            };
            left += rect.width + self.gap;
            let visible = rect.x < end && rect.x + rect.width > start;
            Placement {
                window: column.window,
                workspace: 0,
                rect: Some(rect),
                visible,
            }
        })
    }

    fn position(&self, window: W) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.window == window)
    }

    /// Removes the column at `index`. Focus stays on the same window, or moves
    /// to a neighbour if the focused column was the one removed.
    fn take(&mut self, index: usize) -> Column<W> {
        let column = self.columns.remove(index);
        if index < self.focused || self.focused == self.columns.len() {
            self.focused = self.focused.saturating_sub(1);
        }
        self.reveal_focused();
        column
    }

    /// Sized so that columns whose fractions add up to one fill the viewport
    /// exactly, gaps included.
    fn preset(&self, (numerator, denominator): Fraction) -> i32 {
        ((i64::from(self.viewport.width - self.gap) * i64::from(numerator))
            / i64::from(denominator)) as i32
            - self.gap
    }

    /// The narrowest and widest a column can be: a tenth of the viewport, and
    /// all of it with a gap each side.
    fn limits(&self) -> (i32, i32) {
        (
            (self.viewport.width * self.options.min_width_percent / 100).max(1),
            self.viewport.width - 2 * self.gap,
        )
    }

    fn configure(&mut self, viewport: Rect, gap: i32, options: Options) {
        self.viewport = viewport;
        self.gap = gap;
        self.options = options;
        let (min, max) = self.limits();
        for column in &mut self.columns {
            column.width.pixels = column.width.pixels.clamp(min, max);
        }
        self.reveal_focused();
    }

    /// How wide the column is drawn.
    fn span(&self, column: &Column<W>) -> i32 {
        if column.width.full {
            self.limits().1
        } else {
            column.width.pixels
        }
    }

    /// Sets the focused column's width from the one it's drawn at, so a full
    /// column stops being full. Returns `false` if nothing changed.
    fn rewidth(&mut self, change: impl FnOnce(i32) -> i32) -> bool {
        let Some(column) = self.columns.get(self.focused) else {
            return false;
        };
        let width = Width {
            pixels: change(self.span(column)),
            full: false,
        };
        let changed = width != column.width;
        self.columns[self.focused].width = width;
        self.reveal_focused();
        changed
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
            .map(|earlier| self.span(earlier) + self.gap)
            .sum();
        let after = before + self.span(column) + 2 * self.gap;
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
        windows
            .into_iter()
            .for_each(|window| strip.insert(window, None));
        strip
    }

    #[test]
    fn a_column_opens_at_a_remembered_width_that_fits() {
        let mut strip = strip([]);
        let remembered = |pixels| Width {
            pixels,
            full: false,
        };
        strip.insert(1, Some(remembered(700)));
        strip.insert(2, Some(remembered(5000)));
        assert_eq!(widths(&strip), [700, 980]);
    }

    fn rects(strip: &Strip<u32>) -> Vec<Rect> {
        strip
            .placements()
            .filter_map(|placement| placement.rect)
            .collect()
    }

    fn xs(strip: &Strip<u32>) -> Vec<i32> {
        rects(strip).into_iter().map(|rect| rect.x).collect()
    }

    fn widths(strip: &Strip<u32>) -> Vec<i32> {
        rects(strip).into_iter().map(|rect| rect.width).collect()
    }

    fn order(strip: &Strip<u32>) -> Vec<u32> {
        strip
            .placements()
            .map(|placement| placement.window)
            .collect()
    }

    #[test]
    fn two_columns_fill_the_viewport() {
        let column = |x| Rect {
            x,
            y: 10,
            width: 485,
            height: 580,
        };
        assert_eq!(rects(&strip([1, 2])), [column(10), column(505)]);
    }

    #[test]
    fn scrolls_only_far_enough_to_show_the_focused_column() {
        let mut strip = strip([1, 2, 3]);
        assert_eq!(xs(&strip), [-485, 10, 505]);

        strip.focus_toward(Step::Back);
        assert_eq!(
            xs(&strip),
            [-485, 10, 505],
            "already on screen, so nothing moves"
        );

        strip.focus_toward(Step::Back);
        assert_eq!(xs(&strip), [10, 505, 1000]);
    }

    #[test]
    fn only_columns_inside_the_viewport_are_visible() {
        let visible: Vec<_> = strip([1, 2, 3])
            .placements()
            .map(|placement| placement.visible)
            .collect();
        assert_eq!(visible, [false, true, true]);
    }

    #[test]
    fn focus_and_move_wrap_round_the_ends() {
        let mut strip = strip([1, 2, 3]);
        strip.focus_toward(Step::Forward);
        assert_eq!(strip.focused(), Some(1));
        strip.focus_toward(Step::Back);
        assert_eq!(strip.focused(), Some(3));

        strip.move_toward(Step::Forward);
        assert_eq!(order(&strip), [3, 1, 2]);
        strip.move_toward(Step::Back);
        assert_eq!(order(&strip), [1, 2, 3]);
        assert_eq!(strip.focused(), Some(3), "focus stays on the moved column");
    }

    #[test]
    fn a_lone_column_has_nowhere_to_go() {
        let mut strip = strip([1]);
        assert!(!strip.focus_toward(Step::Forward));
        assert!(!strip.move_toward(Step::Back));
    }

    #[test]
    fn removing_a_column_keeps_focus_on_the_same_window() {
        let mut strip = strip([1, 2, 3]);
        strip.remove(1);
        assert_eq!(strip.focused(), Some(3));

        strip.remove(3);
        assert_eq!(strip.focused(), Some(2), "focus falls to the neighbour");
    }

    #[test]
    fn width_cycles_through_the_presets_and_wraps() {
        let mut strip = strip([1]);

        assert_eq!(widths(&strip), [485]);
        strip.cycle_width();
        assert_eq!(widths(&strip), [650]);
        strip.cycle_width();
        assert_eq!(widths(&strip), [320]);
        assert_eq!(
            320 * 3 + 10 * 4,
            VIEWPORT.width,
            "three thirds fill the viewport"
        );
    }

    #[test]
    fn a_full_column_returns_to_the_width_it_had() {
        let mut strip = strip([1]);
        strip.resize(10);

        strip.toggle_full();
        assert_eq!(widths(&strip), [980]);
        strip.toggle_full();
        assert_eq!(widths(&strip), [585]);

        strip.toggle_full();
        strip.resize(-10);
        assert_eq!(widths(&strip), [880], "resizing starts from the full width");
    }

    #[test]
    fn mode_keys_switch_a_mode_on_and_back_off() {
        use Mode::{Floating, OnTop, Sticky, Tiled};

        assert_eq!(Tiled.toggled(Floating), Floating);
        assert_eq!(Floating.toggled(Floating), Tiled);
        assert_eq!(OnTop.toggled(Floating), Tiled, "the floating key tiles");

        assert_eq!(Tiled.toggled(OnTop), OnTop, "floats it on the way");
        assert_eq!(OnTop.toggled(OnTop), Floating);
        assert_eq!(
            OnTop.toggled(Sticky),
            Sticky,
            "stays pinned, no longer on top"
        );
    }

    #[test]
    fn resizing_steps_by_a_share_of_the_viewport_within_limits() {
        let mut strip = strip([1]);

        strip.resize(10);
        assert_eq!(widths(&strip), [585]);
        strip.cycle_width();
        assert_eq!(widths(&strip), [650], "presets pick up from any width");

        while strip.resize(10) {}
        assert_eq!(widths(&strip), [980], "the viewport less a gap each side");

        while strip.resize(-10) {}
        assert_eq!(widths(&strip), [100], "a tenth of the viewport");
    }
}
