//! A minimal, synchronous, thread-per-connection NBD server that exports a
//! [`LayeredDisk`] over localhost (loopback TCP or a Unix socket).
//!
//! Scope is deliberately small (design §6.2): the fixed-newstyle handshake
//! (`EXPORT_NAME` and `GO`) plus simple-reply `READ` / `WRITE` / `FLUSH` /
//! `DISC`, with optional `WRITE_ZEROES` / `TRIM`. The composed disk is shared
//! behind a `Mutex`; one block export serves one client at a time.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::layer::{ImmutableLayer, LayerId};
use crate::protocol::*;
use crate::stack::LayeredDisk;

/// Largest single request body we will buffer, to bound memory on malformed or
/// hostile input. Requests above this are rejected with `EINVAL`.
const MAX_REQUEST_LEN: u32 = 64 * 1024 * 1024;

/// How long the accept loop blocks between shutdown checks.
const ACCEPT_POLL: Duration = Duration::from_millis(5);

/// Where to bind the export.
pub enum BindMode {
    /// Loopback TCP on `127.0.0.1:<port>`. Port `0` lets the OS choose; the
    /// chosen port is reflected in [`NbdHandle::url`].
    LoopbackTcp(u16),
    /// A Unix domain socket at the given path (recreated if it exists).
    UnixSocket(PathBuf),
}

/// Options for [`NbdServer::serve`].
pub struct ServeOpts {
    pub bind: BindMode,
    pub export_name: String,
    /// Advertise the export read-only and reject all mutating commands.
    pub read_only: bool,
}

impl ServeOpts {
    /// Loopback TCP on an OS-chosen port with a default export name.
    pub fn loopback() -> Self {
        Self {
            bind: BindMode::LoopbackTcp(0),
            export_name: "petri".to_string(),
            read_only: false,
        }
    }
}

/// Entry point namespace for serving a [`LayeredDisk`] over NBD.
pub struct NbdServer;

impl NbdServer {
    /// Bind the endpoint and start serving `disk` until [`NbdHandle::shutdown`].
    pub fn serve(disk: LayeredDisk, opts: ServeOpts) -> io::Result<NbdHandle> {
        let disk = Arc::new(Mutex::new(disk));
        let disk_handle = disk.clone();
        let running = Arc::new(AtomicBool::new(true));
        let export_name = Arc::new(opts.export_name);
        let read_only = opts.read_only;

        match opts.bind {
            BindMode::LoopbackTcp(port) => {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
                listener.set_nonblocking(true)?;
                let bound = listener.local_addr()?;
                let url = format!("nbd://127.0.0.1:{}/{}", bound.port(), export_name);
                let accept =
                    spawn_tcp_accept(listener, disk, running.clone(), export_name, read_only);
                Ok(NbdHandle {
                    url,
                    running,
                    accept: Some(accept),
                    unix_path: None,
                    disk: disk_handle,
                })
            }
            BindMode::UnixSocket(path) => {
                let _ = std::fs::remove_file(&path);
                let listener = UnixListener::bind(&path)?;
                listener.set_nonblocking(true)?;
                // Canonical NBD URI form: export in the path, socket as a query
                // param (https://github.com/NetworkBlockDevice/nbd .../uri.md).
                let url = format!("nbd+unix:///{}?socket={}", export_name, path.display());
                let accept =
                    spawn_unix_accept(listener, disk, running.clone(), export_name, read_only);
                Ok(NbdHandle {
                    url,
                    running,
                    accept: Some(accept),
                    unix_path: Some(path),
                    disk: disk_handle,
                })
            }
        }
    }
}

/// Handle to a running export. Dropping it shuts the server down.
pub struct NbdHandle {
    url: String,
    running: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    unix_path: Option<PathBuf>,
    disk: Arc<Mutex<LayeredDisk>>,
}

impl NbdHandle {
    /// The URL a client (or the AVF helper) uses to attach this export.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Seal the served stack's scratch overlay into an immutable layer under
    /// `dir`, without interrupting the export (the scratch stays live). Use
    /// after the guest has quiesced its writes (e.g. VM stopped) for a
    /// consistent snapshot.
    pub fn seal_scratch(&self, dir: &Path, parents: &[LayerId]) -> io::Result<ImmutableLayer> {
        self.disk
            .lock()
            .expect("disk mutex poisoned")
            .seal_scratch(dir, parents)
    }

