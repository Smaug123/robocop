//! Explicit state machine for code review lifecycle.
//!
//! This module implements a pure functional state machine for managing
//! code reviews. The design separates:
//! - **State**: What the system knows (`ReviewMachineState`)
//! - **Events**: What happened (`Event`)
//! - **Effects**: What to do (`Effect`)
//! - **Transition**: Pure function `(State, Event) -> (State, Vec<Effect>)`
//!
//! The interpreter executes effects against real APIs and returns result events.

pub mod effect;
pub mod event;
pub mod state;
pub mod transition;

pub use effect::*;
pub use event::*;
pub use state::*;
pub use transition::*;
