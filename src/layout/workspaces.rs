use super::{Direction, Mode, Options, Placement, Rect, Step, Strip, Width};

/// A strip of tiled columns and the floating windows that sit above it.
struct Workspace<W> {
    strip: Strip<W>,
    floating: Vec<W>,
    /// Named workspaces come from the config and stay when empty.
    name: Option<String>,
}

impl<W: Copy + PartialEq> Workspace<W> {
    fn new(viewport: Rect, gap: i32) -> Self {
        Workspace {
            strip: Strip::new(viewport, gap),
            floating: Vec::new(),
            name: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.strip.is_empty() && self.floating.is_empty()
    }
}

/// A window and where `Workspaces::arrange` puts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arranged<W> {
    pub window: W,
    pub mode: Mode,
    /// For a tiled window, `None` for the default.
    pub width: Option<Width>,
    /// Counted from the top. Past the last workspace with windows means a
    /// new one at the bottom, one for each distinct index, in order.
    pub workspace: usize,
    /// For a tiled window, the column it goes right of. Without one it goes
    /// right of the window arranged before it in that workspace, or else
    /// leftmost.
    pub after: Option<W>,
}

/// Where a window is kept. The indices are workspaces in the stack.
#[derive(Clone, Copy)]
enum Home {
    Tiled(usize),
    Floating(usize),
    Sticky,
    OnTop,
}

/// A stack of workspaces with one of them on screen.
///
/// Named workspaces sit at the top in the config's order and always stay.
/// Below them, the bottom workspace is always empty, so there is always
/// somewhere new to go, and any other one disappears once it's empty and
/// left. Workspaces are never created or deleted by hand.
///
/// Floating windows are tracked but never positioned: they stay wherever they
/// are and only show or hide with their workspace.
pub struct Workspaces<W> {
    viewport: Rect,
    gap: i32,
    options: Options,
    stack: Vec<Workspace<W>>,
    /// The workspace on screen.
    active: usize,
    /// Floating windows that show on every workspace.
    sticky: Vec<W>,
    /// Sticky windows that are also kept above every other window.
    on_top: Vec<W>,
    /// The floating window that has focus, when one does. It shows on every
    /// workspace or belongs to the one on screen.
    floating_focus: Option<W>,
}

impl<W: Copy + PartialEq> Workspaces<W> {
    pub fn new(viewport: Rect, gap: i32) -> Self {
        Workspaces {
            viewport,
            gap,
            options: Options::default(),
            stack: vec![Workspace::new(viewport, gap)],
            active: 0,
            sticky: Vec::new(),
            on_top: Vec::new(),
            floating_focus: None,
        }
    }

    pub fn contains(&self, window: W) -> bool {
        self.home(window).is_some()
    }

    /// How the window is kept, or `None` if it isn't in any workspace.
    pub fn mode(&self, window: W) -> Option<Mode> {
        self.home(window).map(|home| match home {
            Home::Tiled(_) => Mode::Tiled,
            Home::Floating(_) => Mode::Floating,
            Home::Sticky => Mode::Sticky,
            Home::OnTop => Mode::OnTop,
        })
    }

    /// The width of the window's column, or `None` if it isn't tiled.
    pub fn width(&self, window: W) -> Option<Width> {
        match self.home(window)? {
            Home::Tiled(index) => self.stack[index].strip.width(window),
            _ => None,
        }
    }

    /// The workspace on screen, counted from zero at the top, and how many
    /// there are with the empty one at the bottom.
    pub fn active(&self) -> (usize, usize) {
        (self.active, self.stack.len())
    }

    /// The part of the screen that tiled windows fill.
    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    /// Update settings in place, preserving workspaces, windows, focus and widths.
    /// Widths are clamped only when the new viewport/limits require it.
    pub fn configure(&mut self, viewport: Rect, gap: i32, options: Options) {
        self.viewport = viewport;
        self.gap = gap;
        self.options = options;
        for workspace in &mut self.stack {
            workspace
                .strip
                .configure(viewport, gap, self.options.clone());
        }
    }

