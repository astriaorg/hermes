use crate::error::Error;
use crate::event::IbcEventWithHeight;
use ibc_relayer_types::core::ics24_host::identifier::ChainId;
use ibc_relayer_types::core::ics24_host::identifier::ClientId;
use std::cmp::Ordering;
use tendermint::node;
use tendermint_rpc::Client as _;
use tendermint_rpc::HttpClient;

pub(crate) fn sort_events_by_sequence(events: &mut [IbcEventWithHeight]) {
    events.sort_by(|a, b| {
        a.event
            .packet()
            .zip(b.event.packet())
            .map(|(pa, pb)| pa.sequence.cmp(&pb.sequence))
            .unwrap_or(Ordering::Equal)
    });
}

/// Returns the suffix counter for a CosmosSDK client id.
/// Returns `None` if the client identifier is malformed
/// and the suffix could not be parsed.
pub(crate) fn client_id_suffix(client_id: &ClientId) -> Option<u64> {
    client_id
        .as_str()
        .split('-')
        .last()
        .and_then(|e| e.parse::<u64>().ok())
}

pub(crate) async fn fetch_node_info(
    rpc_client: &HttpClient,
    id: &ChainId,
    rpc_addr: &tendermint_rpc::Url,
) -> Result<node::Info, Error> {
    crate::time!("fetch_node_info",
    {
        "src_chain": id.to_string(),
    });

    rpc_client
        .status()
        .await
        .map(|s| s.node_info)
        .map_err(|e| Error::rpc(rpc_addr.clone(), e))
}
