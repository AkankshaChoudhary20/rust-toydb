//! Binary encoding shared by the WAL and SSTable files.
//!
//! Record layout (little-endian):
//!   op: u8            (1 = PUT, 0 = DELETE)
//!   key_len: u32
//!   key: [u8; key_len]
//!   -- only if op == PUT --
//!   value_len: u32
//!   value: [u8; value_len]

use std::io::{self, Read, Write};

pub const OP_DELETE: u8 = 0;
pub const OP_PUT: u8 = 1;

/// A decoded record: a key, plus `Some(value)` for a put or `None` for a
/// tombstone (delete).
pub type Entry = (Vec<u8>, Option<Vec<u8>>);

pub fn write_record<W: Write>(w: &mut W, key: &[u8], value: Option<&[u8]>) -> io::Result<()> {
    match value {
        Some(v) => {
            w.write_all(&[OP_PUT])?;
            w.write_all(&(key.len() as u32).to_le_bytes())?;
            w.write_all(key)?;
            w.write_all(&(v.len() as u32).to_le_bytes())?;
            w.write_all(v)?;
        }
        None => {
            w.write_all(&[OP_DELETE])?;
            w.write_all(&(key.len() as u32).to_le_bytes())?;
            w.write_all(key)?;
        }
    }
    Ok(())
}

/// Reads one record. Returns `Ok(None)` at a clean end-of-stream, or if the stream
/// ends partway through a record (e.g. a process was killed mid-write) -- in that
/// case the incomplete trailing record is silently discarded, which is the standard
/// crash-recovery behavior for an append-only log.
pub fn read_record<R: Read>(r: &mut R) -> io::Result<Option<Entry>> {
    read_record_inner(r, true)
}

/// Immutable tables must end at a record boundary: a partial record is
/// corruption, not an unacknowledged WAL tail that can be discarded.
pub fn read_record_strict<R: Read>(r: &mut R) -> io::Result<Option<Entry>> {
    read_record_inner(r, false)
}

fn read_record_inner<R: Read>(r: &mut R, allow_incomplete: bool) -> io::Result<Option<Entry>> {
    let mut op_buf = [0u8; 1];
    if !read_exact_or_eof(r, &mut op_buf)? {
        return Ok(None);
    }
    let op = op_buf[0];
    // Reject corrupt operations before interpreting their length fields.
    if op != OP_PUT && op != OP_DELETE {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("corrupt record: unknown op byte {}", op)));
    }

    let mut len_buf = [0u8; 4];
    if !read_field(r, &mut len_buf, allow_incomplete)? {
        return Ok(None);
    }
    let klen = u32::from_le_bytes(len_buf) as usize;
    let mut key = vec![0u8; klen];
    if !read_field(r, &mut key, allow_incomplete)? {
        return Ok(None);
    }

    match op {
        OP_PUT => {
            if !read_field(r, &mut len_buf, allow_incomplete)? {
                return Ok(None);
            }
            let vlen = u32::from_le_bytes(len_buf) as usize;
            let mut value = vec![0u8; vlen];
            if !read_field(r, &mut value, allow_incomplete)? {
                return Ok(None);
            }
            Ok(Some((key, Some(value))))
        }
        OP_DELETE => Ok(Some((key, None))),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("corrupt record: unknown op byte {}", other),
        )),
    }
}

#[cfg(test)]
mod invalid_operation_tests {
    use super::*;

    #[test]
    fn invalid_operation_is_not_silently_treated_as_incomplete_wal() {
        let error = read_record(&mut &[255_u8][..]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = read_record_strict(&mut &[255_u8, 255, 255, 255, 255][..]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    if buf.is_empty() {
        return Ok(true);
    }
    match r.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

fn read_field<R: Read>(r: &mut R, buf: &mut [u8], allow_incomplete: bool) -> io::Result<bool> {
    if allow_incomplete {
        read_exact_or_eof(r, buf)
    } else {
        r.read_exact(buf)?;
        Ok(true)
    }
}
