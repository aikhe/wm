//! Spring motion for the pictures of windows. Presentation only: layout and
//! keyboard focus never wait for a frame.
use serde::Deserialize;

use crate::layout::Rect;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Animations {
    pub enabled: bool,
    pub horizontal: Transition,
    pub workspaces: Transition,
    pub overview: Transition,
}

impl Default for Animations {
    fn default() -> Self {
        Self {
            enabled: true,
            horizontal: Transition::default(),
            workspaces: Transition::default(),
            overview: Transition::default(),
        }
    }
}

impl Animations {
    pub fn validate(&self) -> Result<(), String> {
        for (name, transition) in [
            ("horizontal", self.horizontal),
            ("workspaces", self.workspaces),
            ("overview", self.overview),
        ] {
            if transition.duration_ms > 2000 {
                return Err(format!("animations.{name}.duration_ms must be 0 to 2000"));
            }
        }
        Ok(())
    }

    /// How fast the effect's springs close in, or `None` when it's off.
    pub fn rate(&self, transition: Transition) -> Option<f64> {
        (self.enabled && transition.enabled && transition.duration_ms > 0)
            .then(|| SETTLE / (f64::from(transition.duration_ms) / 1000.0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Transition {
    pub enabled: bool,
    pub duration_ms: u32,
}

impl Default for Transition {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_ms: 300,
        }
    }
}

/// A spring from rest is within a thousandth of its target once its rate
/// times the seconds elapsed reaches this. It turns a duration into a rate.
const SETTLE: f64 = 9.233;

/// Slower than this, in units a second, a spring that has arrived stops.
const REST_SPEED: f64 = 30.0;

/// A critically damped spring, as niri uses: it eases in and out without
/// overshooting from rest, and keeps its speed when its target changes, so an
/// interrupted move bends toward the new place instead of restarting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    position: f64,
    velocity: f64,
    target: f64,
}

impl Spring {
    pub fn at(position: f64) -> Self {
        Self {
            position,
            velocity: 0.0,
            target: position,
        }
    }

    pub fn aim(&mut self, target: f64) {
        self.target = target;
    }

    /// Advances by `seconds`. The step is exact, so one slow frame lands where
    /// several fast ones would have.
    pub fn step(&mut self, rate: f64, seconds: f64) {
        let offset = self.position - self.target;
        let pull = self.velocity + rate * offset;
        let decay = (-rate * seconds).exp();
        let travelled = offset + pull * seconds;
        self.position = self.target + travelled * decay;
        self.velocity = (pull - rate * travelled) * decay;
        if (self.position - self.target).abs() < 0.5 && self.velocity.abs() < REST_SPEED {
            self.finish();
        }
    }

    pub fn finish(&mut self) {
        self.position = self.target;
        self.velocity = 0.0;
    }

    pub fn settled(&self) -> bool {
        self.position == self.target && self.velocity == 0.0
    }

    pub fn value(&self) -> i32 {
        self.position.round() as i32
    }
}

/// One window's picture: where it is shown first, and where it's heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slide<W> {
    pub window: W,
    pub from: Rect,
    pub to: Rect,
}

struct Track<W> {
    window: W,
    /// The rectangle's x, y, width and height.
    edges: [Spring; 4],
    /// No longer part of the scene: it finishes its move, then goes.
    leaving: bool,
}

impl<W> Track<W> {
    fn settled(&self) -> bool {
        self.edges.iter().all(Spring::settled)
    }
}

fn edges(rect: Rect) -> [f64; 4] {
    [rect.x, rect.y, rect.width, rect.height].map(f64::from)
}

/// The pictures in motion, bottom-most first.
pub struct Motion<W> {
    tracks: Vec<Track<W>>,
}

impl<W> Default for Motion<W> {
    fn default() -> Self {
        Self { tracks: Vec::new() }
    }
}

impl<W: Copy + Eq> Motion<W> {
    /// Sends every picture toward its slide's end. One already moving carries
    /// on from where it is, and a new one starts at its slide's beginning.
    /// Pictures the slides leave out finish the move they were making.
    pub fn retarget(&mut self, slides: &[Slide<W>]) {
        let mut tracks = std::mem::take(&mut self.tracks);
        let aimed: Vec<_> = slides
            .iter()
            .map(|slide| {
                let known = tracks.iter().position(|track| track.window == slide.window);
                let mut track = known.map_or_else(
                    || Track {
                        window: slide.window,
                        edges: edges(slide.from).map(Spring::at),
                        leaving: false,
                    },
                    |index| tracks.remove(index),
                );
                for (edge, target) in track.edges.iter_mut().zip(edges(slide.to)) {
                    edge.aim(target);
                }
                track.leaving = false;
                track
            })
            .collect();
        tracks.retain(|track| !track.settled());
        for track in &mut tracks {
            track.leaving = true;
        }
        tracks.extend(aimed);
        self.tracks = tracks;
    }

    pub fn step(&mut self, rate: f64, seconds: f64) {
        for edge in self.tracks.iter_mut().flat_map(|track| &mut track.edges) {
            edge.step(rate, seconds);
        }
        self.tracks
            .retain(|track| !(track.leaving && track.settled()));
    }

