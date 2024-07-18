//! Response structures for chainstate api calls.

/// Response to get chainstate request.
pub type GetChainstateResponse = super::Chainstate;

/// Response to set chainstate request.
pub type SetChainstateResponse = super::Chainstate;

/// Response to update chainstate request.
#[allow(dead_code)] // Used by the utoipa generation.
pub type UpdateChainstateResponse = super::Chainstate;
