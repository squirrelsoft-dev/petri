use std::io;

#[derive(Debug, Clone, Copy)]
pub struct VsockListenerConfig {
    pub port: u32,
}

#[cfg(target_os = "linux")]
pub struct VsockListener {
    inner: vsock::VsockListener,
}

#[cfg(target_os = "linux")]
impl VsockListenerConfig {
    pub fn bind(&self) -> io::Result<VsockListener> {
        let inner = vsock::VsockListener::bind_with_cid_port(vsock::VMADDR_CID_ANY, self.port)?;
        Ok(VsockListener { inner })
    }
}

#[cfg(target_os = "linux")]
impl VsockListener {
    pub fn incoming(&self) -> vsock::Incoming<'_> {
        self.inner.incoming()
    }
}

#[cfg(not(target_os = "linux"))]
pub struct VsockListener;

#[cfg(not(target_os = "linux"))]
impl VsockListenerConfig {
    pub fn bind(&self) -> io::Result<VsockListener> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "vsock listener scaffold is present, but binding port {} requires Linux",
                self.port
            ),
        ))
    }
}