    /// Puts every picture where it was heading.
    pub fn finish(&mut self) {
        self.tracks.retain(|track| !track.leaving);
        for edge in self.tracks.iter_mut().flat_map(|track| &mut track.edges) {
            edge.finish();
        }
    }

    pub fn settled(&self) -> bool {
        self.tracks.iter().all(Track::settled)
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn contains(&self, window: W) -> bool {
        self.tracks.iter().any(|track| track.window == window)
    }

    pub fn remove(&mut self, window: W) {
        self.tracks.retain(|track| track.window != window);
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
    }

    /// Where each picture is now, in whole pixels.
    pub fn frames(&self) -> impl Iterator<Item = (W, Rect)> {
        self.tracks.iter().map(|track| {
            let [x, y, width, height] = track.edges.map(|edge| edge.value());
            (
                track.window,
                Rect {
                    x,
                    y,
                    width,
                    height,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = SETTLE / 0.3;
    const FRAME: f64 = 1.0 / 60.0;

    fn card(x: i32) -> Rect {
        Rect {
            x,
            y: 0,
            width: 100,
            height: 200,
        }
    }

    fn slide(from: i32, to: i32) -> Slide<u32> {
        Slide {
            window: 1,
            from: card(from),
            to: card(to),
        }
    }

    fn position(motion: &Motion<u32>, window: u32) -> Option<i32> {
        let mut frames = motion.frames();
        frames
            .find(|(known, _)| *known == window)
            .map(|(_, rect)| rect.x)
    }

    #[test]
    fn a_spring_eases_in_and_arrives_within_its_duration_without_overshooting() {
        let mut spring = Spring::at(0.0);
        spring.aim(1000.0);
        spring.step(RATE, FRAME);
        let first = spring.position;
        assert!(first < 150.0, "a gentle first frame, not a jump: {first}");
        let mut previous = first;
        for _ in 1..18 {
            spring.step(RATE, FRAME);
            assert!(spring.position >= previous && spring.position <= 1000.0);
            previous = spring.position;
        }
        assert!(previous > 998.0, "there after 300ms: {previous}");
        for _ in 0..12 {
            spring.step(RATE, FRAME);
        }
        assert!(spring.settled());
        assert_eq!(spring.value(), 1000);
    }

    #[test]
    fn one_slow_frame_lands_where_several_fast_ones_do() {
        let mut fast = Spring::at(0.0);
        let mut slow = fast;
        fast.aim(500.0);
        slow.aim(500.0);
        for _ in 0..4 {
            fast.step(RATE, FRAME);
        }
        slow.step(RATE, 4.0 * FRAME);
        assert!((fast.position - slow.position).abs() < 1e-6);
        assert!((fast.velocity - slow.velocity).abs() < 1e-6);
    }

    #[test]
    fn an_interrupted_move_keeps_its_place_and_speed() {
        let mut motion = Motion::default();
        motion.retarget(&[slide(0, 1000)]);
        for _ in 0..4 {
            motion.step(RATE, FRAME);
        }
        let turned = position(&motion, 1).unwrap();
        assert!(turned > 100 && turned < 900);

        // The new slide's beginning is ignored: the picture is already on its way.
        motion.retarget(&[slide(1000, 0)]);
        assert_eq!(position(&motion, 1), Some(turned));
        motion.step(RATE, FRAME);
        let carried = position(&motion, 1).unwrap();
        assert!(carried > turned, "still carrying its speed: {carried}");
        for _ in 0..60 {
            motion.step(RATE, FRAME);
        }
        assert!(motion.settled());
        assert_eq!(position(&motion, 1), Some(0));
    }

    #[test]
    fn a_picture_left_out_finishes_its_move_then_goes() {
        let mut motion = Motion::default();
        motion.retarget(&[slide(0, 1000)]);
        motion.step(RATE, FRAME);
        let other = Slide {
            window: 2,
            from: card(0),
            to: card(0),
        };
        motion.retarget(&[other]);
        assert!(motion.contains(1), "still on its way out");
        let order: Vec<_> = motion.frames().map(|(window, _)| window).collect();
        assert_eq!(order, [1, 2], "beneath the pictures of the scene");
        for _ in 0..60 {
            motion.step(RATE, FRAME);
        }
        assert!(!motion.contains(1) && motion.contains(2));

        // One at rest goes at once, and finishing drops any still moving.
        motion.retarget(&[slide(0, 1000)]);
        assert!(!motion.contains(2));
        motion.step(RATE, FRAME);
        motion.retarget(&[other]);
        motion.finish();
        assert!(motion.settled() && !motion.contains(1));
    }

    #[test]
    fn a_disabled_or_instant_effect_has_no_rate() {
        let on = Animations::default();
        assert!((on.rate(on.horizontal).unwrap() - RATE).abs() < 1e-9);
        let instant = Transition {
            duration_ms: 0,
            ..on.overview
        };
        assert!(on.rate(instant).is_none());
        let off = Animations {
            enabled: false,
            ..on
        };
        assert!(off.rate(off.workspaces).is_none());
    }
}
