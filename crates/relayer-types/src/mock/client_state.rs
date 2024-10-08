use std::time::Duration;

use ibc_proto::{google::protobuf::Any, ibc::mock::ClientState as RawMockClientState, Protobuf};
use serde::{Deserialize, Serialize};

use crate::{
    core::{
        ics02_client::{client_state::ClientState, client_type::ClientType, error::Error},
        ics24_host::identifier::ChainId,
    },
    mock::{consensus_state::MockConsensusState, header::MockHeader},
    Height,
};

pub const MOCK_CLIENT_STATE_TYPE_URL: &str = "/ibc.mock.ClientState";

/// A mock of a client state. For an example of a real structure that this mocks, you can see
/// `ClientState` of ics07_tendermint/client_state.rs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MockClientState {
    pub header: MockHeader,
    pub frozen_height: Option<Height>,
}

impl MockClientState {
    pub fn new(header: MockHeader) -> Self {
        Self {
            header,
            frozen_height: None,
        }
    }

    pub fn latest_height(&self) -> Height {
        self.header.height()
    }
}

impl Protobuf<RawMockClientState> for MockClientState {}

impl TryFrom<RawMockClientState> for MockClientState {
    type Error = Error;

    fn try_from(raw: RawMockClientState) -> Result<Self, Self::Error> {
        Ok(Self::new(raw.header.unwrap().try_into()?))
    }
}

impl From<MockClientState> for RawMockClientState {
    fn from(value: MockClientState) -> Self {
        RawMockClientState {
            header: Some(ibc_proto::ibc::mock::Header {
                height: Some(value.header.height().into()),
                timestamp: value.header.timestamp.nanoseconds(),
            }),
        }
    }
}

impl Protobuf<Any> for MockClientState {}

impl TryFrom<Any> for MockClientState {
    type Error = Error;

    fn try_from(raw: Any) -> Result<Self, Error> {
        use core::ops::Deref;

        use bytes::Buf;
        use prost::Message;

        fn decode_client_state<B: Buf>(buf: B) -> Result<MockClientState, Error> {
            RawMockClientState::decode(buf)
                .map_err(Error::decode)?
                .try_into()
        }

        match raw.type_url.as_str() {
            MOCK_CLIENT_STATE_TYPE_URL => {
                decode_client_state(raw.value.deref()).map_err(Into::into)
            }
            _ => Err(Error::unexpected_client_state_type(
                MOCK_CLIENT_STATE_TYPE_URL.to_string(),
                raw.type_url,
            )),
        }
    }
}

impl From<MockClientState> for Any {
    fn from(client_state: MockClientState) -> Self {
        Any {
            type_url: MOCK_CLIENT_STATE_TYPE_URL.to_string(),
            value: Protobuf::<RawMockClientState>::encode_vec(client_state),
        }
    }
}

impl ClientState for MockClientState {
    type UpgradeOptions = ();

    fn chain_id(&self) -> ChainId {
        unimplemented!()
    }

    fn client_type(&self) -> ClientType {
        ClientType::Mock
    }

    fn latest_height(&self) -> Height {
        self.header.height()
    }

    fn frozen_height(&self) -> Option<Height> {
        self.frozen_height
    }

    fn upgrade(&mut self, _upgrade_height: Height, _upgrade_options: (), _chain_id: ChainId) {
        unimplemented!()
    }

    fn expired(&self, _elapsed: Duration) -> bool {
        false
    }
}

impl From<MockConsensusState> for MockClientState {
    fn from(cs: MockConsensusState) -> Self {
        Self::new(cs.header)
    }
}
