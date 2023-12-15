use alloc::sync::Arc;
use std::{
    str::FromStr as _,
    time::Duration,
};

use ibc_proto::{
    ibc::{
        apps::fee::v1::{
            QueryIncentivizedPacketRequest,
            QueryIncentivizedPacketResponse,
        },
        core::{
            client::v1::query_client::QueryClient as IbcClientQueryClient,
            connection::v1::query_client::QueryClient as IbcConnectionQueryClient,
        },
    },
    Protobuf as _,
};
use ibc_relayer_types::{
    applications::ics31_icq::response::CrossChainQueryResponse,
    clients::ics07_tendermint::{
        client_state::ClientState as TendermintClientState,
        consensus_state::ConsensusState as TendermintConsensusState,
        header::Header,
    },
    core::{
        ics02_client::{
            client_type::ClientType,
            events::UpdateClient,
        },
        ics03_connection::connection::{
            ConnectionEnd,
            IdentifiedConnectionEnd,
        },
        ics04_channel::{
            channel::{
                ChannelEnd,
                IdentifiedChannelEnd,
            },
            packet::{
                PacketMsgType,
                Sequence,
            },
        },
        ics23_commitment::{
            commitment::{
                CommitmentPrefix,
                CommitmentProofBytes,
            },
            merkle::MerkleProof,
        },
        ics24_host::{
            identifier::{
                ChainId,
                ChannelId,
                ClientId,
                ConnectionId,
                PortId,
            },
            path::{
                ClientConsensusStatePath,
                ClientStatePath,
            },
            Path,
        },
    },
    proofs::{
        ConsensusProof,
        Proofs,
    },
    signer::Signer,
    timestamp::Timestamp,
    Height as ICSHeight,
};
use prost::Message;
use tendermint::time::Time as TmTime;
use tendermint_light_client::verifier::types::LightBlock;
use tendermint_rpc::{
    endpoint::{
        broadcast::{
            tx_commit,
            tx_sync,
            tx_sync::Response as TxResponse,
        },
        status,
    },
    event::EventData,
    Client,
    HttpClient,
    SubscriptionClient,
};
use tokio::runtime::Runtime as TokioRuntime;

use crate::{
    account::Balance,
    chain::{
        astria::utils::response_to_tx_sync_result,
        client::ClientSettings,
        cosmos::{
            query::QueryResponse,
            version::Specs,
            wait::wait_for_block_commits,
        },
        endpoint::{
            ChainEndpoint,
            ChainStatus,
            HealthCheck,
        },
        handle::Subscription,
        requests::*,
        tracking::TrackedMsgs,
    },
    client_state::{
        AnyClientState,
        IdentifiedAnyClientState,
    },
    config::ChainConfig,
    connection::ConnectionMsgType,
    consensus_state::AnyConsensusState,
    denom::DenomTrace,
    error::{
        Error,
        ErrorDetail::KeyNotFound,
        KeyNotFoundSubdetail,
    },
    event::IbcEventWithHeight,
    keyring::{
        AnySigningKeyPair,
        Ed25519KeyPair,
        KeyRing,
        SigningKeyPairSized,
    },
    light_client::{
        tendermint::LightClient,
        LightClient as _,
    },
    misbehaviour::MisbehaviourEvidence,
};

const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(10);

pub struct AstriaEndpoint {
    config: ChainConfig,
    keybase: KeyRing<Ed25519KeyPair>,
    sequencer_client: HttpClient,
    light_client: LightClient,
    /// gRPC client for sequencer app
    ibc_client_grpc_client: IbcClientQueryClient<tonic::transport::Channel>,
    ibc_connection_grpc_client: IbcConnectionQueryClient<tonic::transport::Channel>,
    rt: Arc<TokioRuntime>,
}

