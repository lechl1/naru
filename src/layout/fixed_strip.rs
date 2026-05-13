//! Fixed panels — vertical strips pinned to the left/right edges of a workspace.
//!
//! A `FixedStrip` is a thin wrapper around `ScrollingSpace` operating in
//! "no-scroll" mode: `view_offset` stays at zero, scroll/center/edge-wrap
//! methods are never called. By construction the strip shares the carousel's
//! column/tile/gap math, so windows in a panel size and lay out exactly as
//! they would in the carousel.
//!
//! Windows enter a strip only via stack-move overflow at the carousel's
//! left/right edges. Cross-strip moves route through the carousel multi-step.
//! The strip's screen position is anchored to one workspace edge (left or
//! right); panel width is `sum(column.width) + gaps` and is zero while empty.

use std::rc::Rc;

use smithay::utils::{Logical, Rectangle, Size};

use super::scrolling::ScrollingSpace;
use super::tile::Tile;
use super::{LayoutElement, Options};
use crate::animation::Clock;

/// Which edge of the workspace working area this strip is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedSide {
    Left,
    Right,
}

/// A vertical strip of columns pinned to one edge of the workspace.
///
/// Wraps a `ScrollingSpace<W>` running with the carousel disabled. The wrapper
/// exposes only the operations that make sense for a panel — there is no
/// public surface for `view_offset` manipulation, scroll animation, or other
/// carousel-specific behaviour.
#[derive(Debug)]
pub struct FixedStrip<W: LayoutElement> {
    /// Which workspace edge this strip is anchored to.
    side: FixedSide,

    /// Underlying scrolling layout, used as a column container only.
    /// `view_offset` is never animated for this instance — callers must avoid
    /// invoking scroll/center methods on it.
    inner: ScrollingSpace<W>,
}

impl<W: LayoutElement> FixedStrip<W> {
    pub fn new(
        side: FixedSide,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        Self {
            side,
            inner: ScrollingSpace::new(view_size, parent_area, scale, clock, options),
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.inner
            .update_config(view_size, parent_area, scale, options);
    }

    pub fn side(&self) -> FixedSide {
        self.side
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Total logical width this strip occupies inside the workspace working
    /// area: sum of column widths plus inter-column gaps. Zero while empty.
    pub fn width(&self) -> f64 {
        self.inner.content_width()
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.inner.tiles()
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.inner.tiles_mut()
    }

    pub fn advance_animations(&mut self) {
        self.inner.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.inner.are_animations_ongoing()
    }
}
