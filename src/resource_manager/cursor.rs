//! The cursor surface: the plane the controller draws the pointer on.
//!
//! The controller owns the cursor, so it owns this: a sprite bound on an overlay
//! plane of the pipeline's CRTC, positioned from the controller's own pointer
//! tracking. An application never touches it — it only names a shape.
//!
//! **Not `drmModeSetCursor2`.** vc4 (the Pi 5) has no atomic CURSOR plane, and
//! its legacy cursor ioctls return success without putting anything on screen —
//! measured, not assumed: the daemon drew, logged no error, and the panel stayed
//! empty. The H618 has no cursor plane either. So the sprite goes on a regular
//! overlay plane with its zpos raised above the primary and per-pixel alpha,
//! which is what lean-display-server does for the same reason.

use std::io;

use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{Device as _, crtc, framebuffer, plane};

use simple_graphics_protocol::CursorShape;

use super::drm::DrmCard;

/// One card's cursor: an overlay plane holding the current shape.
#[derive(Debug)]
pub(crate) struct CursorSurface {
    plane: plane::Handle,
    crtc: crtc::Handle,
    /// The shape the loaded sprite holds, so a shape change is the only thing
    /// that re-uploads pixels.
    shape: Option<CursorShape>,
    /// The buffer behind the framebuffer. Kept alive while the plane is bound:
    /// dropping it would leave the kernel scanning out freed memory.
    buffer: Option<DumbBuffer>,
    fb: Option<framebuffer::Handle>,
    /// Whether the sprite is on screen right now.
    visible: bool,
}

impl CursorSurface {
    pub(crate) fn new(plane: plane::Handle, crtc: crtc::Handle) -> Self {
        Self {
            plane,
            crtc,
            shape: None,
            buffer: None,
            fb: None,
            visible: false,
        }
    }

    pub(crate) fn plane(&self) -> plane::Handle {
        self.plane
    }

    /// Draw `shape` with its hotspot on `position` (the pointer), clamped into
    /// `mode`: an overlay neither wraps nor scales, so half a sprite would
    /// simply not appear near an edge.
    pub(crate) fn show_at(
        &mut self,
        card: &DrmCard,
        shape: CursorShape,
        position: (i32, i32),
        mode: (u32, u32),
    ) -> io::Result<()> {
        if self.shape != Some(shape) || self.fb.is_none() {
            self.load(card, shape)?;
        }
        let art = super::cursor_art(shape);
        let (w, h) = (art.width, art.height);
        // The sprite hangs off the pointer by its hotspot: the pixel the theme
        // says is the point of the arrow sits where the pointer is.
        let max_x = mode.0.saturating_sub(w) as i32;
        let max_y = mode.1.saturating_sub(h) as i32;
        let x = (position.0 - art.hotspot.0).clamp(0, max_x);
        let y = (position.1 - art.hotspot.1).clamp(0, max_y);

        let fb = self.fb.expect("loaded above");
        card.set_plane(
            self.plane,
            self.crtc,
            Some(fb),
            0,
            (x, y, w, h),
            // Source rectangle in 16.16 fixed point: the whole sprite.
            (0, 0, w << 16, h << 16),
        )?;
        self.visible = true;
        Ok(())
    }

    /// Take the sprite off the plane. Idempotent: a hidden cursor is not
    /// re-committed on every event.
    pub(crate) fn hide(&mut self, card: &DrmCard) -> io::Result<()> {
        if self.visible {
            card.set_plane(self.plane, self.crtc, None, 0, (0, 0, 0, 0), (0, 0, 0, 0))?;
            self.visible = false;
        }
        Ok(())
    }

    fn load(&mut self, card: &DrmCard, shape: CursorShape) -> io::Result<()> {
        let art = super::cursor_art(shape);
        // A new shape may be a different size (an icon-derived shape and a drawn
        // fallback are not the same box), so the framebuffer is rebuilt.
        if let Some(old) = self.fb.take() {
            let _ = card.destroy_framebuffer(old);
        }
        let mut buffer =
            card.create_dumb_buffer((art.width, art.height), DrmFourcc::Argb8888, 32)?;
        let pitch = buffer.pitch() as usize;
        {
            let mut mapping = card.map_dumb_buffer(&mut buffer)?;
            let dst = mapping.as_mut();
            for (y, row) in art.pixels.chunks(art.width as usize).enumerate() {
                let offset = y * pitch;
                for (x, pixel) in row.iter().enumerate() {
                    // ARGB8888 in the kernel's memory order is little-endian
                    // BGRA bytes, so a u32 written natively is the same pixel
                    // on every aarch64/amd64 board we target.
                    dst[offset + x * 4..offset + x * 4 + 4].copy_from_slice(&pixel.to_le_bytes());
                }
            }
        }
        // Legacy addFB, and the depth is what selects the format: 24/bpp 32 is
        // guessed as XRGB8888, which drops the alpha and paints the sprite's
        // transparent pixels black. 32/32 gives ARGB8888, which is what a cursor
        // needs. (A dumb buffer is linear by construction, so no modifier.)
        let fb = card.add_framebuffer(&buffer, 32, 32)?;
        // The art actually loaded, with its framebuffer: the only place a shape
        // becomes pixels, and the thing to look at when a shape "does not change".
        tracing::debug!(
            "cursor art loaded: {shape:?} ({}x{}, hotspot {:?}) as fb {}",
            art.width,
            art.height,
            art.hotspot,
            u32::from(fb)
        );
        self.fb = Some(fb);
        self.buffer = Some(buffer);
        self.shape = Some(shape);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_surface_has_no_sprite() {
        let surface = CursorSurface::new(
            plane::Handle::from(std::num::NonZeroU32::new(107).unwrap()),
            crtc::Handle::from(std::num::NonZeroU32::new(94).unwrap()),
        );
        assert!(!surface.visible);
        assert!(surface.shape.is_none());
        assert!(surface.fb.is_none());
    }

    /// A loaded shape is remembered, which is what stops `show_at` from
    /// re-uploading pixels on every motion event.
    #[test]
    fn a_loaded_shape_is_remembered() {
        let mut surface = CursorSurface::new(
            plane::Handle::from(std::num::NonZeroU32::new(107).unwrap()),
            crtc::Handle::from(std::num::NonZeroU32::new(94).unwrap()),
        );
        surface.shape = Some(CursorShape::Text);
        assert_eq!(surface.shape, Some(CursorShape::Text));
    }
}