    /// Puts the named workspaces at the top in this order, making the missing
    /// ones empty. A workspace whose name is gone keeps its windows unnamed.
    pub fn name(&mut self, names: &[String]) {
        let mut old: Vec<_> = self.stack.drain(..).enumerate().collect();
        let mut stack = Vec::with_capacity(old.len() + names.len());
        let mut active = 0;
        for name in names {
            let found = old
                .iter()
                .position(|(_, workspace)| workspace.name.as_ref() == Some(name));
            let (index, workspace) = match found {
                Some(found) => old.remove(found),
                None => (usize::MAX, self.blank(Some(name.clone()))),
            };
            if index == self.active {
                active = stack.len();
            }
            stack.push(workspace);
        }
        for (index, mut workspace) in old {
            workspace.name = None;
            if index == self.active {
                active = stack.len();
            }
            stack.push(workspace);
        }
        self.stack = stack;
        self.active = active;
        self.tidy();
    }

    /// The workspace with this name, counted from the top.
    pub fn named(&self, name: &str) -> Option<usize> {
        self.stack
            .iter()
            .position(|workspace| workspace.name.as_deref() == Some(name))
    }

    /// Each workspace's name, top to bottom.
    pub fn names(&self) -> impl Iterator<Item = Option<&str>> {
        self.stack.iter().map(|workspace| workspace.name.as_deref())
    }

    /// The workspace the window belongs to, unless it's on every one.
    pub fn workspace_of(&self, window: W) -> Option<usize> {
        match self.home(window)? {
            Home::Tiled(index) | Home::Floating(index) => Some(index),
            Home::Sticky | Home::OnTop => None,
        }
    }

    /// The focused column of the workspace at `index`.
    pub fn column_focus(&self, index: usize) -> Option<W> {
        self.stack.get(index)?.strip.focused()
    }

    /// Moves each window to its spot, adding the ones not in the layout yet,
    /// without changing which workspace is on screen. Focus only moves off a
    /// window that leaves the workspace on screen.
    pub fn arrange(&mut self, items: &[Arranged<W>]) {
        let focused = self.focused();
        // Indices are read against the stack as it is now; moving windows
        // out can only empty workspaces, which go once everything is placed.
        let bottom = self.stack.len() - 1;
        let mut beyond: Vec<usize> = items
            .iter()
            .map(|item| item.workspace)
            .filter(|index| *index >= bottom)
            .collect();
        beyond.sort_unstable();
        beyond.dedup();
        let target = |index: usize| match beyond.binary_search(&index) {
            Ok(rank) => bottom + rank,
            Err(_) => index,
        };
        while self.stack.len() < bottom + beyond.len() {
            self.stack.push(self.blank(None));
        }
        for item in items {
            self.detach(item.window);
        }
        let mut last: Vec<(usize, W)> = Vec::new();
        for item in items {
            let index = target(item.workspace);
            match item.mode {
                Mode::Tiled => {
                    let previous = last
                        .iter()
                        .rev()
                        .find(|(placed, _)| *placed == index)
                        .map(|(_, window)| *window);
                    let strip = &mut self.stack[index].strip;
                    let column = item
                        .after
                        .or(previous)
                        .and_then(|after| strip.position(after))
                        .map_or(0, |position| position + 1);
                    strip.put(column, item.window, item.width);
                    last.push((index, item.window));
                }
                Mode::Floating => self.stack[index].floating.push(item.window),
                Mode::Sticky => self.sticky.push(item.window),
                Mode::OnTop => self.on_top.push(item.window),
            }
        }
        self.tidy();
        if let Some(window) = focused
            && self.contains(window)
            && self
                .workspace_of(window)
                .is_none_or(|index| index == self.active)
        {
            self.focus(window);
        }
    }

    pub fn default_width(&self) -> i32 {
        let strip = &self.stack[self.active].strip;
        let (min, max) = strip.limits();
        strip.preset(self.options.default_width).clamp(min, max)
    }

    /// The focused window: a floating one, or else the focused column of the
    /// workspace on screen.
    pub fn focused(&self) -> Option<W> {
        self.floating_focus
            .or_else(|| self.stack[self.active].strip.focused())
    }

    /// Adds the window to the workspace on screen and focuses it. A tiled one
    /// goes to the right of the focused column, at `width` or the default.
    pub fn insert(&mut self, window: W, mode: Mode, width: Option<Width>) {
        let workspace = &mut self.stack[self.active];
        match mode {
            Mode::Tiled => workspace.strip.insert(window, width),
            Mode::Floating => workspace.floating.push(window),
            Mode::Sticky => self.sticky.push(window),
            Mode::OnTop => self.on_top.push(window),
        }
        self.floating_focus = (mode != Mode::Tiled).then_some(window);
        self.tidy();
    }

