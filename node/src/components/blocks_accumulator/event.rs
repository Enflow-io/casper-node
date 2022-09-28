use std::fmt::{self, Display, Formatter};

use derive_more::From;

use casper_types::EraId;

use crate::effect::requests::BlocksAccumulatorRequest;
use crate::types::{BlockAdded, FinalitySignature, NodeId};

#[derive(Debug, From)]
pub(crate) enum Event {
    #[from]
    Request(BlocksAccumulatorRequest),
    ReceivedBlock {
        block: Box<BlockAdded>,
        sender: NodeId,
    },
    ReceivedFinalitySignature {
        finality_signature: Box<FinalitySignature>,
        sender: NodeId,
    },
    UpdatedValidatorMatrix {
        era_id: EraId,
    },
}

impl Display for Event {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Event::Request(BlocksAccumulatorRequest::GetPeersForBlock { block_hash, .. }) => {
                write!(
                    f,
                    "blocks accumulator peers request for block: {}",
                    block_hash
                )
            }
            Event::Request(_) => {
                write!(f, "blocks accumulator request from effect builder")
            }
            Event::ReceivedBlock { block, sender } => {
                write!(f, "received {} from {}", block, sender)
            }
            Event::ReceivedFinalitySignature {
                finality_signature,
                sender,
            } => {
                write!(f, "received {} from {}", finality_signature, sender)
            }
            Event::UpdatedValidatorMatrix { era_id } => {
                write!(f, "validator matrix update for era {}", era_id)
            }
        }
    }
}
