//! Who gets an overlay plane when more than one client wants one.
//!
//! An overlay is not a claim on the display: the client holding the display
//! keeps it, whatever happens here. This module decides only which *client*
//! gets the plane of a pipeline, and what to say to the others. Keeping that
//! decision in one place is what makes the current rule — first come, never
//! preempted — a thing that can be read, tested, and later replaced, instead of
//! a condition buried in the grant path.
//!
//! The decision is a pure function of the claim and the grants that exist right
//! now. That is deliberate: a policy that also looked at, say, which client is
//! drawing (to reclaim an idle plane) would still be testable against the same
//! signature, because everything it needs arrives as an argument.

use crate::types::ClientId;

/// A client asking for an overlay of a card.
///
/// Note what is *not* here: whether the asker is the display's owner. Policy by
/// design does not see that — the app on the display asks like anyone else, and
/// there is no privileged class.
#[derive(Clone, Copy, Debug)]
pub(super) struct OverlayClaim {
    pub client: ClientId,
    pub card: u8,
}

/// A grant that exists right now, as much of it as a policy needs to know.
///
/// `plane` and `crtc` are here for policies that care which plane is held (a
/// reclaim of an idle plane needs to name it back to its owner); they are not
/// used by the rule below.
#[derive(Clone, Copy, Debug)]
pub(super) struct OverlayGrantView {
    pub client: ClientId,
    pub plane: u32,
    pub crtc: u32,
}

/// What the daemon should do about a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OverlayDecision {
    /// Put the asker on the plane. If the asker already holds it, that is the
    /// same grant restated, not a second one.
    Grant,
    /// Refuse, naming who holds the plane so the refusal can say it.
    Refuse { holder: ClientId },
}

/// How the plane of a pipeline is handed out.
pub(super) trait OverlayPolicy {
    fn decide(&self, claim: &OverlayClaim, grants: &[OverlayGrantView]) -> OverlayDecision;
}

/// One overlay per pipeline, first come first served, never taken away.
///
/// This is the rule the daemon was written with, now stated once. A client that
/// already holds the plane is granted it again rather than being told it is
/// held — a retry must not look like a conflict.
#[derive(Debug, Default)]
pub(super) struct NoPreemption;

impl OverlayPolicy for NoPreemption {
    fn decide(&self, claim: &OverlayClaim, grants: &[OverlayGrantView]) -> OverlayDecision {
        match grants.first() {
            None => OverlayDecision::Grant,
            Some(held) if held.client == claim.client => OverlayDecision::Grant,
            Some(held) => OverlayDecision::Refuse {
                holder: held.client,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(client: u64, card: u8) -> OverlayClaim {
        OverlayClaim {
            client: ClientId::new(client),
            card,
        }
    }

    fn held_by(client: u64) -> OverlayGrantView {
        OverlayGrantView {
            client: ClientId::new(client),
            plane: 272,
            crtc: 94,
        }
    }

    fn refuse(holder: u64) -> OverlayDecision {
        OverlayDecision::Refuse {
            holder: ClientId::new(holder),
        }
    }

    #[test]
    fn free_plane_is_granted() {
        assert_eq!(
            NoPreemption.decide(&claim(4, 0), &[]),
            OverlayDecision::Grant
        );
    }

    #[test]
    fn a_second_client_is_refused_and_told_who_has_it() {
        assert_eq!(NoPreemption.decide(&claim(5, 0), &[held_by(4)]), refuse(4));
    }

    #[test]
    fn the_holder_asking_again_is_granted_the_same_plane() {
        // A retry is not a conflict: the same client keeps the same plane.
        assert_eq!(
            NoPreemption.decide(&claim(4, 0), &[held_by(4)]),
            OverlayDecision::Grant
        );
    }

    #[test]
    fn the_card_does_not_enter_the_decision() {
        // The grants a card manager holds are its own; the claim's card is
        // carried for policies that reason across cards, and today's rule does
        // not care. If this ever changes, this test is the one to rewrite.
        assert_eq!(NoPreemption.decide(&claim(5, 3), &[held_by(4)]), refuse(4));
    }
}
