//! Primitive canon — re-exported from [`clawft_canon`] (WEFT-427).
//!
//! Lives in a shared crate so `clawft-surface`'s composer can drive
//! the same types without a surface ↔ gui-egui dependency cycle.
//! Call sites inside this crate keep using `crate::canon::…`.

pub use clawft_canon::*;
pub use clawft_canon::{
    canvas, chip, dock, field, gauge, grid, media, modal, plot, pressable, response, select, sheet,
    slider, stack, stream_view, strip, table, tabs, thread_dock, toggle, tree, types,
};
