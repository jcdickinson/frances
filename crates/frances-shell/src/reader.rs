use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::timeout;

use crate::proto::{Sentinel, SentinelMatch};

const READ_CHUNK: usize = 4096;

enum ReadLoopEvent {
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

/// Discrete events delivered through an `OutputReader`'s sink. A
/// command run produces zero or more `Output` events as bytes arrive
/// (the sentinel itself is never shipped), terminated by exactly one
/// of `Quiet { reason }`, `Done { exit_code }`, or `Dead`. The
/// terminal event always corresponds to the `ReadOutcome` the same
/// `read_until_sentinel` call is about to return, so consumers can
/// drive frame-close logic off the event alone without a separate
/// barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadEvent {
    Output(Vec<u8>),
    Quiet { reason: QuietReason },
    Done { exit_code: i32 },
    Dead,
}

pub struct OutputReader<R> {
    reader: R,
    sentinel: Sentinel,
    buf: Vec<u8>,
    /// Optional event stream. When `Some`, every read ships
    /// safe-not-sentinel bytes through the channel as `Output` events,
    /// and every terminal (sentinel/quiet/eof) ships its matching
    /// `Done`/`Quiet`/`Dead` event before `read_until_sentinel`
    /// returns. Bytes stay in `buf` too so `ReadOutcome::*` payloads
    /// — and therefore direct callers of [`Shell::run`] — keep seeing
    /// the full output unchanged. `shipped` tracks how many bytes
    /// have already been emitted to avoid double-sending.
    sink: Option<UnboundedSender<ReadEvent>>,
    /// Byte index into `buf` past which every byte has already been
    /// shipped as an `Output` event. Reset to 0 by every `take_*`
    /// finaliser, which also drains `buf`.
    shipped: usize,
}

impl<R: AsyncRead + Unpin> OutputReader<R> {
    pub fn new(reader: R, sentinel: Sentinel) -> Self {
        Self {
            reader,
            sentinel,
            buf: Vec::with_capacity(READ_CHUNK),
            sink: None,
            shipped: 0,
        }
    }

    /// Install (or remove) the event sink. Resets `shipped` so the
    /// next ship starts from the head of `buf`.
    pub fn set_sink(&mut self, sink: Option<UnboundedSender<ReadEvent>>) {
        self.sink = sink;
        self.shipped = 0;
    }

    /// Emit `buf[shipped..end]` as an `Output` event if there's
    /// anything new. Bytes remain in `buf`; only `shipped` moves
    /// forward. No-op when no sink is attached or when there's nothing
    /// new to send.
    fn ship(&mut self, end: usize) {
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        if end <= self.shipped {
            return;
        }
        let chunk = self.buf[self.shipped..end].to_vec();
        self.shipped = end;
        let _ = sink.send(ReadEvent::Output(chunk));
    }

    /// Emit a terminal event if a sink is attached.
    fn emit_terminal(&mut self, event: ReadEvent) {
        if let Some(sink) = self.sink.as_ref() {
            let _ = sink.send(event);
        }
    }

    /// Ship every byte we've confirmed is not part of an in-flight
    /// sentinel match — i.e. all but the trailing
    /// `sentinel.max_match_len()` slack we hold back for cross-read
    /// detection.
    fn ship_safe(&mut self) {
        let reserve = self.sentinel.max_match_len();
        let safe_end = self.buf.len().saturating_sub(reserve);
        self.ship(safe_end);
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
                Some(d) if d.is_zero() => ReadLoopEvent::Timer,
                Some(d) => match timeout(d, self.reader.read(&mut chunk)).await {
                    Ok(Ok(n)) => ReadLoopEvent::Read(n),
                    Ok(Err(e)) => return Err(e),
                    Err(_) => ReadLoopEvent::Timer,
                },
                None => match self.reader.read(&mut chunk).await {
                    Ok(n) => ReadLoopEvent::Read(n),
                    Err(e) => return Err(e),
                },
            };

