//! The pointer position, as the controller knows it.
//!
//! The controller reads the input devices itself, so it can say where the
//! pointer is — that is what lets it draw the cursor without asking any
//! application, and what makes the position one fact rather than one per
//! client. Events stay as the hardware sent them (`EV_REL` stays relative);
//! this is only the controller's own sum of them.

use evdev::{EventType, RelativeAxisType};

/// The pointer, clamped to the display it is on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pointer {
    position: (i32, i32),
    mode: (u32, u32),
}

impl Pointer {
    /// A pointer starts at the centre of the display: the first motion then
    /// moves it from somewhere sane instead of from a corner.
    pub fn new(mode: (u32, u32)) -> Self {
        let mut pointer = Self {
            position: (0, 0),
            mode,
        };
        pointer.position = ((mode.0 as i32) / 2, (mode.1 as i32) / 2);
        pointer
    }

    pub fn position(&self) -> (i32, i32) {
        self.position
    }

    /// The display's mode changed under the pointer (a new pipeline, a new
    /// panel): keep it inside the new bounds. Returns whether it moved.
    pub fn set_mode(&mut self, mode: (u32, u32)) -> bool {
        let before = self.position;
        self.mode = mode;
        self.clamp();
        self.position != before
    }

    /// Feed one evdev event. Returns whether the position moved, so a caller
    /// can decide whether a cursor commit is worth it.
    ///
    /// Only relative motion is integrated here. An absolute device (a tablet,
    /// a touch panel) reports `EV_ABS`, which needs the device's own axis range
    /// to become a pixel: that belongs with the projection work, and until then
    /// such a device moves the pointer not at all rather than wrongly.
    pub fn handle(&mut self, kind: EventType, code: u16, value: i32) -> bool {
        // The comparison brackets the whole update: taken after the delta, it
        // would compare the new position with itself and always say "unmoved".
        let before = self.position;
        // Compared, not pattern-matched: evdev's `EventType` is a newtype whose
        // associated constants are not constant patterns.
        if kind == EventType::RELATIVE && code == REL_X {
            self.position.0 += value;
        } else if kind == EventType::RELATIVE && code == REL_Y {
            self.position.1 += value;
        } else {
            // A wheel, a button, a sync: not motion.
            return false;
        }
        self.clamp();
        self.position != before
    }

    fn clamp(&mut self) {
        let max_x = (self.mode.0 as i32 - 1).max(0);
        let max_y = (self.mode.1 as i32 - 1).max(0);
        self.position = (
            self.position.0.clamp(0, max_x),
            self.position.1.clamp(0, max_y),
        );
    }
}

/// The two axis codes this integrates, as the wire numbers evdev reports.
const REL_X: u16 = RelativeAxisType::REL_X.0;
const REL_Y: u16 = RelativeAxisType::REL_Y.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_centred() {
        assert_eq!(Pointer::new((100, 40)).position(), (50, 20));
    }

    #[test]
    fn relative_motion_accumulates() {
        let mut pointer = Pointer::new((100, 100));
        assert!(pointer.handle(EventType::RELATIVE, REL_X, 5));
        assert!(pointer.handle(EventType::RELATIVE, REL_Y, -10));
        assert_eq!(pointer.position(), (55, 40));
    }

    #[test]
    fn the_edges_clamp_and_say_so_once() {
        let mut pointer = Pointer::new((100, 100));
        // Walk into the right edge: the last step that hits it moves, further
        // steps do not (nothing changes, so no commit is worth it).
        assert!(pointer.handle(EventType::RELATIVE, REL_X, 10_000));
        assert_eq!(pointer.position(), (99, 50));
        assert!(!pointer.handle(EventType::RELATIVE, REL_X, 10));
        assert!(pointer.handle(EventType::RELATIVE, REL_X, -1));
        assert_eq!(pointer.position(), (98, 50));
        // And the left/top corner, where a negative delta would otherwise
        // produce a position the kernel refuses.
        assert!(pointer.handle(EventType::RELATIVE, REL_Y, -10_000));
        assert_eq!(pointer.position(), (98, 0));
    }

    #[test]
    fn buttons_and_wheels_are_not_motion() {
        let mut pointer = Pointer::new((100, 100));
        let before = pointer.position();
        assert!(!pointer.handle(EventType::KEY, evdev::Key::BTN_LEFT.0, 1));
        assert!(!pointer.handle(EventType::SYNCHRONIZATION, 0, 0));
        // REL_WHEEL is relative, but it is not position.
        assert!(!pointer.handle(EventType::RELATIVE, RelativeAxisType::REL_WHEEL.0, 3));
        assert_eq!(pointer.position(), before);
    }

    #[test]
    fn a_new_mode_clamps_the_old_position() {
        let mut pointer = Pointer::new((1000, 1000));
        pointer.handle(EventType::RELATIVE, REL_X, 10_000);
        assert_eq!(pointer.position(), (999, 500));
        // A smaller panel arrives: the pointer cannot be outside it.
        assert!(pointer.set_mode((640, 480)));
        assert_eq!(pointer.position(), (639, 479));
        // The same mode again changes nothing.
        assert!(!pointer.set_mode((640, 480)));
    }
}