    /// Stop accepting connections and join the accept loop.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.stop();
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        if let Some(path) = self.unix_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for NbdHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_tcp_accept(
    listener: TcpListener,
    disk: Arc<Mutex<LayeredDisk>>,
    running: Arc<AtomicBool>,
    export_name: Arc<String>,
    read_only: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    spawn_worker(stream, &disk, &export_name, read_only);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                Err(_) => break,
            }
        }
    })
}

fn spawn_unix_accept(
    listener: UnixListener,
    disk: Arc<Mutex<LayeredDisk>>,
    running: Arc<AtomicBool>,
    export_name: Arc<String>,
    read_only: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    spawn_worker(stream, &disk, &export_name, read_only);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
                Err(_) => break,
            }
        }
    })
}

fn spawn_worker<S>(
    stream: S,
    disk: &Arc<Mutex<LayeredDisk>>,
    export_name: &Arc<String>,
    read_only: bool,
) where
    S: Read + Write + Send + 'static,
{
    let disk = disk.clone();
    let export_name = export_name.clone();
    thread::spawn(move || {
        let _ = serve_connection(stream, &disk, &export_name, read_only);
    });
}

/// Drive one client connection from handshake through disconnect.
fn serve_connection<S: Read + Write>(
    mut stream: S,
    disk: &Mutex<LayeredDisk>,
    _export_name: &str,
    read_only: bool,
) -> io::Result<()> {
    let (vsize, xflags) = {
        let d = disk.lock().expect("disk mutex poisoned");
        (d.virtual_size(), export_flags(read_only))
    };

    // --- Fixed-newstyle greeting ---
    write_u64(&mut stream, INIT_PASSWD)?;
    write_u64(&mut stream, IHAVEOPT)?;
    write_u16(&mut stream, FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES)?;
    stream.flush()?;

    let client_flags = read_u32(&mut stream)?;
    let no_zeroes = client_flags & FLAG_C_NO_ZEROES != 0;

    // --- Option haggling ---
    loop {
        let magic = read_u64(&mut stream)?;
        if magic != IHAVEOPT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad option magic",
            ));
        }
        let opt = read_u32(&mut stream)?;
        let len = read_u32(&mut stream)?;
        let mut data = vec![0u8; len as usize];
        stream.read_exact(&mut data)?;

        match opt {
            OPT_EXPORT_NAME => {
                // Legacy: reply with the export tuple and go straight to I/O.
                write_u64(&mut stream, vsize)?;
                write_u16(&mut stream, xflags)?;
                if !no_zeroes {
                    stream.write_all(&[0u8; 124])?;
                }
                stream.flush()?;
                return transmission(&mut stream, disk, vsize, read_only);
            }
            OPT_GO | OPT_INFO => {
                let mut info = Vec::with_capacity(12);
                write_u16(&mut info, INFO_EXPORT)?;
                write_u64(&mut info, vsize)?;
                write_u16(&mut info, xflags)?;
                write_opt_reply(&mut stream, opt, REP_INFO, &info)?;
                write_opt_reply(&mut stream, opt, REP_ACK, &[])?;
                stream.flush()?;
                if opt == OPT_GO {
                    return transmission(&mut stream, disk, vsize, read_only);
                }
                // OPT_INFO: keep negotiating.
            }
            OPT_ABORT => {
                write_opt_reply(&mut stream, opt, REP_ACK, &[])?;
                stream.flush()?;
                return Ok(());
            }
            _ => {
                write_opt_reply(&mut stream, opt, REP_ERR_UNSUP, &[])?;
                stream.flush()?;
            }
        }
    }
}

