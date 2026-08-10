//! Crash-recovery journal.
//!
//! # Why full-page redo images
//!
//! `docs/architecture.md` leaves the journal format to this crate. Ferrite
//! v1 uses a *physical redo journal*: every page a transaction dirtied is
//! appended verbatim, followed by a commit record, and the file is fsynced
//! before `commit` returns. Recovery replays the images in order over the
//! data file and then resolves transaction outcomes from the commit
//! records.
//!
//! The alternative — physiological logging, where each record describes an
//! operation on a page rather than the page itself — writes far less, but
//! needs per-page LSN comparison during replay, idempotent redo for every
//! structural B-tree operation, and a torn-page defence anyway (Postgres
//! writes full pages after each checkpoint for exactly that reason). Full
//! images are correct by construction: replaying an image twice is the same
//! as replaying it once, and a page that was half-written by the operating
//! system is overwritten wholesale rather than patched. The cost is write
//! amplification, which v1 accepts and the README records as the first
//! thing to revisit.
//!
//! Uncommitted work is allowed to reach both the journal and the data file.
//! Nothing has to be undone at recovery because visibility is decided from
//! the commit-status bitmap (`clog`): a transaction that never wrote a
//! commit record is treated as aborted, so its row versions are simply
//! invisible and get reclaimed by the next pruning pass.
//!
//! # Record framing
//!
//! ```text
//! u32 payload_len
//! u32 crc32c(payload)
//! payload:
//!   u8 kind
//!   kind-specific bytes
//! ```
//!
//! A partially written tail — the normal outcome of losing power mid-append
//! — fails either the length check or the CRC, and recovery stops there.
//! Records before the tear are unaffected because each one is independently
//! checksummed.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use ferrite_common::{FerriteError, TxnId};

use crate::codec::{Reader, Writer};
use crate::crc::crc32c;
use crate::page::{PageId, PAGE_SIZE};

const KIND_PAGE_IMAGE: u8 = 1;
const KIND_COMMIT: u8 = 2;
const KIND_ABORT: u8 = 3;
const KIND_CHECKPOINT: u8 = 4;

fn io_err(context: &str, e: std::io::Error) -> FerriteError {
    FerriteError::Storage(format!("{context}: {e}"))
}

/// A decoded journal record.
pub enum Record {
    PageImage {
        lsn: u64,
        page_id: PageId,
        bytes: Box<[u8; PAGE_SIZE]>,
    },
    Commit(TxnId),
    Abort(TxnId),
    /// All pages up to this point are known to be in the data file.
    Checkpoint,
}

pub struct Journal {
    file: File,
    /// Monotonic sequence stamped on page images; also written into each
    /// page header so a page can be traced back to the record that
    /// produced it.
    next_lsn: u64,
    sync_on_commit: bool,
}