impl AstriaEndpoint {
    pub fn new(
        config: ChainConfig,
        keybase: KeyRing<Ed25519KeyPair>,
        rt: Arc<TokioRuntime>,
    ) -> Result<Self, Error> {
        let sequencer_client =
            HttpClient::new(config.rpc_addr().clone()).map_err(|e| Error::other(e.into()))?;

        let cosmos_config = match &config {
            ChainConfig::Astria(c) => c,
            _ => panic!("wrong chain config type"), // TODO no panic
        };

        use crate::chain::cosmos::fetch_node_info;
        let node_info = rt.block_on(fetch_node_info(&sequencer_client, cosmos_config))?;

        let light_client = LightClient::from_cosmos_sdk_config(cosmos_config, node_info.id)?;

        use http::Uri;
        let grpc_addr = Uri::from_str(&cosmos_config.grpc_addr.to_string())
            .map_err(|e| Error::invalid_uri(cosmos_config.grpc_addr.to_string(), e))?;
        let ibc_client_grpc_client = rt
            .block_on(IbcClientQueryClient::connect(grpc_addr.clone()))
            .map_err(Error::grpc_transport)?;
        let ibc_connection_grpc_client = rt
            .block_on(IbcConnectionQueryClient::connect(grpc_addr))
            .map_err(Error::grpc_transport)?;

        Ok(Self {
            config,
            keybase,
            sequencer_client,
            light_client,
            ibc_client_grpc_client,
            ibc_connection_grpc_client,
            rt,
        })
    }

    fn chain_status(&self) -> Result<status::Response, Error> {
        let status = self
            .rt
            .block_on(self.sequencer_client.status())
            .map_err(|e| Error::rpc(self.config.rpc_addr().clone(), e))?;

        Ok(status)
    }
}

impl AstriaEndpoint {
    fn block_on<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.rt.block_on(fut)
    }

    async fn broadcast_messages(&mut self, tracked_msgs: TrackedMsgs) -> Result<TxResponse, Error> {
        use ::astria_proto::native::sequencer::v1alpha1::{
            asset::default_native_asset_id,
            Action,
            UnsignedTransaction,
        };
        use penumbra_ibc::IbcRelay;
        use penumbra_proto::core::component::ibc::v1alpha1::IbcRelay as RawIbcRelay;

        let msg_len = tracked_msgs.msgs.len();
        let mut ibc_actions: Vec<Action> = Vec::with_capacity(msg_len);
        for msg in tracked_msgs.msgs {
            let ibc_action = RawIbcRelay {
                raw_action: Some(pbjson_types::Any {
                    type_url: msg.type_url,
                    value: msg.value.into(),
                }),
            };
            let non_raw = IbcRelay::try_from(ibc_action).map_err(|e| Error::other(e.into()))?;
            ibc_actions.push(Action::Ibc(non_raw));
        }

        let unsigned_tx = UnsignedTransaction {
            nonce: 0, // TODO
            actions: ibc_actions,
            fee_asset_id: default_native_asset_id(),
        };

        let signing_key: ed25519_consensus::SigningKey =
            self.get_key()?.signing_key().as_bytes().clone().into(); // TODO cache this
        let signed_tx = unsigned_tx.into_signed(&signing_key);
        let tx_bytes = signed_tx.into_raw().encode_to_vec();

        let resp = self
            .sequencer_client
            .broadcast_tx_sync(tx_bytes)
            .await
            .map_err(|e| Error::other(e.into()))?;
        Ok(resp)
    }

    fn query(
        &self,
        data: impl Into<Path>,
        height_query: QueryHeight,
        prove: bool,
    ) -> Result<QueryResponse, Error> {
        use crate::chain::cosmos::query::abci_query;

        let data_prefixed = format!("ibc-data/{}", data.into());

        let response = self.block_on(abci_query(
            &self.sequencer_client,
            self.config.rpc_addr(),
            "state/key".to_string(),
            data_prefixed,
            height_query.into(),
            prove,
        ))?;

        // TODO - Verify response proof, if requested.
        if prove {}

        Ok(response)
    }
}

impl ChainEndpoint for AstriaEndpoint {
    /// Type of light blocks for this chain
    type LightBlock = LightBlock;

    /// Type of headers for this chain
    type Header = Header;

    /// Type of consensus state for this chain
    type ConsensusState = TendermintConsensusState;

    /// Type of the client state for this chain
    type ClientState = TendermintClientState;

    /// The type of time for this chain
    type Time = TmTime;

    /// Type of the key pair used for signatures of messages on chain
    type SigningKeyPair = Ed25519KeyPair;

    /// Returns the chain's identifier
    fn id(&self) -> &ChainId {
        &self.config.id()
    }

    /// Returns the chain configuration
    fn config(&self) -> ChainConfig {
        self.config.clone()
    }

    // Life cycle

    /// Constructs the chain
    fn bootstrap(config: ChainConfig, rt: Arc<TokioRuntime>) -> Result<Self, Error> {
        // TODO: don't use a test keyring?
        let keybase = KeyRing::new_ed25519(crate::keyring::Store::Test, "test", config.id(), &None)
            .map_err(|e| Error::other(e.into()))?;
        Self::new(config, keybase, rt)
    }

