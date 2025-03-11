use ibc_relayer_types::core::ics23_commitment::merkle::MerkleProof;
use ibc_types::DomainType as _;

use crate::error::Error;

pub(crate) fn decode_merkle_proof(proof_bytes: Vec<u8>) -> Result<MerkleProof, Error> {
    use ibc_types::core::commitment::MerkleProof as IbcTypesMerkleProof;

    let proof: IbcTypesMerkleProof =
        IbcTypesMerkleProof::decode::<bytes::Bytes>(proof_bytes.into())
            .map_err(|e| Error::other(e.to_string()))?;
    let proof_proto = proof.to_proto();
    Ok(proof_proto.into())
}
