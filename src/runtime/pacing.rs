//! How often the window draws, which game frame it shows, and how far that
//! frame's 3D has moved toward the next list.
//!
//! The game produces a frame every 59.83 Hz tick (or a 3D list every other
//! tick). To draw between them, the window runs one list interval behind
//! the game: it shows a frame it already has, and moves the frame's 3D
//! polygons toward where they are in a newer list it also has. At the next
//! tick that newer frame is shown as is, so the motion is continuous.

use super::screen::FRAME_TIME;
use std::time::{Duration, Instant};

/// How often the window draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fps {
    /// Once per game frame, as the DS does: no interpolation.
    Ds,
    /// At the display's refresh rate (vsync), interpolating.
    Display,
    /// At most this many times a second, without vsync, interpolating.
    Cap(u32),
    /// As often as the host can, without vsync, interpolating.
    Uncapped,
}

impl Fps {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "ds" => Ok(Self::Ds),
            "display" | "vsync" => Ok(Self::Display),
            "uncapped" => Ok(Self::Uncapped),
            n => match n.parse::<u32>() {
                Ok(n @ 1..=1000) => Ok(Self::Cap(n)),
                _ => Err(format!(
                    "invalid fps: {text} (ds, display, uncapped, or 1-1000)"
                )),
            },
        }
    }

    /// Whether frames are drawn between game frames.
    pub fn interpolates(self) -> bool {
        self != Self::Ds
    }

    /// Whether presenting waits for the display's refresh.
    pub fn vsync(self) -> bool {
        matches!(self, Self::Ds | Self::Display)
    }
}

/// Lists further apart than this many frames are not interpolated: the
/// game stalled, or its 3D scene stood still.
const MAX_SPAN: u32 = 4;

