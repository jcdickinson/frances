use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::timeout;

use crate::proto::{Sentinel, SentinelMatch};

const READ_CHUNK: usize = 4096;

enum ReadEvent {
    Read(usize),
    Timer,
}

#[derive(Debug)]
pub enum ReadOutcome {
    Done {
        output: Vec<u8>,
        exit_code: i32,
    },
    Quiet {
        output: Vec<u8>,
        reason: QuietReason,
    },
    Eof {
        output: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuietReason {
    NoOutput,
    MaxElapsed,
}

pub struct OutputReader<R> {
    reader: R,
    sentinel: Sentinel,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> OutputReader<R> {
    pub fn new(reader: R, sentinel: Sentinel) -> Self {
        Self {
            reader,
            sentinel,
            buf: Vec::with_capacity(READ_CHUNK),
        }
    }

    /// Read until the sentinel marker is found, EOF, or one of the
    /// quiet/max thresholds is hit. The sentinel marker itself is consumed
    /// from the buffer; only the bytes belonging to the command's output are
    /// returned.
    ///
    /// `quiet` returns Quiet{NoOutput} when no bytes have been read for that
    /// long. `max` returns Quiet{MaxElapsed} after that wall-clock from the
    /// start of this call. `None` for either disables that trigger.
    pub async fn read_until_sentinel(
        &mut self,
        quiet: Option<Duration>,
        max: Option<Duration>,
    ) -> std::io::Result<ReadOutcome> {
        if let Some(m) = self.sentinel.find(&self.buf) {
            return Ok(self.take_match(m));
        }

        let start = Instant::now();
        let mut last_byte_at = Instant::now();
        let mut chunk = [0u8; READ_CHUNK];

        loop {
            let mut deadlines: Vec<Duration> = Vec::with_capacity(2);
            if let Some(q) = quiet {
                deadlines.push(q.saturating_sub(last_byte_at.elapsed()));
            }
            if let Some(m) = max {
                deadlines.push(m.saturating_sub(start.elapsed()));
            }
            let next = deadlines.into_iter().min();

            let event = match next {
                Some(d) if d.is_zero() => ReadEvent::Timer,
                Some(d) => match timeout(d, self.reader.read(&mut chunk)).await {
                    Ok(Ok(n)) => ReadEvent::Read(n),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => ReadEvent::Timer,
                },
                None => match self.reader.read(&mut chunk).await {
                    Ok(n) => ReadEvent::Read(n),
                    Err(e) => return Err(e),
                },
            };

            match event {
                ReadEvent::Read(0) => {
                    let leftover = std::mem::take(&mut self.buf);
                    return Ok(ReadOutcome::Eof { output: leftover });
                }
                ReadEvent::Read(n) => {
                    last_byte_at = Instant::now();
                    self.buf.extend_from_slice(&chunk[..n]);
                    if let Some(m) = self.sentinel.find(&self.buf) {
                        return Ok(self.take_match(m));
                    }
                }
                ReadEvent::Timer => {
                    if let Some(q) = quiet
                        && last_byte_at.elapsed() >= q
                    {
                        return Ok(self.take_quiet(QuietReason::NoOutput));
                    }
                    if let Some(m) = max
                        && start.elapsed() >= m
                    {
                        return Ok(self.take_quiet(QuietReason::MaxElapsed));
                    }
                    // Timer collapsed but neither threshold actually trips
                    // yet — rare scheduling race. Loop and recompute.
                }
            }
        }
    }

    fn take_match(&mut self, m: SentinelMatch) -> ReadOutcome {
        let output = self.buf[..m.output_len].to_vec();
        self.buf.drain(..m.consumed);
        ReadOutcome::Done {
            output,
            exit_code: m.exit_code,
        }
    }

    fn take_quiet(&mut self, reason: QuietReason) -> ReadOutcome {
        let output = std::mem::take(&mut self.buf);
        ReadOutcome::Quiet { output, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn nonce() -> &'static str {
        "abcdef0011223344"
    }

    fn marker(exit: i32) -> String {
        format!("\n__F_{}_{exit}__\n", nonce())
    }

    /// Build a reader from in-memory bytes.
    fn from_bytes(b: &[u8]) -> OutputReader<std::io::Cursor<Vec<u8>>> {
        OutputReader::new(std::io::Cursor::new(b.to_vec()), Sentinel::new(nonce()))
    }

    #[tokio::test]
    async fn done_in_one_chunk() {
        let stream = format!("hello{}", marker(0));
        let mut r = from_bytes(stream.as_bytes());
        let out = r.read_until_sentinel(None, None).await.unwrap();
        match out {
            ReadOutcome::Done { output, exit_code } => {
                assert_eq!(output, b"hello");
                assert_eq!(exit_code, 0);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn done_with_trailing_newline() {
        let stream = format!("hello\n{}", marker(0));
        let mut r = from_bytes(stream.as_bytes());
        let out = r.read_until_sentinel(None, None).await.unwrap();
        match out {
            ReadOutcome::Done { output, .. } => assert_eq!(output, b"hello\n"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn done_nonzero_exit() {
        let stream = format!("oops\n{}", marker(127));
        let mut r = from_bytes(stream.as_bytes());
        let out = r.read_until_sentinel(None, None).await.unwrap();
        match out {
            ReadOutcome::Done { exit_code, .. } => assert_eq!(exit_code, 127),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn eof_before_sentinel() {
        let mut r = from_bytes(b"partial output");
        let out = r.read_until_sentinel(None, None).await.unwrap();
        match out {
            ReadOutcome::Eof { output } => assert_eq!(output, b"partial output"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn quiet_no_output_trips() {
        // Use a tokio duplex pipe: the writer side never writes, so the
        // reader will block on read until quiet trips.
        let (client, _server) = tokio::io::duplex(64);
        let mut r = OutputReader::new(client, Sentinel::new(nonce()));
        let out = r
            .read_until_sentinel(Some(Duration::from_millis(50)), None)
            .await
            .unwrap();
        match out {
            ReadOutcome::Quiet {
                reason: QuietReason::NoOutput,
                output,
            } => assert!(output.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_elapsed_trips_even_with_streaming() {
        let (client, mut server) = tokio::io::duplex(1024);
        let mut r = OutputReader::new(client, Sentinel::new(nonce()));

        // Producer task: write a byte every 5 ms.
        let producer = tokio::spawn(async move {
            for _ in 0..200 {
                if server.write_all(b"x").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let out = r
            .read_until_sentinel(
                Some(Duration::from_millis(20)),
                Some(Duration::from_millis(100)),
            )
            .await
            .unwrap();
        producer.abort();

        match out {
            ReadOutcome::Quiet {
                reason: QuietReason::MaxElapsed,
                output,
            } => assert!(!output.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn sentinel_at_chunk_boundary() {
        // First chunk: command output and the marker's leading \n.
        // Second chunk: the rest of the marker. Tests that the buffer joins
        // them and finds the boundary correctly.
        let (client, mut server) = tokio::io::duplex(64);
        let mut r = OutputReader::new(client, Sentinel::new(nonce()));

        let producer = tokio::spawn(async move {
            server.write_all(b"hi\n__F_").await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            let rest = format!("{}_5__\n", nonce());
            server.write_all(rest.as_bytes()).await.unwrap();
        });

        let out = r.read_until_sentinel(None, None).await.unwrap();
        producer.await.unwrap();

        match out {
            ReadOutcome::Done { output, exit_code } => {
                assert_eq!(output, b"hi");
                assert_eq!(exit_code, 5);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn keep_waiting_after_quiet_resumes() {
        // First call returns Quiet (server is silent). Then server writes
        // output + sentinel, and a second call returns Done.
        let (client, mut server) = tokio::io::duplex(256);
        let mut r = OutputReader::new(client, Sentinel::new(nonce()));

        let first = r
            .read_until_sentinel(Some(Duration::from_millis(30)), None)
            .await
            .unwrap();
        assert!(matches!(first, ReadOutcome::Quiet { .. }));

        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let s = format!("done\n__F_{}_0__\n", nonce());
            server.write_all(s.as_bytes()).await.unwrap();
        });

        let second = r.read_until_sentinel(None, None).await.unwrap();
        producer.await.unwrap();

        match second {
            ReadOutcome::Done { output, exit_code } => {
                assert_eq!(output, b"done");
                assert_eq!(exit_code, 0);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
