//! Bounded second-pass redaction when publishing an observed spool prefix.
use std::io::{Read, Result};
pub(crate) struct RedactedReader<R> {
    source: R,
    filter: excalibur_vault::StreamRedactor,
    pending: std::io::Cursor<Vec<u8>>,
    finished: bool,
}
impl<R: Read> RedactedReader<R> {
    pub(crate) fn new(source: R, redactor: &excalibur_vault::Redactor) -> Self {
        Self {
            source,
            filter: redactor.stream(),
            pending: std::io::Cursor::new(Vec::new()),
            finished: false,
        }
    }
}
impl<R: Read> Read for RedactedReader<R> {
    fn read(&mut self, output: &mut [u8]) -> Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.pending.read(output)?;
            if n > 0 || self.finished {
                return Ok(n);
            }
            let mut chunk = [0u8; 64 * 1024];
            let n = self.source.read(&mut chunk)?;
            let bytes = if n == 0 {
                self.finished = true;
                self.filter.finish()
            } else {
                self.filter.push(&chunk[..n])
            };
            self.pending = std::io::Cursor::new(bytes);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_filters_newly_registered_values_across_spool_chunks_without_losing_safe_bytes() {
        let value =
            excalibur_vault::SecretValue::new(b"synthetic-spooled-credential-59021".to_vec())
                .unwrap();
        let redactor = excalibur_vault::Redactor::default();
        let mut raw = vec![b'x'; 65530];
        raw.extend_from_slice(b"synthetic-spooled-credential-59021");
        raw.extend(vec![b'y'; 2 * 1024 * 1024]);
        // The spool predates registering this value; publication must use the live registry again.
        let file = tempfile::tempfile().unwrap();
        use std::io::{Seek, SeekFrom, Write};
        let mut file = file;
        file.write_all(&raw).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        redactor.register_secret(&value);
        let mut output = Vec::new();
        RedactedReader::new(file, &redactor)
            .read_to_end(&mut output)
            .unwrap();
        let mut expected = vec![b'x'; 65530];
        expected.extend_from_slice(b"[REDACTED]");
        expected.extend(vec![b'y'; 2 * 1024 * 1024]);
        assert_eq!(output, expected);
    }
}
