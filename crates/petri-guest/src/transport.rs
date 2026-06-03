use std::io;

#[derive(Debug, Clone, Copy)]
pub struct VsockListenerConfig {
    pub port: u32,
}

impl VsockListenerConfig {
    pub fn bind(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "vsock listener scaffold is present, but binding port {} is not implemented yet",
                self.port
            ),
        ))
    }
}
