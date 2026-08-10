//! MVCC row versions and their on-disk chain.
//!
//! Postgres stores each version as an independent heap tuple and lets
//! indexes point at every one of them, which buys cheap in-place updates at
//! the cost of index bloat, HOT chains, and a vacuum that has to walk the
//! heap. Ferrite v1 instead keeps all versions of a row **together**, as a
//! single B-tree payload keyed by `RowId`:
//!
//! ```text
//! u16 version_count            newest first
//! repeated:
//!   u64 xmin                   transaction that created this version
//!   u64 xmax                   transaction that deleted it, 0 if live
//!   u32 len
//!   len bytes                  the encoded Row
//! ```
//!
//! The trade-off is deliberate. Reads resolve visibility with one B-tree
//! descent and no extra page hops, `RowId` stays stable across updates (so
//! the `StorageEngine` contract's "update by RowId" is a direct hit), and
//! reclaiming dead versions is a local rewrite of one payload instead of a
//! separate vacuum pass over the whole table. The cost is that a row
//! updated many times inside one long-lived snapshot window carries all of
//! those versions in a single payload, which the overflow-page machinery
//! handles but does not make cheap. Chains are pruned on every write, so
//! the steady state for an ordinary workload is one or two versions.

use ferrite_common::{FerriteError, TxnId};

use crate::codec::{Reader, Writer};

/// `xmax == NO_TXN` means the version has not been deleted. Transaction
/// ids start at 1, so 0 is free to mean "none".
pub const NO_TXN: TxnId = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub xmin: TxnId,
    pub xmax: TxnId,
    pub bytes: Vec<u8>,
}

impl Version {
    pub fn live(xmin: TxnId, bytes: Vec<u8>) -> Self {
        Self {
            xmin,
            xmax: NO_TXN,
            bytes,
        }
    }
}

pub fn encode_chain(versions: &[Version]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u16(versions.len() as u16);
    for v in versions {
        w.u64(v.xmin);
        w.u64(v.xmax);
        w.len_prefixed(&v.bytes);
    }
    w.finish()
}

pub fn decode_chain(bytes: &[u8]) -> Result<Vec<Version>, FerriteError> {
    let mut r = Reader::new(bytes);
    let count = r.u16()? as usize;
    let mut out = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let xmin = r.u64()?;
        let xmax = r.u64()?;
        let len = r.u32()? as usize;
        out.push(Version {
            xmin,
            xmax,
            bytes: r.take(len)?.to_vec(),
        });
    }
    if !r.is_empty() {
        return Err(FerriteError::Storage(
            "corrupt version chain: trailing bytes".into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_roundtrips() {
        let chain = vec![
            Version {
                xmin: 9,
                xmax: NO_TXN,
                bytes: vec![1, 2, 3],
            },
            Version {
                xmin: 4,
                xmax: 9,
                bytes: vec![],
            },
        ];
        assert_eq!(decode_chain(&encode_chain(&chain)).unwrap(), chain);
    }

    #[test]
    fn empty_chain_roundtrips() {
        assert_eq!(decode_chain(&encode_chain(&[])).unwrap(), Vec::new());
    }

    #[test]
    fn rejects_truncated_chain() {
        let encoded = encode_chain(&[Version::live(1, vec![7; 10])]);
        assert!(decode_chain(&encoded[..encoded.len() - 1]).is_err());
    }
}