    /// Shutdown the chain runtime
    fn shutdown(self) -> Result<(), Error> {
        Ok(())
    }

    /// Perform a health check
    fn health_check(&mut self) -> Result<HealthCheck, Error> {
        Ok(HealthCheck::Healthy)
    }

    // Events
    fn subscribe(&mut self) -> Result<Subscription, Error> {
        todo!()
    }

    // Keyring

    /// Returns the chain's keybase
    fn keybase(&self) -> &KeyRing<Self::SigningKeyPair> {
        &self.keybase
    }

    /// Returns the chain's keybase, mutably
    fn keybase_mut(&mut self) -> &mut KeyRing<Self::SigningKeyPair> {
        &mut self.keybase
    }

    fn get_signer(&self) -> Result<Signer, Error> {
        use std::str::FromStr as _;

        use crate::keyring::SigningKeyPair as _;

        Signer::from_str(&self.get_key()?.account()).map_err(|e| Error::other(e.into()))
    }

    /// Get the signing key pair
    fn get_key(&self) -> Result<Self::SigningKeyPair, Error> {
        self.keybase
            .get_key(self.config.key_name())
            .map_err(|e| Error::key_not_found(self.config.key_name().to_string(), e))
    }

    // Versioning

    /// Return the version of the IBC protocol that this chain is running, if known.
    fn version_specs(&self) -> Result<Specs, Error> {
        todo!()
    }

    // Send transactions

    /// Sends one or more transactions with `msgs` to chain and
    /// synchronously wait for it to be committed.
    fn send_messages_and_wait_commit(
        &mut self,
        tracked_msgs: TrackedMsgs,
    ) -> Result<Vec<IbcEventWithHeight>, Error> {
        let runtime = self.rt.clone();
        let msg_len = tracked_msgs.msgs.len();
        let resp = runtime.block_on(self.broadcast_messages(tracked_msgs))?;

        // `wait_for_block_commits` will append the events to this
        let mut resps = vec![response_to_tx_sync_result(self.id(), msg_len, resp)];

        runtime.block_on(wait_for_block_commits(
            self.config.id(),
            &self.sequencer_client,
            self.config.rpc_addr(),
            &DEFAULT_RPC_TIMEOUT,
            &mut resps,
        ))?;

        let events = resps
            .into_iter()
            .flat_map(|resp| resp.events)
            .collect::<Vec<_>>();

        Ok(events)
    }

    /// Sends one or more transactions with `msgs` to chain.
    /// Non-blocking alternative to `send_messages_and_wait_commit` interface.
    fn send_messages_and_wait_check_tx(
        &mut self,
        tracked_msgs: TrackedMsgs,
    ) -> Result<Vec<TxResponse>, Error> {
        let runtime = self.rt.clone();
        runtime
            .block_on(self.broadcast_messages(tracked_msgs))
            .map(|resp| vec![resp])
    }

    /// Fetch a header from the chain at the given height and verify it.
    fn verify_header(
        &mut self,
        trusted: ICSHeight,
        target: ICSHeight,
        client_state: &AnyClientState,
    ) -> Result<Self::LightBlock, Error> {
        let status = self.chain_status()?;
        if status.sync_info.catching_up {
            return Err(Error::chain_not_caught_up(
                self.config.rpc_addr().to_string(),
                self.id().clone(),
            ));
        }

        let latest_timestamp = status.sync_info.latest_block_time;
        self.light_client
            .verify(trusted, target, client_state, latest_timestamp)
            .map(|v| v.target)
    }

    /// Given a client update event that includes the header used in a client update,
    /// look for misbehaviour by fetching a header at same or latest height.
    fn check_misbehaviour(
        &mut self,
        update: &UpdateClient,
        client_state: &AnyClientState,
    ) -> Result<Option<MisbehaviourEvidence>, Error> {
        let status = self.chain_status()?;
        if status.sync_info.catching_up {
            return Err(Error::chain_not_caught_up(
                self.config.rpc_addr().to_string(),
                self.id().clone(),
            ));
        }

        let latest_timestamp = status.sync_info.latest_block_time;
        self.light_client
            .detect_misbehaviour(update, client_state, latest_timestamp)
    }

    // Queries

