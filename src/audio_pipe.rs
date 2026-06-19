use songbird::input::ChildContainer;
use std::io::{Cursor, ErrorKind, Read};
use std::time::Duration;

/// Reads from the ffmpeg stdout until an Ogg header is available so songbird
/// does not probe an empty pipe (`probe reach EOF at 0 bytes`).
pub struct PrefixedPipe {
    prefix: Cursor<Vec<u8>>,
    pipe: ChildContainer,
}

impl PrefixedPipe {
    pub fn new(prefix: Vec<u8>, pipe: ChildContainer) -> Self {
        Self {
            prefix: Cursor::new(prefix),
            pipe,
        }
    }
}

impl Read for PrefixedPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.prefix.read(buf)?;
        if n > 0 {
            return Ok(n);
        }
        self.pipe.read(buf)
    }
}

pub fn buffer_initial_ogg(container: &mut ChildContainer) -> Result<Vec<u8>, String> {
    buffer_initial_ogg_with_timeout(container, Duration::from_secs(45))
}

pub fn buffer_initial_ogg_with_timeout(
    container: &mut ChildContainer,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    buffer_initial_ogg_with_timeout_min(container, timeout, 4096)
}

pub fn buffer_initial_ogg_with_timeout_min(
    container: &mut ChildContainer,
    timeout: Duration,
    min_bytes: usize,
) -> Result<Vec<u8>, String> {
    let min_bytes = min_bytes.max(4);
    let start = std::time::Instant::now();
    let mut buf = vec![0u8; min_bytes];
    let mut filled = 0usize;

    while filled < min_bytes {
        if start.elapsed() > timeout {
            return Err(format!(
                "Timed out after {}s waiting for audio (no Ogg data on ffmpeg stdout).",
                timeout.as_secs()
            ));
        }

        match container.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled >= 4 && buf[..filled].starts_with(b"OggS") {
                    buf.truncate(filled);
                    return Ok(buf);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(n) => {
                filled += n;
                if filled >= 4 && buf[..4] != *b"OggS" {
                    return Err("ffmpeg pipe did not produce Ogg/Opus output.".into());
                }
                if filled >= min_bytes {
                    buf.truncate(filled);
                    return Ok(buf);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("Error reading ffmpeg pipe: {e}")),
        }
    }

    buf.truncate(filled);
    Ok(buf)
}

/// Read until at least `min_bytes` are available from a live PCM byte stream.
pub fn buffer_initial_bytes_with_timeout_min(
    container: &mut ChildContainer,
    timeout: Duration,
    min_bytes: usize,
) -> Result<Vec<u8>, String> {
    let min_bytes = min_bytes.max(1);
    let start = std::time::Instant::now();
    let mut buf = vec![0u8; min_bytes];
    let mut filled = 0usize;

    while filled < min_bytes {
        if start.elapsed() > timeout {
            return Err(format!(
                "Timed out after {}s waiting for PCM data on ffmpeg stdout.",
                timeout.as_secs()
            ));
        }

        match container.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled > 0 {
                    buf.truncate(filled);
                    return Ok(buf);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(n) => {
                filled += n;
                if filled >= min_bytes {
                    buf.truncate(filled);
                    return Ok(buf);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("Error reading ffmpeg pipe: {e}")),
        }
    }

    buf.truncate(filled);
    Ok(buf)
}
