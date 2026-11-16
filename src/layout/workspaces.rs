use super::{Direction, Placement, Rect, Step, Strip};

/// A stack of strips with one of them on screen.
///
/// The bottom strip is always empty, so there is always somewhere new to go,
/// and any other strip disappears once it's empty and left. Workspaces are
/// never created or deleted by hand.
pub struct Workspaces<W> {
    viewport: Rect,
    gap: i32,
    strips: Vec<Strip<W>>,
    /// The strip on screen.
    active: usize,
}

impl<W: Copy + PartialEq> Workspaces<W> {
    pub fn new(viewport: Rect, gap: i32) -> Self {
        Workspaces {
            viewport,
            gap,
            strips: vec![Strip::new(viewport, gap)],
            active: 0,
        }
    }

    pub fn contains(&self, window: W) -> bool {
        self.holder(window).is_some()
    }

    /// The focused window of the workspace on screen, if it has any windows.
    pub fn focused(&self) -> Option<W> {
        self.strips[self.active].focused()
    }

    /// Adds the window to the workspace on screen, to the right of its
    /// focused column, and focuses it.
    pub fn insert(&mut self, window: W) {
        self.strips[self.active].insert(window);
        self.tidy();
    }

    /// Returns `false` if the window isn't in any workspace.
    pub fn remove(&mut self, window: W) -> bool {
        let Some(index) = self.holder(window) else {
            return false;
        };
        self.strips[index].remove(window);
        self.tidy();
        true
    }

    /// Focuses the window, switching to its workspace. Returns `false` if it
    /// isn't in any workspace.
    pub fn focus(&mut self, window: W) -> bool {
        let Some(index) = self.holder(window) else {
            return false;
        };
        self.active = index;
        self.strips[index].focus(window);
        self.tidy();
        true
    }

    /// Moves focus one column or one workspace over, wrapping round at the
    /// ends. Returns `false` when there is nowhere else to go.
    pub fn focus_toward(&mut self, direction: Direction) -> bool {
        match direction {
            Direction::Left => self.strips[self.active].focus_toward(Step::Back),
            Direction::Right => self.strips[self.active].focus_toward(Step::Forward),
            Direction::Up => self.switch(Step::Back),
            Direction::Down => self.switch(Step::Forward),
        }
    }

    /// Moves the focused column past its neighbour, or into the next
    /// workspace, and follows it. Returns `false` when nothing moved.
    pub fn move_toward(&mut self, direction: Direction) -> bool {
        match direction {
            Direction::Left => self.strips[self.active].move_toward(Step::Back),
            Direction::Right => self.strips[self.active].move_toward(Step::Forward),
            Direction::Up => self.send(Step::Back),
            Direction::Down => self.send(Step::Forward),
        }
    }

    /// See `Strip::cycle_width`.
    pub fn cycle_width(&mut self) -> bool {
        self.strips[self.active].cycle_width()
    }

    /// Where every window goes. Windows on other workspaces keep their place
    /// in their own strip and aren't visible, so switching moves nothing.
    pub fn placements(&self) -> impl Iterator<Item = Placement<W>> {
        let active = self.active;
        self.strips
            .iter()
            .enumerate()
            .flat_map(move |(index, strip)| {
                strip.placements().map(move |placement| Placement {
                    visible: placement.visible && index == active,
                    ..placement
                })
            })
    }

    fn holder(&self, window: W) -> Option<usize> {
        self.strips.iter().position(|strip| strip.contains(window))
    }

    fn switch(&mut self, step: Step) -> bool {
        let Some(target) = step.from(self.active, self.strips.len()) else {
            return false;
        };
        self.active = target;
        self.tidy();
        true
    }

    fn send(&mut self, step: Step) -> bool {
        let Some(target) = step.from(self.active, self.strips.len()) else {
            return false;
        };
        let Some(column) = self.strips[self.active].take_focused() else {
            return false;
        };
        self.strips[target].insert_column(column);
        self.active = target;
        self.tidy();
        true
    }

    /// Drops the empty strips nobody is looking at and makes sure the bottom
    /// one is empty.
    fn tidy(&mut self) {
        let active = self.active;
        let dropped_above = self.strips[..active]
            .iter()
            .filter(|strip| strip.is_empty())
            .count();

        let mut index = 0;
        self.strips.retain(|strip| {
            let keep = index == active || !strip.is_empty();
            index += 1;
            keep
        });
        self.active -= dropped_above;

        if self.strips.last().is_none_or(|strip| !strip.is_empty()) {
            self.strips.push(Strip::new(self.viewport, self.gap));
        }
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

    fn workspaces(windows: impl IntoIterator<Item = u32>) -> Workspaces<u32> {
        let mut workspaces = Workspaces::new(VIEWPORT, 10);
        windows
            .into_iter()
            .for_each(|window| workspaces.insert(window));
        workspaces
    }

    /// The windows in each workspace, top to bottom.
    fn rows(workspaces: &Workspaces<u32>) -> Vec<Vec<u32>> {
        let windows = |strip: &Strip<u32>| strip.placements().map(|p| p.window).collect();
        workspaces.strips.iter().map(windows).collect()
    }

    fn visible(workspaces: &Workspaces<u32>) -> Vec<u32> {
        workspaces
            .placements()
            .filter(|p| p.visible)
            .map(|p| p.window)
            .collect()
    }

    #[test]
    fn sending_a_column_down_makes_a_workspace_and_follows_it() {
        let mut workspaces = workspaces([1, 2]);
        assert_eq!(rows(&workspaces), [vec![1, 2], vec![]]);

        workspaces.move_toward(Direction::Down);
        assert_eq!(rows(&workspaces), [vec![1], vec![2], vec![]]);
        assert_eq!(visible(&workspaces), [2]);
    }

    #[test]
    fn an_emptied_workspace_disappears_once_left() {
        let mut workspaces = workspaces([1, 2, 3]);
        workspaces.move_toward(Direction::Down);
        workspaces.focus(2);
        workspaces.move_toward(Direction::Down);
        workspaces.focus_toward(Direction::Up);
        assert_eq!(rows(&workspaces), [vec![1], vec![3, 2], vec![]]);

        workspaces.focus(1);
        workspaces.remove(1);
        assert_eq!(
            rows(&workspaces),
            [vec![], vec![3, 2], vec![]],
            "still on it, so it stays"
        );

        workspaces.focus_toward(Direction::Down);
        assert_eq!(rows(&workspaces), [vec![3, 2], vec![]]);
        assert_eq!(visible(&workspaces), [3, 2]);
    }

    #[test]
    fn focusing_a_window_switches_to_its_workspace() {
        let mut workspaces = workspaces([1, 2]);
        workspaces.move_toward(Direction::Down);

        assert!(workspaces.focus(1));
        assert_eq!(visible(&workspaces), [1]);
    }

    #[test]
    fn workspace_focus_wraps_through_the_empty_one() {
        let mut workspaces = workspaces([1]);
        workspaces.focus_toward(Direction::Down);
        assert_eq!(
            workspaces.focused(),
            None,
            "the empty workspace at the bottom"
        );

        workspaces.focus_toward(Direction::Down);
        assert_eq!(workspaces.focused(), Some(1));
    }
}