    /// Query the balance of the given account for the given denom.
    /// If no account is given, behavior must be specified, e.g. retrieve it from configuration file.
    /// If no denom is given, behavior must be specified, e.g. retrieve the denom used to pay tx fees.
    fn query_balance(&self, key_name: Option<&str>, denom: Option<&str>) -> Result<Balance, Error> {
        todo!()
    }

    /// Query the balances of the given account for all the denom.
    /// If no account is given, behavior must be specified, e.g. retrieve it from configuration file.
    fn query_all_balances(&self, key_name: Option<&str>) -> Result<Vec<Balance>, Error> {
        todo!()
    }

    /// Query the denomination trace given a trace hash.
    fn query_denom_trace(&self, hash: String) -> Result<DenomTrace, Error> {
        todo!()
    }

    fn query_commitment_prefix(&self) -> Result<CommitmentPrefix, Error> {
        // TODO get this from the penumbra const
        Ok(b"ibc-data".to_vec().try_into().unwrap())
    }

    /// Query the latest height and timestamp the application is at
    fn query_application_status(&self) -> Result<ChainStatus, Error> {
        let rt = self.rt.clone();
        let head_block = rt
            .block_on(self.sequencer_client.latest_block())
            .map_err(|e| Error::rpc(self.config.rpc_addr().clone(), e))?;
        Ok(ChainStatus {
            height: ICSHeight::new(self.id().version(), head_block.block.header.height.value())
                .map_err(|e| Error::other(Box::new(e)))?,
            timestamp: head_block.block.header.time.into(),
        })
    }

    /// Performs a query to retrieve the state of all clients that a chain hosts.
    fn query_clients(
        &self,
        request: QueryClientStatesRequest,
    ) -> Result<Vec<IdentifiedAnyClientState>, Error> {
        use tracing::warn;

        use crate::{
            chain::cosmos::client_id_suffix,
            util::pretty::PrettyIdentifiedClientState,
        };

        let rt = self.rt.clone();

        let mut client = self
            .ibc_client_grpc_client
            .clone()
            .max_decoding_message_size(self.config().max_grpc_decoding_size().get_bytes() as usize);

        let request = tonic::Request::new(request.into());
        let response = rt
            .block_on(client.client_states(request))
            .map_err(|e| Error::grpc_status(e, "query_clients".to_owned()))?
            .into_inner();

        // Deserialize into domain type
        let mut clients: Vec<IdentifiedAnyClientState> = response
            .client_states
            .into_iter()
            .filter_map(|cs| {
                IdentifiedAnyClientState::try_from(cs.clone())
                    .map_err(|e| {
                        warn!(
                            "failed to parse client state {}. Error: {}",
                            PrettyIdentifiedClientState(&cs),
                            e
                        )
                    })
                    .ok()
            })
            .collect();

        // Sort by client identifier counter
        clients.sort_by_cached_key(|c| client_id_suffix(&c.client_id).unwrap_or(0));

        Ok(clients)
    }

    /// Performs a query to retrieve the state of the specified light client. A
    /// proof can optionally be returned along with the result.
    fn query_client_state(
        &self,
        request: QueryClientStateRequest,
        include_proof: IncludeProof,
    ) -> Result<(AnyClientState, Option<MerkleProof>), Error> {
        let res = self.query(
            ClientStatePath(request.client_id.clone()),
            request.height,
            matches!(include_proof, IncludeProof::Yes),
        )?;
        let client_state = AnyClientState::decode_vec(&res.value).map_err(Error::decode)?;

        match include_proof {
            IncludeProof::Yes => {
                let proof = res.proof.ok_or_else(Error::empty_response_proof)?;
                Ok((client_state, Some(proof)))
            }
            IncludeProof::No => Ok((client_state, None)),
        }
    }

    /// Query the consensus state at the specified height for a given client.
    fn query_consensus_state(
        &self,
        request: QueryConsensusStateRequest,
        include_proof: IncludeProof,
    ) -> Result<(AnyConsensusState, Option<MerkleProof>), Error> {
        let res = self.query(
            ClientConsensusStatePath {
                client_id: request.client_id.clone(),
                epoch: request.consensus_height.revision_number(),
                height: request.consensus_height.revision_height(),
            },
            request.query_height,
            matches!(include_proof, IncludeProof::Yes),
        )?;

        let consensus_state = AnyConsensusState::decode_vec(&res.value).map_err(Error::decode)?;

        if !matches!(consensus_state, AnyConsensusState::Tendermint(_)) {
            return Err(Error::consensus_state_type_mismatch(
                ClientType::Tendermint,
                consensus_state.client_type(),
            ));
        }

        match include_proof {
            IncludeProof::Yes => {
                let proof = res.proof.ok_or_else(Error::empty_response_proof)?;
                Ok((consensus_state, Some(proof)))
            }
            IncludeProof::No => Ok((consensus_state, None)),
        }
    }

