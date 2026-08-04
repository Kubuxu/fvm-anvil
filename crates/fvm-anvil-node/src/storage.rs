use anyhow::Result;
use cid::Cid;
use fvm_anvil_state::ForestCar;
use fvm_ipld_blockstore::{Blockstore, MemoryBlockstore};

/// Writable runtime blocks layered over an optional read-only prepared archive.
pub(crate) struct RuntimeBlockstore {
    overlay: MemoryBlockstore,
    base: Option<ForestCar>,
}

impl RuntimeBlockstore {
    pub(crate) fn empty() -> Self {
        Self { overlay: MemoryBlockstore::default(), base: None }
    }

    pub(crate) fn over(base: ForestCar) -> Self {
        Self { overlay: MemoryBlockstore::default(), base: Some(base) }
    }
}

impl Blockstore for RuntimeBlockstore {
    fn get(&self, cid: &Cid) -> Result<Option<Vec<u8>>> {
        match self.overlay.get(cid)? {
            Some(block) => Ok(Some(block)),
            None => match &self.base {
                Some(base) => base.get(cid),
                None => Ok(None),
            },
        }
    }

    fn put_keyed(&self, cid: &Cid, block: &[u8]) -> Result<()> {
        self.overlay.put_keyed(cid, block)
    }
}