impl Journal {
    pub fn open(path: &Path, sync_on_commit: bool) -> Result<Self, FerriteError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| io_err("opening journal", e))?;
        Ok(Self {
            file,
            next_lsn: 1,
            sync_on_commit,
        })
    }

    pub fn next_lsn(&mut self) -> u64 {
        let lsn = self.next_lsn;
        self.next_lsn += 1;
        lsn
    }

    pub fn set_next_lsn(&mut self, lsn: u64) {
        self.next_lsn = self.next_lsn.max(lsn);
    }

    fn append(&mut self, payload: &[u8]) -> Result<(), FerriteError> {
        let mut framed = Vec::with_capacity(payload.len() + 8);
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&crc32c(payload).to_le_bytes());
        framed.extend_from_slice(payload);
        self.file
            .write_all(&framed)
            .map_err(|e| io_err("appending to journal", e))
    }

    pub fn log_page_image(
        &mut self,
        lsn: u64,
        page_id: PageId,
        bytes: &[u8; PAGE_SIZE],
    ) -> Result<(), FerriteError> {
        let mut w = Writer::new();
        w.u8(KIND_PAGE_IMAGE);
        w.u64(lsn);
        w.u32(page_id);
        w.bytes(bytes);
        self.append(&w.finish())
    }

    pub fn log_commit(&mut self, txn: TxnId) -> Result<(), FerriteError> {
        let mut w = Writer::new();
        w.u8(KIND_COMMIT);
        w.u64(txn);
        self.append(&w.finish())
    }

    pub fn log_abort(&mut self, txn: TxnId) -> Result<(), FerriteError> {
        let mut w = Writer::new();
        w.u8(KIND_ABORT);
        w.u64(txn);
        self.append(&w.finish())
    }

    pub fn log_checkpoint(&mut self) -> Result<(), FerriteError> {
        let mut w = Writer::new();
        w.u8(KIND_CHECKPOINT);
        self.append(&w.finish())
    }

    /// Makes everything appended so far durable. Honouring
    /// `sync_on_commit == false` is a test/bench affordance only; the
    /// engine defaults to syncing.
    pub fn sync(&mut self) -> Result<(), FerriteError> {
        self.file
            .flush()
            .map_err(|e| io_err("flushing journal", e))?;
        if self.sync_on_commit {
            self.file
                .sync_data()
                .map_err(|e| io_err("syncing journal", e))?;
        }
        Ok(())
    }

    /// Discards the journal after a checkpoint has made the data file
    /// self-sufficient.
    pub fn truncate(&mut self) -> Result<(), FerriteError> {
        self.file
            .set_len(0)
            .map_err(|e| io_err("truncating journal", e))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|e| io_err("rewinding journal", e))?;
        self.file
            .sync_data()
            .map_err(|e| io_err("syncing truncated journal", e))?;
        Ok(())
    }

    /// Streams every intact record from the start of the journal, stopping
    /// at the first torn or corrupt record. Returns the number of records
    /// that were skipped because of a tear, which is zero for a clean
    /// shutdown and at most one for a power loss during append.
    pub fn replay(
        path: &Path,
        mut visit: impl FnMut(Record) -> Result<(), FerriteError>,
    ) -> Result<bool, FerriteError> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_err("opening journal for replay", e)),
        };
        let mut reader = BufReader::new(file);
        let mut header = [0u8; 8];
        let mut torn = false;
        loop {
            if !read_exact_or_eof(&mut reader, &mut header)? {
                break;
            }
            let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let expected_crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            if len == 0 || len > PAGE_SIZE + 64 {
                torn = true;
                break;
            }
            let mut payload = vec![0u8; len];
            if !read_exact_or_eof(&mut reader, &mut payload)? {
                torn = true;
                break;
            }
            if crc32c(&payload) != expected_crc {
                torn = true;
                break;
            }
            match decode(&payload)? {
                Some(record) => visit(record)?,
                None => {
                    torn = true;
                    break;
                }
            }
        }
        Ok(torn)
    }
}

fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> Result<bool, FerriteError> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err("reading journal", e)),
        }
    }
    Ok(true)
}

fn decode(payload: &[u8]) -> Result<Option<Record>, FerriteError> {
    let mut r = Reader::new(payload);
    let kind = r.u8()?;
    Ok(match kind {
        KIND_PAGE_IMAGE => {
            let lsn = r.u64()?;
            let page_id = r.u32()?;
            let raw = r.take(PAGE_SIZE)?;
            let mut bytes = Box::new([0u8; PAGE_SIZE]);
            bytes.copy_from_slice(raw);
            Some(Record::PageImage {
                lsn,
                page_id,
                bytes,
            })
        }
        KIND_COMMIT => Some(Record::Commit(r.u64()?)),
        KIND_ABORT => Some(Record::Abort(r.u64()?)),
        KIND_CHECKPOINT => Some(Record::Checkpoint),
        // An unknown kind can only come from a journal written by another
        // build; treat it as the end of what this build understands rather
        // than guessing at the bytes that follow.
        _ => None,
    })
}
