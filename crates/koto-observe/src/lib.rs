//! Observation ladder contracts.
//!
//! Backends return `None` when their rung has no useful content.  The first
//! successful observer wins, preserving the structured-before-pixels rule.

use koto_core::{CoreError, Observation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    Cdp = 1,
    Tmux = 2,
    AtSpi = 3,
    Hypr = 4,
    Ocr = 5,
    Pixels = 6,
}

pub trait Observer {
    fn rung(&self) -> Rung;
    fn observe(&mut self) -> Result<Option<Observation>, CoreError>;
}

pub fn observe_ladder(observers: &mut [&mut dyn Observer]) -> Result<Observation, CoreError> {
    observers.sort_by_key(|observer| observer.rung());
    for observer in observers {
        if let Some(observation) = observer.observe()? {
            return Ok(observation);
        }
    }
    Err(CoreError::ObservationUnavailable(
        "no observation rung yielded content".into(),
    ))
}