    /// Returns `false` if the window isn't in any workspace.
    pub fn remove(&mut self, window: W) -> bool {
        let removed = self.detach(window);
        self.tidy();
        removed
    }

    /// Changes how the window is kept, bringing it to the workspace on screen
    /// with focus. See `insert`. Does nothing if it isn't in any workspace.
    pub fn set_mode(&mut self, window: W, mode: Mode, width: Option<Width>) {
        if self.detach(window) {
            self.insert(window, mode, width);
        }
    }

    /// Focuses the window, switching to its workspace. Returns `false` if it
    /// isn't in any workspace.
    pub fn focus(&mut self, window: W) -> bool {
        let Some(home) = self.home(window) else {
            return false;
        };
        self.floating_focus = match home {
            Home::Tiled(index) => {
                self.active = index;
                self.stack[index].strip.focus(window);
                None
            }
            Home::Floating(index) => {
                self.active = index;
                Some(window)
            }
            Home::Sticky | Home::OnTop => Some(window),
        };
        self.tidy();
        true
    }

    /// Moves focus one column or one workspace over, wrapping round at the
    /// ends. Returns `false` when there is nowhere else to go.
    pub fn focus_toward(&mut self, direction: Direction) -> bool {
        match direction {
            Direction::Left => self.focus_column(Step::Back),
            Direction::Right => self.focus_column(Step::Forward),
            Direction::Up => self.switch(Step::Back),
            Direction::Down => self.switch(Step::Forward),
        }
    }

    /// Switches to the workspace at `index`, counted from the top, or to the
    /// empty one at the bottom when there aren't that many. Returns `false`
    /// if it's already on screen.
    pub fn focus_workspace(&mut self, index: usize) -> bool {
        self.show(index.min(self.stack.len() - 1))
    }

    /// Moves the focused column past its neighbour, or the focused window
    /// into the next workspace, and follows it. Returns `false` when nothing
    /// moved: a floating window isn't in the row of columns.
    pub fn move_toward(&mut self, direction: Direction) -> bool {
        let shift =
            |strip: Option<&mut Strip<W>>, step| strip.is_some_and(|strip| strip.move_toward(step));
        let next = |step: Step| step.from(self.active, self.stack.len(), self.options.wrap_move);
        match direction {
            Direction::Left => shift(self.focused_strip(), Step::Back),
            Direction::Right => shift(self.focused_strip(), Step::Forward),
            Direction::Up => next(Step::Back).is_some_and(|target| self.send(target)),
            Direction::Down => next(Step::Forward).is_some_and(|target| self.send(target)),
        }
    }

    /// Moves the focused window to the workspace at `index` and follows it.
    /// See `focus_workspace` for how workspaces are counted. Returns `false`
    /// when nothing moved.
    pub fn move_to_workspace(&mut self, index: usize) -> bool {
        self.send(index.min(self.stack.len() - 1))
    }

    /// See `Strip::cycle_width`. Does nothing to a floating window.
    pub fn cycle_width(&mut self) -> bool {
        self.focused_strip().is_some_and(Strip::cycle_width)
    }

    /// See `Strip::resize`. Does nothing to a floating window.
    pub fn resize(&mut self, percent: i32) -> bool {
        self.focused_strip()
            .is_some_and(|strip| strip.resize(percent))
    }

    /// See `Strip::toggle_full`. Does nothing to a floating window.
    pub fn toggle_full(&mut self) -> bool {
        self.focused_strip().is_some_and(Strip::toggle_full)
    }

    /// Where every window goes. Windows on other workspaces keep their place
    /// in their own strip and aren't visible, so switching moves nothing.
    pub fn placements(&self) -> impl Iterator<Item = Placement<W>> {
        let active = self.active;
        let float = move |window: &W, workspace| Placement {
            window: *window,
            workspace,
            rect: None,
            visible: workspace == active,
        };
        let kept = self
            .stack
            .iter()
            .enumerate()
            .flat_map(move |(index, workspace)| {
                let tiled = workspace
                    .strip
                    .placements()
                    .map(move |placement| Placement {
                        workspace: index,
                        visible: placement.visible && index == active,
                        ..placement
                    });
                let floating = workspace.floating.iter();
                tiled.chain(floating.map(move |window| float(window, index)))
            });
        let everywhere = self.sticky.iter().chain(&self.on_top);
        kept.chain(everywhere.map(move |window| float(window, active)))
    }

