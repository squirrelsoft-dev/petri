//! NBD (Network Block Device) wire protocol: constants and big-endian framing
//! helpers for the fixed-newstyle handshake and the simple-reply transmission
//! phase.
//!
//! This is intentionally a small, self-contained slice of the protocol — only
//! what `petri-nbd` exports and what a guest/NBD client needs to drive a
//! [`crate::LayeredDisk`]. See `docs/petri-nbd-layered-storage.md` §6.
//!
//! The constant table is kept intentionally complete (including a few values
//! used only by the in-crate test client or reserved for later milestones), so
//! `dead_code` is allowed module-wide rather than per-constant.
#![allow(dead_code)]

use std::io::{self, Read, Write};

// --- Handshake magics ---------------------------------------------------------

/// `"NBDMAGIC"` — first 8 bytes the server sends.
pub const INIT_PASSWD: u64 = 0x4e42_444d_4147_4943;
/// `"IHAVEOPT"` — option magic, sent by server in greeting and by client per option.
pub const IHAVEOPT: u64 = 0x4948_4156_454f_5054;
/// Magic prefixing every server reply to a client option.
pub const REP_MAGIC: u64 = 0x0003_e889_0455_65a9;

// --- Handshake flags (server -> client, u16) ---------------------------------

pub const FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
pub const FLAG_NO_ZEROES: u16 = 1 << 1;

// --- Client handshake flags (client -> server, u32) --------------------------

pub const FLAG_C_FIXED_NEWSTYLE: u32 = 1 << 0;
pub const FLAG_C_NO_ZEROES: u32 = 1 << 1;

// --- Options (client -> server, u32) -----------------------------------------

pub const OPT_EXPORT_NAME: u32 = 1;
pub const OPT_ABORT: u32 = 2;
pub const OPT_LIST: u32 = 3;
pub const OPT_INFO: u32 = 6;
pub const OPT_GO: u32 = 7;

// --- Option replies (server -> client, u32) ----------------------------------

pub const REP_ACK: u32 = 1;
pub const REP_INFO: u32 = 3;
pub const REP_ERR_UNSUP: u32 = 0x8000_0001;
pub const REP_ERR_INVALID: u32 = 0x8000_0003;

/// Information type carried in an `NBD_REP_INFO` payload.
pub const INFO_EXPORT: u16 = 0;

// --- Transmission flags (per-export, u16) ------------------------------------

pub const FLAG_HAS_FLAGS: u16 = 1 << 0;
pub const FLAG_READ_ONLY: u16 = 1 << 1;
pub const FLAG_SEND_FLUSH: u16 = 1 << 2;
pub const FLAG_SEND_TRIM: u16 = 1 << 5;
pub const FLAG_SEND_WRITE_ZEROES: u16 = 1 << 6;

// --- Transmission phase ------------------------------------------------------

/// Magic prefixing every client transmission request.
pub const REQUEST_MAGIC: u32 = 0x2560_9513;
/// Magic prefixing every server simple reply.
pub const SIMPLE_REPLY_MAGIC: u32 = 0x6744_6698;

// Command types (low 16 bits of the request `flags`+`type` field).
pub const CMD_READ: u16 = 0;
pub const CMD_WRITE: u16 = 1;
pub const CMD_DISC: u16 = 2;
pub const CMD_FLUSH: u16 = 3;
pub const CMD_TRIM: u16 = 4;
pub const CMD_WRITE_ZEROES: u16 = 6;

/// Command flag: Force Unit Access — persist this write before replying.
pub const CMD_FLAG_FUA: u16 = 1 << 0;

// NBD errno values used in replies (subset of standard errno).
pub const EPERM: u32 = 1;
pub const EIO: u32 = 5;
pub const EINVAL: u32 = 22;
pub const ENOSPC: u32 = 28;

/// A decoded transmission request header.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub flags: u16,
    pub cmd: u16,
    pub handle: u64,
    pub offset: u64,
    pub length: u32,
}

// --- Big-endian framing helpers ----------------------------------------------

pub fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

pub fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

pub fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

pub fn write_u16(w: &mut impl Write, v: u16) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Read a transmission request header (the 28-byte fixed prefix).
pub fn read_request(r: &mut impl Read) -> io::Result<Request> {
    let magic = read_u32(r)?;
    if magic != REQUEST_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad NBD request magic",
        ));
    }
    let flags = read_u16(r)?;
    let cmd = read_u16(r)?;
    let handle = read_u64(r)?;
    let offset = read_u64(r)?;
    let length = read_u32(r)?;
    Ok(Request {
        flags,
        cmd,
        handle,
        offset,
        length,
    })
}

/// Write a simple-reply header. For reads, the payload follows immediately.
pub fn write_simple_reply(w: &mut impl Write, error: u32, handle: u64) -> io::Result<()> {
    write_u32(w, SIMPLE_REPLY_MAGIC)?;
    write_u32(w, error)?;
    write_u64(w, handle)
}
