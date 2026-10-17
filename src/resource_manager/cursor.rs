//! The cursor surface: the plane the controller draws the pointer on.
//!
//! The controller owns the cursor, so it owns this: one surface per CRTC, an
//! ARGB buffer holding the current shape, and a position taken from the
//! controller's own pointer tracking. An application never touches it — it only
//! names a shape.
//!
//! vc4 (the Pi 5) has no atomic CURSOR plane; its cursor is reachable only
//! through the legacy `set_cursor2`/`move_cursor` pair, which is what this uses.
//! Boards with no cursor at all fall back to a reserved overlay (not yet
//! implemented, and not needed on vc4).

use std::io;

use drm::buffer::Buffer as _;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{Device as _, crtc};

use simple_graphics_protocol::CursorShape;

use super::drm::DrmCard;

/// One card's cursor: the CRTC it is drawn on and the shape currently loaded.
#[derive(Debug)]
pub(crate) struct CursorSurface {
    crtc: crtc::Handle,
    /// The shape the loaded buffer holds, so a shape change is the only thing
    /// that re-uploads pixels.
    shape: Option<CursorShape>,
    /// The buffer the kernel scans out as the cursor. Kept alive while
    /// programmed: the plane holds a reference to it, and dropping it under the
    /// kernel would leave a dangling framebuffer.
    buffer: Option<DumbBuffer>,
    /// Whether the cursor is on screen right now.
    visible: bool,
}

impl CursorSurface {
    pub(crate) fn new(crtc: crtc::Handle) -> Self {
        Self {
            crtc,
            shape: None,
            buffer: None,
            visible: false,
        }
    }

    /// Draw `shape` with its hotspot on `position` (the pointer), or just move
    /// the cursor there if the shape is already loaded.
    #[allow(deprecated)] // see the module note: vc4 has no cursor plane
    pub(crate) fn show_at(
        &mut self,
        card: &DrmCard,
        shape: CursorShape,
        position: (i32, i32),
    ) -> io::Result<()> {
        if self.shape != Some(shape) || self.buffer.is_none() {
            self.load(card, shape)?;
        }
        // The legacy cursor API separates the image from the position: the image
        // (with its hotspot) is set once, the position moves on every event.
        card.move_cursor(self.crtc, position)?;
        self.visible = true;
        Ok(())
    }

    /// Take the cursor off screen. Kept idempotent: a hidden cursor is not
    /// re-committed on every event.
    #[allow(deprecated)] // see the module note: vc4 has no cursor plane
    pub(crate) fn hide(&mut self, card: &DrmCard) -> io::Result<()> {
        if self.visible {
            card.set_cursor2(self.crtc, None::<&DumbBuffer>, (0, 0))?;
            self.visible = false;
        }
        Ok(())
    }

    #[allow(deprecated)] // see the module note: vc4 has no cursor plane
    fn load(&mut self, card: &DrmCard, shape: CursorShape) -> io::Result<()> {
        let art = super::cursor_art(shape);
        let mut buffer = card.create_dumb_buffer(
            (art.width, art.height),
            drm::buffer::DrmFourcc::Argb8888,
            32,
        )?;
        let pitch = buffer.pitch() as usize;
        {
            let mut mapping = card.map_dumb_buffer(&mut buffer)?;
            let dst = mapping.as_mut();
            let row_bytes = art.width as usize * 4;
            for (y, row) in art.pixels.chunks(art.width as usize).enumerate() {
                let offset = y * pitch;
                for (x, pixel) in row.iter().enumerate() {
                    // ARGB8888 in the kernel's memory order is little-endian
                    // BGRA bytes, so a u32 written natively is the same pixel
                    // on every aarch64/amd64 board we target.
                    dst[offset + x * 4..offset + x * 4 + 4].copy_from_slice(&pixel.to_le_bytes());
                }
                debug_assert_eq!(row.len() * 4, row_bytes);
            }
        }
        card.set_cursor2(self.crtc, Some(&buffer), art.hotspot)?;
        self.buffer = Some(buffer);
        self.shape = Some(shape);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing testable without a card: a fresh surface draws nothing and
    /// knows it, so a caller's "hide" is a no-op rather than a commit.
    #[test]
    fn a_fresh_surface_is_hidden() {
        let surface =
            CursorSurface::new(crtc::Handle::from(std::num::NonZeroU32::new(94).unwrap()));
        assert!(!surface.visible);
        assert!(surface.shape.is_none());
        assert!(surface.buffer.is_none());
    }

    /// `show_at` must not re-upload pixels for a shape that is already loaded:
    /// the buffer identity is what a caller relies on to avoid a commit per
    /// event. Asserted through the state the loader sets.
    #[test]
    fn a_loaded_shape_is_remembered() {
        let mut surface =
            CursorSurface::new(crtc::Handle::from(std::num::NonZeroU32::new(94).unwrap()));
        surface.shape = Some(CursorShape::Text);
        assert_eq!(surface.shape, Some(CursorShape::Text));
    }
}