    fn home(&self, window: W) -> Option<Home> {
        if self.sticky.contains(&window) {
            return Some(Home::Sticky);
        }
        if self.on_top.contains(&window) {
            return Some(Home::OnTop);
        }
        self.stack
            .iter()
            .enumerate()
            .find_map(|(index, workspace)| {
                if workspace.strip.contains(window) {
                    Some(Home::Tiled(index))
                } else {
                    workspace
                        .floating
                        .contains(&window)
                        .then_some(Home::Floating(index))
                }
            })
    }

    /// Takes the window out of wherever it's kept, leaving the stack untidied.
    /// Returns `false` if it wasn't anywhere.
    fn detach(&mut self, window: W) -> bool {
        let Some(home) = self.home(window) else {
            return false;
        };
        match home {
            Home::Tiled(index) => {
                self.stack[index].strip.remove(window);
            }
            Home::Floating(index) => self.stack[index].floating.retain(|kept| *kept != window),
            Home::Sticky => self.sticky.retain(|kept| *kept != window),
            Home::OnTop => self.on_top.retain(|kept| *kept != window),
        }
        if self.floating_focus == Some(window) {
            self.floating_focus = None;
        }
        true
    }

    /// The strip that column keys act on, unless a floating window has focus.
    fn focused_strip(&mut self) -> Option<&mut Strip<W>> {
        self.floating_focus
            .is_none()
            .then(|| &mut self.stack[self.active].strip)
    }

    /// From a floating window, focus first comes back to the strip.
    fn focus_column(&mut self, step: Step) -> bool {
        let strip = &mut self.stack[self.active].strip;
        match self.floating_focus {
            None => strip.focus_toward(step),
            Some(_) if strip.is_empty() => false,
            Some(_) => {
                self.floating_focus = None;
                true
            }
        }
    }

    fn switch(&mut self, step: Step) -> bool {
        step.from(self.active, self.stack.len(), self.options.wrap_focus)
            .is_some_and(|target| self.show(target))
    }

    /// Puts the workspace at `target` on screen, with focus on its strip.
    /// Returns `false` if it already is.
    fn show(&mut self, target: usize) -> bool {
        if target == self.active {
            return false;
        }
        self.active = target;
        self.floating_focus = None;
        self.tidy();
        true
    }

    /// Moves the focused column or floating window to the workspace at
    /// `target` and puts that one on screen. Returns `false` when it's there
    /// already, as a window that shows on every workspace always is.
    fn send(&mut self, target: usize) -> bool {
        if target == self.active {
            return false;
        }
        let from = &mut self.stack[self.active];
        match self.floating_focus {
            None => {
                let Some(column) = from.strip.take_focused() else {
                    return false;
                };
                self.stack[target].strip.insert_column(column);
            }
            Some(window) => {
                let Some(index) = from.floating.iter().position(|kept| *kept == window) else {
                    return false;
                };
                from.floating.remove(index);
                self.stack[target].floating.push(window);
            }
        }
        self.active = target;
        self.tidy();
        true
    }

    /// Drops the empty unnamed workspaces nobody is looking at and makes sure
    /// the bottom one is empty and unnamed.
    fn tidy(&mut self) {
        let active = self.active;
        let unused = |workspace: &Workspace<W>| workspace.is_empty() && workspace.name.is_none();
        let dropped_above = self.stack[..active]
            .iter()
            .filter(|workspace| unused(workspace))
            .count();

        let mut index = 0;
        self.stack.retain(|workspace| {
            let keep = index == active || !unused(workspace);
            index += 1;
            keep
        });
        self.active -= dropped_above;

        if self.stack.last().is_none_or(|last| !unused(last)) {
            self.stack.push(self.blank(None));
        }
    }

    fn blank(&self, name: Option<String>) -> Workspace<W> {
        let mut workspace = Workspace::new(self.viewport, self.gap);
        workspace.strip.options = self.options.clone();
        workspace.name = name;
        workspace
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
            .for_each(|window| workspaces.insert(window, Mode::Tiled, None));
        workspaces
    }

