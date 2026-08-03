use cid::Cid;
use fvm::externs::{Chain, Consensus, Externs, Rand};
use fvm_ipld_encoding::DAG_CBOR;
use fvm_shared::clock::ChainEpoch;
use fvm_shared::consensus::ConsensusFault;
use multihash_codetable::{Code, MultihashDigest};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DeterministicExterns;

impl DeterministicExterns {
    fn digest(domain: u8, round: ChainEpoch) -> cid::multihash::Multihash<64> {
        let mut input = [0_u8; 9];
        input[0] = domain;
        input[1..].copy_from_slice(&round.to_be_bytes());
        Code::Blake2b256.digest(&input)
    }

    fn randomness(domain: u8, round: ChainEpoch) -> [u8; 32] {
        let digest = Self::digest(domain, round);
        let mut randomness = [0_u8; 32];
        randomness.copy_from_slice(digest.digest());
        randomness
    }
}

impl Rand for DeterministicExterns {
    fn get_chain_randomness(&self, round: ChainEpoch) -> anyhow::Result<[u8; 32]> {
        Ok(Self::randomness(0, round))
    }

    fn get_beacon_randomness(&self, round: ChainEpoch) -> anyhow::Result<[u8; 32]> {
        Ok(Self::randomness(1, round))
    }
}

impl Consensus for DeterministicExterns {
    fn verify_consensus_fault(
        &self,
        _h1: &[u8],
        _h2: &[u8],
        _extra: &[u8],
    ) -> anyhow::Result<(Option<ConsensusFault>, i64)> {
        Ok((None, 0))
    }
}

impl Chain for DeterministicExterns {
    fn get_tipset_cid(&self, epoch: ChainEpoch) -> anyhow::Result<Cid> {
        Ok(Cid::new_v1(DAG_CBOR, Self::digest(2, epoch)))
    }
}

impl Externs for DeterministicExterns {}

#[cfg(test)]
mod tests {
    use fvm::externs::{Chain, Rand};

    use super::DeterministicExterns;

    #[test]
    fn randomness_and_tipsets_are_stable_and_domain_separated() {
        let externs = DeterministicExterns;

        assert_eq!(
            externs.get_chain_randomness(12).unwrap(),
            externs.get_chain_randomness(12).unwrap()
        );
        assert_ne!(
            externs.get_chain_randomness(12).unwrap(),
            externs.get_beacon_randomness(12).unwrap()
        );
        assert_eq!(externs.get_tipset_cid(12).unwrap(), externs.get_tipset_cid(12).unwrap());
        assert_ne!(externs.get_tipset_cid(12).unwrap(), externs.get_tipset_cid(13).unwrap());
    }
}