/// Transmission phase: serve commands until disconnect or EOF.
fn transmission<S: Read + Write>(
    stream: &mut S,
    disk: &Mutex<LayeredDisk>,
    vsize: u64,
    read_only: bool,
) -> io::Result<()> {
    loop {
        let req = match read_request(stream) {
            Ok(req) => req,
            // A clean client disconnect (socket close) ends the loop.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };

        match req.cmd {
            CMD_DISC => return Ok(()),

            CMD_READ => {
                if req.length > MAX_REQUEST_LEN || out_of_bounds(req.offset, req.length, vsize) {
                    write_simple_reply(stream, EINVAL, req.handle)?;
                    stream.flush()?;
                    continue;
                }
                let mut buf = vec![0u8; req.length as usize];
                let res = disk
                    .lock()
                    .expect("disk mutex poisoned")
                    .read_at(req.offset, &mut buf);
                match res {
                    Ok(()) => {
                        write_simple_reply(stream, 0, req.handle)?;
                        stream.write_all(&buf)?;
                    }
                    Err(_) => write_simple_reply(stream, EIO, req.handle)?,
                }
            }

            CMD_WRITE => {
                // Payload must be drained to keep framing intact even on error.
                if req.length > MAX_REQUEST_LEN {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "write too large",
                    ));
                }
                let mut buf = vec![0u8; req.length as usize];
                stream.read_exact(&mut buf)?;
                let err = if read_only {
                    EPERM
                } else if out_of_bounds(req.offset, req.length, vsize) {
                    EINVAL
                } else {
                    let mut d = disk.lock().expect("disk mutex poisoned");
                    match d.write_at(req.offset, &buf) {
                        Ok(()) => {
                            if req.flags & CMD_FLAG_FUA != 0 && d.flush().is_err() {
                                EIO
                            } else {
                                0
                            }
                        }
                        Err(_) => EIO,
                    }
                };
                write_simple_reply(stream, err, req.handle)?;
            }

            CMD_FLUSH => {
                let err = match disk.lock().expect("disk mutex poisoned").flush() {
                    Ok(()) => 0,
                    Err(_) => EIO,
                };
                write_simple_reply(stream, err, req.handle)?;
            }

            CMD_TRIM => {
                let err = if read_only {
                    EPERM
                } else if out_of_bounds(req.offset, req.length, vsize) {
                    EINVAL
                } else {
                    match disk
                        .lock()
                        .expect("disk mutex poisoned")
                        .trim(req.offset, u64::from(req.length))
                    {
                        Ok(()) => 0,
                        Err(_) => EIO,
                    }
                };
                write_simple_reply(stream, err, req.handle)?;
            }

            CMD_WRITE_ZEROES => {
                let err = if read_only {
                    EPERM
                } else if out_of_bounds(req.offset, req.length, vsize) {
                    EINVAL
                } else {
                    match disk
                        .lock()
                        .expect("disk mutex poisoned")
                        .write_zeroes(req.offset, u64::from(req.length))
                    {
                        Ok(()) => 0,
                        Err(_) => EIO,
                    }
                };
                write_simple_reply(stream, err, req.handle)?;
            }

            _ => write_simple_reply(stream, EINVAL, req.handle)?,
        }
        stream.flush()?;
    }
}

fn export_flags(read_only: bool) -> u16 {
    let mut flags = FLAG_HAS_FLAGS | FLAG_SEND_FLUSH | FLAG_SEND_TRIM | FLAG_SEND_WRITE_ZEROES;
    if read_only {
        flags |= FLAG_READ_ONLY;
    }
    flags
}

fn out_of_bounds(offset: u64, length: u32, vsize: u64) -> bool {
    match offset.checked_add(u64::from(length)) {
        Some(end) => end > vsize,
        None => true,
    }
}

fn write_opt_reply(w: &mut impl Write, opt: u32, rep_type: u32, data: &[u8]) -> io::Result<()> {
    // The option reply header carries the payload length as u32. Every payload
    // this server builds is small (export names and info records), but encode
    // the bound rather than truncating silently: a wrapped length would frame
    // the reply wrongly and desynchronize the client's option stream.
    let data_len = u32::try_from(data.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("option reply payload of {} bytes exceeds u32", data.len()),
        )
    })?;
    write_u64(w, REP_MAGIC)?;
    write_u32(w, opt)?;
    write_u32(w, rep_type)?;
    write_u32(w, data_len)?;
    w.write_all(data)
}

