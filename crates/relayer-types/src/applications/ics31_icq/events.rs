use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tendermint::{abci, block::Height};

use super::error::Error;
use crate::{
    core::ics24_host::identifier::{ChainId, ConnectionId},
    events::IbcEvent,
};

pub const EVENT_TYPE_PREFIX: &str = "query_request";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CrossChainQueryPacket {
    pub module: String,
    pub action: String,
    pub query_id: String,
    pub chain_id: ChainId,
    pub connection_id: ConnectionId,
    pub query_type: String,
    pub height: Height,
    pub request: String,
}

impl From<CrossChainQueryPacket> for IbcEvent {
    fn from(packet: CrossChainQueryPacket) -> Self {
        IbcEvent::CrossChainQueryPacket(packet)
    }
}

fn find_value<'a>(key: &str, entries: &'a [abci::EventAttribute]) -> Result<&'a str, Error> {
    entries
        .iter()
        .find_map(|entry| {
            if entry.key_bytes() == key.as_bytes() {
                Some(entry.value_str().map_err(|_| {
                    Error::event(format!(
                        "attribute value for key {key} is not a valid UTF-8 string"
                    ))
                }))
            } else {
                None
            }
        })
        .transpose()?
        .ok_or_else(|| Error::event(format!("attribute not found for key: {key}")))
}

fn new_attr(key: &str, value: &str) -> abci::EventAttribute {
    abci::EventAttribute::from((key, value, true))
}

impl From<CrossChainQueryPacket> for abci::Event {
    fn from(packet: CrossChainQueryPacket) -> Self {
        let attributes: Vec<abci::EventAttribute> = vec![
            new_attr("module", packet.module.as_str()),
            new_attr("action", packet.action.as_str()),
            new_attr("query_id", packet.query_id.as_str()),
            new_attr("chain_id", packet.chain_id.as_str()),
            new_attr("connection_id", packet.connection_id.as_str()),
            new_attr("type", &packet.query_type.to_string()),
            new_attr("request", packet.request.as_str()),
            new_attr("height", &packet.height.to_string()),
        ];

        abci::Event {
            kind: String::from("message"),
            attributes,
        }
    }
}

impl<'a> TryFrom<&'a [abci::EventAttribute]> for CrossChainQueryPacket {
    type Error = Error;

    fn try_from(entries: &'a [abci::EventAttribute]) -> Result<Self, Error> {
        let module = find_value("module", entries)?.to_string();
        let action = find_value("action", entries)?.to_string();
        let query_id = find_value("query_id", entries)?.to_string();
        let chain_id_str = find_value("chain_id", entries)?;
        let connection_id_str = find_value("connection_id", entries)?;
        let query_type = find_value("type", entries)?.to_string();
        let request = find_value("request", entries)?.to_string();
        let height_str = find_value("height", entries)?;

        let chain_id = ChainId::from_string(chain_id_str);
        let connection_id = ConnectionId::from_str(connection_id_str)?;
        let height = Height::from_str(height_str)?;

        Ok(Self {
            module,
            action,
            query_id,
            chain_id,
            connection_id,
            query_type,
            height,
            request,
        })
    }
}