/// A received frame: when it is due on screen, and its 3D list's swap
/// number, if it has a 3D layer.
#[derive(Clone, Copy, Debug)]
pub struct FrameTime {
    pub deadline: Instant,
    pub list: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub struct Pick {
    /// The frame to show.
    pub shown: usize,
    /// A later frame whose 3D list the shown list moves toward.
    pub next: Option<usize>,
    /// How far it has moved: 0 is the shown list as it is.
    pub alpha: f64,
    /// Earlier frames are no longer needed.
    pub keep_from: usize,
}

impl Pick {
    /// The newest frame as it is, without interpolation.
    pub fn newest(frames: usize) -> Self {
        let last = frames.saturating_sub(1);
        Self {
            shown: last,
            next: None,
            alpha: 0.0,
            keep_from: last,
        }
    }
}

/// What to draw at `now` from `frames` (oldest first, not empty).
pub fn pick(frames: &[FrameTime], now: Instant) -> Pick {
    pick_at(frames, desired_time(frames, now))
}

/// A continuous presentation clock. The game's 3D cadence can change between
/// one, two and more vblanks; changing the delay in one step skips geometry
/// poses and can briefly show older texture/2D snapshots.
#[derive(Default)]
pub struct Timeline {
    previous: Option<(Instant, Instant)>,
}

impl Timeline {
    pub fn pick(&mut self, frames: &[FrameTime], now: Instant) -> Pick {
        let wanted = desired_time(frames, now);
        let oldest = frames[0].deadline;
        let last = frames.len() - 1;
        let latest = if frames[last].list.is_some() {
            // Duplicate vblank frames do not provide a future 3D pose.
            // Hold at the newest list's first frame until its successor
            // exists, rather than jumping forward when it finally arrives.
            let mut first = last;
            while first > 0 && frames[first - 1].list == frames[last].list {
                first -= 1;
            }
            frames[first].deadline
        } else {
            frames[last].deadline
        };
        let at = match self.previous {
            Some((previous_now, previous_at))
                if now.saturating_duration_since(previous_now) <= FRAME_TIME * 6 =>
            {
                let elapsed = now.saturating_duration_since(previous_now);
                let nominal = previous_at + elapsed;
                let correction = elapsed / 8;
                if wanted >= nominal {
                    nominal + wanted.duration_since(nominal).min(correction)
                } else {
                    nominal - nominal.duration_since(wanted).min(correction)
                }
            }
            _ => wanted,
        }
        .clamp(oldest, latest);
        self.previous = Some((now, at));
        let mut pick = pick_at(frames, at);
        if let Some(list) = frames[pick.shown].list {
            // Text, sprites and texture memory can still update while a
            // 3D list is held. Use a recent snapshot of that same list;
            // only its geometry interpolation clock is held at the pose.
            let two_d_at = now.checked_sub(FRAME_TIME).unwrap_or(now);
            let end = pick.next.unwrap_or(frames.len());
            if let Some(shown) = (pick.shown..end)
                .rfind(|&i| frames[i].list == Some(list) && frames[i].deadline <= two_d_at)
            {
                pick.shown = shown;
            }
        }
        pick
    }
}

fn desired_time(frames: &[FrameTime], now: Instant) -> Instant {
    let span_limit = FRAME_TIME * MAX_SPAN;
    // When each list first appeared: the interval between the last two is
    // how far behind the game the window runs.
    let arrivals: Vec<Instant> = frames
        .iter()
        .enumerate()
        .filter(|&(i, frame)| frame.list.is_some() && (i == 0 || frames[i - 1].list != frame.list))
        .map(|(_, frame)| frame.deadline)
        .collect();
    let delay = match arrivals[..] {
        [.., earlier, later] => later
            .saturating_duration_since(earlier)
            .clamp(FRAME_TIME, span_limit),
        _ => FRAME_TIME,
    };
    now.checked_sub(delay).unwrap_or(now)
}

fn pick_at(frames: &[FrameTime], at: Instant) -> Pick {
    let span_limit = FRAME_TIME * MAX_SPAN;
    let shown = frames
        .iter()
        .rposition(|frame| frame.deadline <= at)
        .unwrap_or(0);
    let list = frames[shown].list;
    // The first frame (still held) that carried the shown list.
    let mut start = shown;
    while start > 0 && list.is_some() && frames[start - 1].list == list {
        start -= 1;
    }
    let still = Pick {
        shown,
        next: None,
        alpha: 0.0,
        keep_from: start,
    };
    if list.is_none() {
        return still;
    }
    let Some(next) =
        (shown + 1..frames.len()).find(|&j| frames[j].list.is_some_and(|l| Some(l) != list))
    else {
        return still;
    };
    let base = frames[start].deadline;
    let span = frames[next].deadline.saturating_duration_since(base);
    if span.is_zero() || span > span_limit {
        return still;
    }
    let moved = at.saturating_duration_since(base);
    Pick {
        next: Some(next),
        alpha: (moved.as_secs_f64() / span.as_secs_f64()).clamp(0.0, 1.0),
        ..still
    }
}

/// When to present next with a frame-rate cap of `rate`.
pub fn next_present(previous: Instant, now: Instant, rate: u32) -> Instant {
    let period = Duration::from_secs_f64(1.0 / f64::from(rate.max(1)));
    let next = previous + period;
    // Fell behind (the window was busy or hidden): restart from now.
    if next < now { now + period } else { next }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeline(start: Instant, lists: &[Option<u64>]) -> Vec<FrameTime> {
        lists
            .iter()
            .enumerate()
            .map(|(i, &list)| FrameTime {
                deadline: start + FRAME_TIME * i as u32,
                list,
            })
            .collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn a_list_every_frame_is_shown_one_frame_late_and_moved_toward_the_next() {
        let start = Instant::now();
        let frames = timeline(start, &[Some(1), Some(2), Some(3)]);
        let pick = pick(&frames, start + FRAME_TIME * 5 / 2);
        assert_eq!((pick.shown, pick.next, pick.keep_from), (1, Some(2), 1));
        assert!(close(pick.alpha, 0.5), "{}", pick.alpha);
    }

    #[test]
    fn a_list_every_other_frame_moves_over_both_frames() {
        let start = Instant::now();
        let frames = timeline(start, &[Some(1), Some(1), Some(2), Some(2), Some(3)]);
        // Two frames behind: halfway through the shown list's second frame.
        let pick = pick(&frames, start + FRAME_TIME * 7 / 2);
        assert_eq!((pick.shown, pick.next, pick.keep_from), (1, Some(2), 0));
        assert!(close(pick.alpha, 0.75), "{}", pick.alpha);
        // The list's first frame starts where the list is.
        let pick = super::pick(&frames, start + FRAME_TIME * 2);
        assert_eq!((pick.shown, pick.next), (0, Some(2)));
        assert!(close(pick.alpha, 0.0), "{}", pick.alpha);
    }

    #[test]
    fn without_a_newer_list_the_shown_frame_stands_still() {
        let start = Instant::now();
        let frames = timeline(start, &[Some(1), Some(2)]);
        let pick = pick(&frames, start + FRAME_TIME * 10);
        assert_eq!((pick.shown, pick.next, pick.alpha), (1, None, 0.0));
        // No 3D at all: nothing to move.
        let flat = timeline(start, &[None, None]);
        let pick = super::pick(&flat, start + FRAME_TIME * 3 / 2);
        assert_eq!((pick.shown, pick.next), (0, None));
    }

    #[test]
    fn lists_far_apart_are_not_interpolated() {
        let start = Instant::now();
        let mut frames = timeline(start, &[Some(1), Some(2)]);
        frames[1].deadline = start + FRAME_TIME * 10;
        let pick = pick(&frames, start + FRAME_TIME * 6);
        assert_eq!((pick.shown, pick.next), (0, None));
    }

    #[test]
    fn cadence_changes_do_not_skip_or_rewind_presented_poses() {
        let start = Instant::now();
        let all = timeline(
            start,
            &[
                Some(1),
                Some(2),
                Some(3),
                Some(3),
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
            ],
        );
        let sample = |frames: &[FrameTime], pick: &Pick| {
            let mut first = pick.shown;
            while first > 0 && frames[first - 1].list == frames[pick.shown].list {
                first -= 1;
            }
            frames[first].deadline
                + pick.next.map_or(Duration::ZERO, |next| {
                    frames[next]
                        .deadline
                        .duration_since(frames[first].deadline)
                        .mul_f64(pick.alpha)
                })
        };
        let mut clock = Timeline::default();
        let mut previous = start;
        let mut old_previous = start;
        let mut old_jump = Duration::ZERO;
        for tick in 0..=16 {
            let now = start + FRAME_TIME * tick / 2;
            let frames = &all[..(tick as usize / 2 + 1).min(all.len())];
            let shown = clock.pick(frames, now);
            let at = sample(frames, &shown);
            assert!(at >= previous, "pose went backwards at tick {tick}");
            assert!(
                at.duration_since(previous) <= FRAME_TIME.mul_f64(0.563),
                "pose jumped at tick {tick}"
            );
            previous = at;
            let old_at = sample(frames, &pick(frames, now));
            old_jump = old_jump.max(old_at.saturating_duration_since(old_previous));
            old_previous = old_at;
        }
        assert!(
            old_jump > FRAME_TIME * 2,
            "the fixture must reproduce the original cadence jump"
        );
    }

    #[test]
    fn dialogue_snapshots_keep_updating_when_the_3d_pose_is_held() {
        let start = Instant::now();
        let frames = timeline(
            start,
            &[
                Some(1),
                Some(2),
                Some(2),
                Some(2),
                Some(2),
                Some(2),
                Some(2),
            ],
        );
        let mut clock = Timeline::default();
        for frame in 3..frames.len() {
            let picked = clock.pick(
                &frames[..=frame],
                start + FRAME_TIME * frame as u32 + FRAME_TIME / 2,
            );
            assert_eq!(picked.shown, frame - 1);
            assert_eq!((picked.next, picked.alpha), (None, 0.0));
        }
    }

    #[test]
    fn fps_modes_parse() {
        assert_eq!(Fps::parse("ds"), Ok(Fps::Ds));
        assert_eq!(Fps::parse("display"), Ok(Fps::Display));
        assert_eq!(Fps::parse("144"), Ok(Fps::Cap(144)));
        assert_eq!(Fps::parse("uncapped"), Ok(Fps::Uncapped));
        assert!(Fps::parse("0").is_err());
        assert!(!Fps::Ds.interpolates() && Fps::Display.vsync() && !Fps::Cap(90).vsync());
    }

    #[test]
    fn capped_presents_keep_their_period_and_restart_after_falling_behind() {
        let start = Instant::now();
        let period = Duration::from_secs_f64(0.01);
        assert_eq!(next_present(start, start, 100), start + period);
        let late = start + Duration::from_secs(1);
        assert_eq!(next_present(start, late, 100), late + period);
    }
}