#[cfg(test)]
// Tests build offsets from the small `BS` block-size constant and frame
// protocol messages from fixed literals, so the conversions here are
// scaffolding rather than production arithmetic.
#[allow(clippy::cast_lossless, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::stack::Geometry;
    use crate::{ImmutableLayer, ScratchLayer};
    use std::fs;
    use std::net::TcpStream;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::atomic::AtomicU64;

    const BS: u32 = 16;
    const VSIZE: u64 = 160; // 10 blocks

    fn unique_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("petri-nbd-srv-{}-{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn geometry() -> Geometry {
        Geometry::new(VSIZE, BS).unwrap()
    }

    /// Base image covering 5 blocks with byte value `0xB0 + block`.
    fn base_disk(dir: &Path) -> LayeredDisk {
        let base_path = dir.join("base.raw");
        let mut bytes = Vec::new();
        for block in 0u8..5 {
            bytes.extend(std::iter::repeat_n(0xB0 + block, BS as usize));
        }
        fs::write(&base_path, &bytes).unwrap();
        let base = ImmutableLayer::open_raw_base(&base_path, geometry()).unwrap();
        let scratch = ScratchLayer::create(&dir.join("scratch.data"), geometry()).unwrap();
        LayeredDisk::new(vec![base], scratch).unwrap()
    }

    /// In-tree NBD client speaking just enough of the protocol to drive tests.
    struct Client<S: Read + Write>(S);

    impl<S: Read + Write> Client<S> {
        /// Connect via the legacy `EXPORT_NAME` path and return (client, size).
        fn export_name(mut s: S) -> io::Result<(Self, u64)> {
            assert_eq!(read_u64(&mut s)?, INIT_PASSWD);
            assert_eq!(read_u64(&mut s)?, IHAVEOPT);
            let _hs_flags = read_u16(&mut s)?;
            write_u32(&mut s, FLAG_C_FIXED_NEWSTYLE)?; // not NO_ZEROES → expect padding
            write_u64(&mut s, IHAVEOPT)?;
            write_u32(&mut s, OPT_EXPORT_NAME)?;
            let name = b"petri";
            write_u32(&mut s, name.len() as u32)?;
            s.write_all(name)?;
            s.flush()?;
            let size = read_u64(&mut s)?;
            let _flags = read_u16(&mut s)?;
            let mut pad = [0u8; 124];
            s.read_exact(&mut pad)?;
            Ok((Client(s), size))
        }

        /// Connect via the modern `GO` path and return (client, size).
        fn go(mut s: S) -> io::Result<(Self, u64)> {
            assert_eq!(read_u64(&mut s)?, INIT_PASSWD);
            assert_eq!(read_u64(&mut s)?, IHAVEOPT);
            let _hs_flags = read_u16(&mut s)?;
            write_u32(&mut s, FLAG_C_FIXED_NEWSTYLE)?;
            // OPT_GO data: u32 name_len + name + u16 n_info(0)
            let name = b"petri";
            let mut data = Vec::new();
            write_u32(&mut data, name.len() as u32)?;
            data.extend_from_slice(name);
            write_u16(&mut data, 0)?;
            write_u64(&mut s, IHAVEOPT)?;
            write_u32(&mut s, OPT_GO)?;
            write_u32(&mut s, data.len() as u32)?;
            s.write_all(&data)?;
            s.flush()?;
            // Read option replies until ACK.
            let mut size = 0u64;
            loop {
                assert_eq!(read_u64(&mut s)?, REP_MAGIC);
                let _opt = read_u32(&mut s)?;
                let rep_type = read_u32(&mut s)?;
                let len = read_u32(&mut s)?;
                let mut payload = vec![0u8; len as usize];
                s.read_exact(&mut payload)?;
                if rep_type == REP_INFO {
                    // u16 info_type + u64 size + u16 flags
                    size = u64::from_be_bytes(payload[2..10].try_into().unwrap());
                } else if rep_type == REP_ACK {
                    break;
                }
            }
            Ok((Client(s), size))
        }

        fn command(
            &mut self,
            cmd: u16,
            flags: u16,
            offset: u64,
            length: u32,
            payload: &[u8],
        ) -> io::Result<(u32, Vec<u8>)> {
            write_u32(&mut self.0, REQUEST_MAGIC)?;
            write_u16(&mut self.0, flags)?;
            write_u16(&mut self.0, cmd)?;
            write_u64(&mut self.0, 0xfeed)?; // handle (echoed back)
            write_u64(&mut self.0, offset)?;
            write_u32(&mut self.0, length)?;
            self.0.write_all(payload)?;
            self.0.flush()?;
            assert_eq!(read_u32(&mut self.0)?, SIMPLE_REPLY_MAGIC);
            let error = read_u32(&mut self.0)?;
            assert_eq!(read_u64(&mut self.0)?, 0xfeed);
            let mut data = Vec::new();
            if cmd == CMD_READ && error == 0 {
                data = vec![0u8; length as usize];
                self.0.read_exact(&mut data)?;
            }
            Ok((error, data))
        }

        fn read(&mut self, offset: u64, length: u32) -> io::Result<(u32, Vec<u8>)> {
            self.command(CMD_READ, 0, offset, length, &[])
        }
        fn write(&mut self, offset: u64, payload: &[u8]) -> io::Result<u32> {
            Ok(self
                .command(CMD_WRITE, 0, offset, payload.len() as u32, payload)?
                .0)
        }
        fn disconnect(&mut self) -> io::Result<()> {
            write_u32(&mut self.0, REQUEST_MAGIC)?;
            write_u16(&mut self.0, 0)?;
            write_u16(&mut self.0, CMD_DISC)?;
            write_u64(&mut self.0, 0)?;
            write_u64(&mut self.0, 0)?;
            write_u32(&mut self.0, 0)?;
            self.0.flush()
        }
    }

    fn serve_tcp(dir: &Path, read_only: bool) -> NbdHandle {
        NbdServer::serve(
            base_disk(dir),
            ServeOpts {
                bind: BindMode::LoopbackTcp(0),
                export_name: "petri".into(),
                read_only,
            },
        )
        .unwrap()
    }

    fn connect(url: &str) -> TcpStream {
        // url = nbd://127.0.0.1:<port>/petri
        let addr = url.trim_start_matches("nbd://").split('/').next().unwrap();
        TcpStream::connect(addr).unwrap()
    }

    #[test]
    fn export_name_read_write_roundtrip() {
        let dir = unique_dir();
        let server = serve_tcp(&dir, false);
        let (mut c, size) = Client::export_name(connect(server.url())).unwrap();
        assert_eq!(size, VSIZE);

        // Base shows through.
        let (err, data) = c.read(0, BS).unwrap();
        assert_eq!(err, 0);
        assert_eq!(data, vec![0xB0; BS as usize]);

        // Write to scratch, read it back.
        assert_eq!(c.write(BS as u64, &vec![0xAA; BS as usize]).unwrap(), 0);
        let (_, data) = c.read(BS as u64, BS).unwrap();
        assert_eq!(data, vec![0xAA; BS as usize]);

        // Flush succeeds.
        assert_eq!(c.command(CMD_FLUSH, 0, 0, 0, &[]).unwrap().0, 0);
        c.disconnect().unwrap();
        server.shutdown().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn go_handshake_advertises_size() {
        let dir = unique_dir();
        let server = serve_tcp(&dir, false);
        let (mut c, size) = Client::go(connect(server.url())).unwrap();
        assert_eq!(size, VSIZE);
        let (err, data) = c.read(4 * BS as u64, BS).unwrap();
        assert_eq!(err, 0);
        assert_eq!(data, vec![0xB4; BS as usize]);
        c.disconnect().unwrap();
        server.shutdown().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_export_rejects_writes() {
        let dir = unique_dir();
        let server = serve_tcp(&dir, true);
        let (mut c, _) = Client::export_name(connect(server.url())).unwrap();
        let err = c.write(0, &vec![0x11; BS as usize]).unwrap();
        assert_eq!(err, EPERM);
        // Reads still work.
        let (err, data) = c.read(0, BS).unwrap();
        assert_eq!(err, 0);
        assert_eq!(data, vec![0xB0; BS as usize]);
        c.disconnect().unwrap();
        server.shutdown().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_bounds_read_is_rejected() {
        let dir = unique_dir();
        let server = serve_tcp(&dir, false);
        let (mut c, _) = Client::export_name(connect(server.url())).unwrap();
        let (err, _) = c.read(VSIZE, BS).unwrap();
        assert_eq!(err, EINVAL);
        c.disconnect().unwrap();
        server.shutdown().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unix_socket_export_serves() {
        let dir = unique_dir();
        let sock = dir.join("nbd.sock");
        let server = NbdServer::serve(
            base_disk(&dir),
            ServeOpts {
                bind: BindMode::UnixSocket(sock.clone()),
                export_name: "petri".into(),
                read_only: false,
            },
        )
        .unwrap();
        assert!(server.url().contains("nbd+unix"));
        let stream = UnixStream::connect(&sock).unwrap();
        let (mut c, size) = Client::export_name(stream).unwrap();
        assert_eq!(size, VSIZE);
        let (err, data) = c.read(2 * BS as u64, BS).unwrap();
        assert_eq!(err, 0);
        assert_eq!(data, vec![0xB2; BS as usize]);
        c.disconnect().unwrap();
        server.shutdown().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