    /// Query the heights of every consensus state for a given client.
    fn query_consensus_state_heights(
        &self,
        request: QueryConsensusStateHeightsRequest,
    ) -> Result<Vec<ICSHeight>, Error> {
        todo!()
    }

    fn query_upgraded_client_state(
        &self,
        request: QueryUpgradedClientStateRequest,
    ) -> Result<(AnyClientState, MerkleProof), Error> {
        todo!()
    }

    fn query_upgraded_consensus_state(
        &self,
        request: QueryUpgradedConsensusStateRequest,
    ) -> Result<(AnyConsensusState, MerkleProof), Error> {
        todo!()
    }

    /// Performs a query to retrieve the identifiers of all connections.
    fn query_connections(
        &self,
        request: QueryConnectionsRequest,
    ) -> Result<Vec<IdentifiedConnectionEnd>, Error> {
        use tracing::warn;

        use crate::util::pretty::PrettyIdentifiedConnection;

        let mut client = self.ibc_connection_grpc_client.clone();

        client = client
            .max_decoding_message_size(self.config().max_grpc_decoding_size().get_bytes() as usize);

        let request = tonic::Request::new(request.into());

        let response = self
            .block_on(client.connections(request))
            .map_err(|e| Error::grpc_status(e, "query_connections".to_owned()))?
            .into_inner();

        let connections = response
            .connections
            .into_iter()
            .filter_map(|co| {
                IdentifiedConnectionEnd::try_from(co.clone())
                    .map_err(|e| {
                        warn!(
                            "connection with ID {} failed parsing. Error: {}",
                            PrettyIdentifiedConnection(&co),
                            e
                        )
                    })
                    .ok()
            })
            .collect();

        Ok(connections)
    }

    /// Performs a query to retrieve the identifiers of all connections.
    fn query_client_connections(
        &self,
        request: QueryClientConnectionsRequest,
    ) -> Result<Vec<ConnectionId>, Error> {
        todo!()
    }

    /// Performs a query to retrieve the connection associated with a given
    /// connection identifier. A proof can optionally be returned along with the
    /// result.
    fn query_connection(
        &self,
        request: QueryConnectionRequest,
        include_proof: IncludeProof,
    ) -> Result<(ConnectionEnd, Option<MerkleProof>), Error> {
        todo!()
    }

    /// Performs a query to retrieve all channels associated with a connection.
    fn query_connection_channels(
        &self,
        request: QueryConnectionChannelsRequest,
    ) -> Result<Vec<IdentifiedChannelEnd>, Error> {
        todo!()
    }

    /// Performs a query to retrieve all the channels of a chain.
    fn query_channels(
        &self,
        request: QueryChannelsRequest,
    ) -> Result<Vec<IdentifiedChannelEnd>, Error> {
        todo!()
    }

    /// Performs a query to retrieve the channel associated with a given channel
    /// identifier. A proof can optionally be returned along with the result.
    fn query_channel(
        &self,
        request: QueryChannelRequest,
        include_proof: IncludeProof,
    ) -> Result<(ChannelEnd, Option<MerkleProof>), Error> {
        todo!()
    }

    /// Performs a query to retrieve the client state for the channel associated
    /// with a given channel identifier.
    fn query_channel_client_state(
        &self,
        request: QueryChannelClientStateRequest,
    ) -> Result<Option<IdentifiedAnyClientState>, Error> {
        todo!()
    }

    /// Performs a query to retrieve a stored packet commitment hash, stored on
    /// the chain at path `path::CommitmentsPath`. A proof can optionally be
    /// returned along with the result.
    fn query_packet_commitment(
        &self,
        request: QueryPacketCommitmentRequest,
        include_proof: IncludeProof,
    ) -> Result<(Vec<u8>, Option<MerkleProof>), Error> {
        todo!()
    }

    /// Performs a query to retrieve all the packet commitments hashes
    /// associated with a channel. Returns the corresponding packet sequence
    /// numbers and the height at which they were retrieved.
    fn query_packet_commitments(
        &self,
        request: QueryPacketCommitmentsRequest,
    ) -> Result<(Vec<Sequence>, ICSHeight), Error> {
        todo!()
    }