    /// The tiled windows in each workspace, top to bottom.
    fn rows(workspaces: &Workspaces<u32>) -> Vec<Vec<u32>> {
        let tiled = |workspace: &Workspace<u32>| {
            let columns = workspace.strip.placements();
            columns.map(|placement| placement.window).collect()
        };
        workspaces.stack.iter().map(tiled).collect()
    }

    fn visible(workspaces: &Workspaces<u32>) -> Vec<u32> {
        workspaces
            .placements()
            .filter(|p| p.visible)
            .map(|p| p.window)
            .collect()
    }

    #[test]
    fn reloading_layout_preserves_workspaces_focus_and_widths_and_updates_future_strips() {
        let mut workspaces = workspaces([1, 2]);
        workspaces.move_toward(Direction::Down);
        let before_width = workspaces.width(2).unwrap();
        let before_rows = rows(&workspaces);
        let options = Options {
            widths: vec![(1, 4), (3, 4)],
            default_width: (3, 4),
            wrap_focus: false,
            wrap_move: false,
            ..Options::default()
        };
        let viewport = Rect {
            y: 20,
            height: 580,
            ..VIEWPORT
        };
        workspaces.configure(viewport, 12, options);
        assert_eq!(workspaces.active(), (1, 3));
        assert_eq!(workspaces.focused(), Some(2));
        assert_eq!(workspaces.width(2), Some(before_width));
        assert_eq!(rows(&workspaces), before_rows);
        let placed = workspaces
            .placements()
            .find(|p| p.window == 2)
            .unwrap()
            .rect
            .unwrap();
        assert_eq!((placed.y, placed.height), (32, 556));
        // The existing empty workspace and a newly created one inherit defaults.
        workspaces.focus_workspace(2);
        workspaces.insert(3, Mode::Tiled, None);
        assert_eq!(
            workspaces.width(3).unwrap().pixels,
            (1000 - 12) * 3 / 4 - 12
        );
        assert!(!workspaces.focus_toward(Direction::Right));
        assert!(!workspaces.move_toward(Direction::Left));
        workspaces.focus_workspace(3);
        assert!(!workspaces.focus_toward(Direction::Down));
        workspaces.insert(4, Mode::Tiled, None);
        assert_eq!(workspaces.width(4), workspaces.width(3));
        workspaces.cycle_width();
        assert_eq!(workspaces.width(4).unwrap().pixels, (1000 - 12) / 4 - 12);
    }

