//! Query-local factorized bindings shared by the existing pull cursors.

mod asp;
mod asp_ids;
mod asp_runtime;
pub(crate) mod compact;
pub(crate) mod consume;
mod flatten;
mod packet;
pub(crate) mod producer;
mod table;

pub(crate) use flatten::FlattenContinuation;
pub(crate) use packet::{BindingPacket, GroupPacket, GroupProjection};
pub(crate) use table::{
    FACTOR_RESIDENT_BYTES, FactorTable, FactorTableBuilder, factor_member_slot_bytes,
    factor_private_metadata_bytes, factor_table_metadata_bytes,
};