            match event {
                ReadLoopEvent::Read(0) => return Ok(self.take_eof()),
                ReadLoopEvent::Read(n) => {
                    last_byte_at = Instant::now();
                    self.buf.extend_from_slice(&chunk[..n]);
                    if let Some(m) = self.sentinel.find(&self.buf) {
                        return Ok(self.take_match(m));
                    }
                    self.ship_safe();
                }
                ReadLoopEvent::Timer => {
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
        // Ship every output byte before the sentinel, then drop the
        // sentinel itself (which never goes to JS).
        self.ship(m.output_len);
        self.emit_terminal(ReadEvent::Done {
            exit_code: m.exit_code,
        });
        let output = self.buf[..m.output_len].to_vec();
        self.buf.drain(..m.consumed);
        self.shipped = 0;
        ReadOutcome::Done {
            output,
            exit_code: m.exit_code,
        }
    }

    fn take_quiet(&mut self, reason: QuietReason) -> ReadOutcome {
        let len = self.buf.len();
        self.ship(len);
        self.emit_terminal(ReadEvent::Quiet { reason });
        let output = std::mem::take(&mut self.buf);
        self.shipped = 0;
        ReadOutcome::Quiet { output, reason }
    }

    fn take_eof(&mut self) -> ReadOutcome {
        let len = self.buf.len();
        self.ship(len);
        self.emit_terminal(ReadEvent::Dead);
        let output = std::mem::take(&mut self.buf);
        self.shipped = 0;
        ReadOutcome::Eof { output }
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
        // Second chunk: the rest of the marker.
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

    /// Helper: drain everything pending on the event channel without
    /// blocking. Mimics what a JS consumer's `nextEvent`-loop would see
    /// after the call has settled.
    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ReadEvent>) -> Vec<ReadEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn sink_emits_quiet_terminal_when_output_is_empty() {
        let (client, _server) = tokio::io::duplex(64);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut r = OutputReader::new(client, Sentinel::new(nonce()));
        r.set_sink(Some(tx));
        let out = r
            .read_until_sentinel(Some(Duration::from_millis(30)), None)
            .await
            .unwrap();
        assert!(matches!(out, ReadOutcome::Quiet { .. }));
        assert_eq!(
            drain(&mut rx),
            vec![ReadEvent::Quiet {
                reason: QuietReason::NoOutput,
            }],
        );
    }

    #[tokio::test]
    async fn sink_streams_output_then_done_when_command_finishes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Plenty of leading output (> max_match_len) so ship_safe trips
        // on the first read, before the sentinel arrives.
        let leading: String = std::iter::repeat_n('x', 200).collect();
        let stream = format!("{leading}{}", marker(0));
        let mut r = from_bytes(stream.as_bytes());
        r.set_sink(Some(tx));
        let out = r.read_until_sentinel(None, None).await.unwrap();
        match out {
            ReadOutcome::Done { output, exit_code } => {
                assert_eq!(output, leading.as_bytes());
                assert_eq!(exit_code, 0);
            }
            other => panic!("unexpected: {other:?}"),
        }
        let events = drain(&mut rx);
        let last = events.last().expect("at least one event");
        assert_eq!(last, &ReadEvent::Done { exit_code: 0 });
        let mut concat = Vec::new();
        for ev in &events[..events.len() - 1] {
            match ev {
                ReadEvent::Output(bytes) => concat.extend_from_slice(bytes),
                other => panic!("unexpected mid-stream event: {other:?}"),
            }
        }
        assert_eq!(concat, leading.as_bytes());
    }

    #[tokio::test]
    async fn sink_never_ships_sentinel_bytes_when_split_across_reads() {
        let (client, mut server) = tokio::io::duplex(64);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut r = OutputReader::new(client, Sentinel::new(nonce()));
        r.set_sink(Some(tx));

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
        let events = drain(&mut rx);
        let mut shipped = Vec::new();
        for ev in &events {
            match ev {
                ReadEvent::Output(bytes) => shipped.extend_from_slice(bytes),
                ReadEvent::Done { exit_code } => assert_eq!(*exit_code, 5),
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(shipped, b"hi", "sentinel bytes must not leak");
        assert!(matches!(events.last(), Some(ReadEvent::Done { .. })));
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