    #[test]
    fn shrinking_viewport_clamps_widths_without_changing_the_full_toggle() {
        let mut workspaces = workspaces([1, 2]);
        workspaces.toggle_full();
        workspaces.configure(
            Rect {
                width: 300,
                ..VIEWPORT
            },
            10,
            Options::default(),
        );
        assert_eq!(workspaces.width(1).unwrap().pixels, 280);
        assert_eq!(
            workspaces.width(2),
            Some(Width {
                pixels: 280,
                full: true
            })
        );
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

    #[test]
    fn a_workspace_is_reached_by_its_number() {
        let mut workspaces = workspaces([1, 2]);
        workspaces.move_toward(Direction::Down);

        assert!(workspaces.focus_workspace(0));
        assert_eq!(visible(&workspaces), [1]);
        assert!(!workspaces.focus_workspace(0), "already there");

        assert!(workspaces.focus_workspace(8));
        assert_eq!(rows(&workspaces), [vec![1], vec![2], vec![]]);
        assert!(
            visible(&workspaces).is_empty(),
            "past the last one is the empty one"
        );
    }

    #[test]
    fn a_window_is_sent_to_a_numbered_workspace_and_followed() {
        let mut workspaces = workspaces([1, 2]);
        assert!(workspaces.move_to_workspace(4));
        assert_eq!(rows(&workspaces), [vec![1], vec![2], vec![]]);
        assert_eq!(workspaces.active(), (1, 3));
        assert!(!workspaces.move_to_workspace(1), "already there");

        workspaces.set_mode(2, Mode::Floating, None);
        assert!(
            workspaces.move_to_workspace(0),
            "a floating window goes too"
        );
        assert_eq!(rows(&workspaces), [vec![1], vec![]]);
        assert_eq!(visible(&workspaces), [1, 2]);
        assert_eq!(workspaces.focused(), Some(2));

        workspaces.set_mode(2, Mode::Sticky, None);
        assert!(
            !workspaces.move_to_workspace(1),
            "a sticky window is on every workspace already"
        );
    }

    #[test]
    fn a_floating_window_leaves_the_strip_until_tiled_again() {
        let mut workspaces = workspaces([1, 2, 3]);
        workspaces.set_mode(3, Mode::Floating, None);
        assert_eq!(rows(&workspaces), [vec![1, 2], vec![]]);
        assert_eq!(workspaces.focused(), Some(3));
        assert!(
            !workspaces.move_toward(Direction::Left),
            "column keys leave a floating window alone"
        );

        assert!(workspaces.focus_toward(Direction::Left));
        assert_eq!(workspaces.focused(), Some(2), "back to the strip");

        workspaces.focus(1);
        workspaces.set_mode(3, Mode::Tiled, None);
        assert_eq!(
            rows(&workspaces),
            [vec![1, 3, 2], vec![]],
            "tiled beside the strip's focused column"
        );
    }

    fn names(workspaces: &Workspaces<u32>) -> Vec<Option<&str>> {
        workspaces.names().collect()
    }

    #[test]
    fn named_workspaces_stay_on_top_in_order_even_when_empty() {
        let mut workspaces = workspaces([1]);
        workspaces.name(&["web".into(), "chat".into()]);
        assert_eq!(names(&workspaces), [Some("web"), Some("chat"), None, None]);
        assert_eq!(rows(&workspaces), [vec![], vec![], vec![1], vec![]]);
        assert_eq!(workspaces.focused(), Some(1), "still on the same workspace");

        workspaces.focus_workspace(0);
        workspaces.focus(1);
        assert_eq!(rows(&workspaces).len(), 4, "left, but named");
        workspaces.name(&["chat".into()]);
        assert_eq!(names(&workspaces), [Some("chat"), None, None]);
        assert_eq!(workspaces.named("chat"), Some(0));
        assert_eq!(rows(&workspaces), [vec![], vec![1], vec![]]);
    }

    #[test]
    fn arranging_places_windows_in_order_without_switching() {
        let mut workspaces = workspaces([1, 2, 3]);
        let tiled = |window, workspace| Arranged {
            window,
            mode: Mode::Tiled,
            width: None,
            workspace,
            after: None,
        };
        // Past the end, 5 and 7 each get a new workspace, in that order.
        workspaces.arrange(&[tiled(3, 7), tiled(2, 0), tiled(1, 0), tiled(4, 5)]);
        assert_eq!(rows(&workspaces), [vec![2, 1], vec![4], vec![3], vec![]]);
        assert_eq!(workspaces.active(), (0, 4));

        workspaces.arrange(&[Arranged {
            after: Some(2),
            ..tiled(5, 0)
        }]);
        assert_eq!(rows(&workspaces)[0], [2, 5, 1]);
        workspaces.arrange(&[Arranged {
            mode: Mode::Floating,
            ..tiled(1, 1)
        }]);
        assert_eq!(rows(&workspaces)[0], [2, 5]);
        assert_eq!(workspaces.mode(1), Some(Mode::Floating));

        workspaces.focus(2);
        workspaces.arrange(&[tiled(2, 0)]);
        assert_eq!(workspaces.focused(), Some(2), "kept by a window that stays");
    }

    #[test]
    fn a_floating_window_shows_only_on_its_workspace_unless_pinned() {
        let mut workspaces = workspaces([1]);
        workspaces.set_mode(1, Mode::Floating, None);
        workspaces.focus_toward(Direction::Down);
        assert!(visible(&workspaces).is_empty());

        workspaces.focus(1);
        assert_eq!(
            visible(&workspaces),
            [1],
            "its workspace outlived its strip"
        );

        for pinned in [Mode::Sticky, Mode::OnTop] {
            workspaces.set_mode(1, pinned, None);
            workspaces.focus_toward(Direction::Down);
            assert_eq!(visible(&workspaces), [1]);
        }

        workspaces.set_mode(1, Mode::Floating, None);
        workspaces.focus_toward(Direction::Down);
        assert!(
            visible(&workspaces).is_empty(),
            "unpinned onto the workspace that was on screen"
        );
    }
}