    /// Performs a query to retrieve a given packet receipt, stored on the chain at path
    /// `path::CommitmentsPath`. A proof can optionally be returned along with the result.
    fn query_packet_receipt(
        &self,
        request: QueryPacketReceiptRequest,
        include_proof: IncludeProof,
    ) -> Result<(Vec<u8>, Option<MerkleProof>), Error> {
        todo!()
    }

    /// Performs a query about which IBC packets in the specified list has not
    /// been received. Returns the sequence numbers of the packets that were not
    /// received.
    ///
    /// For example, given a request with the sequence numbers `[5,6,7,8]`, a
    /// response of `[7,8]` would indicate that packets 5 & 6 were received,
    /// while packets 7, 8 were not.
    fn query_unreceived_packets(
        &self,
        request: QueryUnreceivedPacketsRequest,
    ) -> Result<Vec<Sequence>, Error> {
        todo!()
    }

    /// Performs a query to retrieve a stored packet acknowledgement hash,
    /// stored on the chain at path `path::AcksPath`. A proof can optionally be
    /// returned along with the result.
    fn query_packet_acknowledgement(
        &self,
        request: QueryPacketAcknowledgementRequest,
        include_proof: IncludeProof,
    ) -> Result<(Vec<u8>, Option<MerkleProof>), Error> {
        todo!()
    }

    /// Performs a query to retrieve all the packet acknowledgements associated
    /// with a channel. Returns the corresponding packet sequence numbers and
    /// the height at which they were retrieved.
    fn query_packet_acknowledgements(
        &self,
        request: QueryPacketAcknowledgementsRequest,
    ) -> Result<(Vec<Sequence>, ICSHeight), Error> {
        todo!()
    }

    /// Performs a query about which IBC packets in the specified list has not
    /// been acknowledged. Returns the sequence numbers of the packets that were not
    /// acknowledged.
    ///
    /// For example, given a request with the sequence numbers `[5,6,7,8]`, a
    /// response of `[7,8]` would indicate that packets 5 & 6 were acknowledged,
    /// while packets 7, 8 were not.
    fn query_unreceived_acknowledgements(
        &self,
        request: QueryUnreceivedAcksRequest,
    ) -> Result<Vec<Sequence>, Error> {
        todo!()
    }

    /// Performs a query to retrieve `nextSequenceRecv` stored at path
    /// `path::SeqRecvsPath` as defined in ICS-4. A proof can optionally be
    /// returned along with the result.
    fn query_next_sequence_receive(
        &self,
        request: QueryNextSequenceReceiveRequest,
        include_proof: IncludeProof,
    ) -> Result<(Sequence, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_txs(&self, request: QueryTxRequest) -> Result<Vec<IbcEventWithHeight>, Error> {
        todo!()
    }

    fn query_packet_events(
        &self,
        request: QueryPacketEventDataRequest,
    ) -> Result<Vec<IbcEventWithHeight>, Error> {
        todo!()
    }

    fn query_host_consensus_state(
        &self,
        request: QueryHostConsensusStateRequest,
    ) -> Result<Self::ConsensusState, Error> {
        todo!()
    }

    fn build_client_state(
        &self,
        height: ICSHeight,
        settings: ClientSettings,
    ) -> Result<Self::ClientState, Error> {
        todo!()
    }

    fn build_header(
        &mut self,
        trusted_height: ICSHeight,
        target_height: ICSHeight,
        client_state: &AnyClientState,
    ) -> Result<(Self::Header, Vec<Self::Header>), Error> {
        todo!()
    }

    fn build_consensus_state(
        &self,
        light_block: Self::LightBlock,
    ) -> Result<Self::ConsensusState, Error> {
        todo!()
    }

    fn maybe_register_counterparty_payee(
        &mut self,
        channel_id: &ChannelId,
        port_id: &PortId,
        counterparty_payee: &Signer,
    ) -> Result<(), Error> {
        todo!()
    }

    fn cross_chain_query(
        &self,
        requests: Vec<CrossChainQueryRequest>,
    ) -> Result<Vec<CrossChainQueryResponse>, Error> {
        todo!()
    }

    fn query_incentivized_packet(
        &self,
        request: QueryIncentivizedPacketRequest,
    ) -> Result<QueryIncentivizedPacketResponse, Error> {
        todo!()
    }

    fn query_consumer_chains(&self) -> Result<Vec<(ChainId, ClientId)>, Error> {
        todo!()
    }
}
