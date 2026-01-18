//! H2MUX Server Stream
//!
//! Wraps H2MuxStream with sing-mux server protocol handling:
//! - Status response is prepended to first write (like sing-mux serverConn)
//! - Optional activity tracking for connection idle timeout

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::async_stream::{AsyncPing, AsyncStream};
use crate::util::write_all;

use super::activity_tracker::ActivityTracker;
use super::h2mux_protocol::{StreamResponse, STATUS_SUCCESS};
use super::h2mux_stream::H2MuxStream;

/// Server stream wrapper that prepends status response to first write.
///
/// This matches sing-mux's serverConn behavior where the status byte
/// is sent with the first data write rather than immediately.
pub struct H2MuxServerStream {
    inner: H2MuxStream,
    /// Whether we've written the status response
    response_written: bool,
    /// Optional activity tracker for connection idle timeout
    activity: Option<ActivityTracker>,
}

impl H2MuxServerStream {
    /// Create without activity tracking.
    #[allow(dead_code)]
    pub fn new(inner: H2MuxStream) -> Self {
        Self {
            inner,
            response_written: false,
            activity: None,
        }
    }

    /// Create with activity tracker for idle timeout support.
    pub fn with_activity(inner: H2MuxStream, activity: ActivityTracker) -> Self {
        Self {
            inner,
            response_written: false,
            activity: Some(activity),
        }
    }

    /// Get reference to inner stream.
    #[allow(dead_code)]
    pub fn inner_mut(&mut self) -> &mut H2MuxStream {
        &mut self.inner
    }

    /// Record activity if tracker is present.
    #[inline]
    fn record_activity(&self) {
        if let Some(ref activity) = self.activity {
            activity.record_activity();
        }
    }

    /// Send an error response to the client before closing.
    ///
    /// This should be called when rejecting a stream (e.g., UDP disabled).
    /// After calling this, the stream should be shut down.
    /// Returns error if response was already written.
    pub async fn write_error_response(&mut self, message: &str) -> io::Result<()> {
        if self.response_written {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Response already written",
            ));
        }

        let response = StreamResponse::error(message);
        let encoded = response.encode();
        write_all(&mut self.inner, &encoded).await?;
        self.response_written = true;
        Ok(())
    }
}

impl AsyncRead for H2MuxServerStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);

        // Record activity on successful read with data
        if let Poll::Ready(Ok(())) = &result {
            if buf.filled().len() > 0 {
                self.record_activity();
            }
        }

        result
    }
}

impl AsyncWrite for H2MuxServerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // First write prepends the status response
        let result = if !self.response_written {
            // Create combined buffer: status + data
            let mut combined = BytesMut::with_capacity(1 + buf.len());
            combined.put_u8(STATUS_SUCCESS);
            combined.put_slice(buf);

            match Pin::new(&mut self.inner).poll_write(cx, &combined) {
                Poll::Ready(Ok(written)) => {
                    self.response_written = true;
                    // Return amount of user data written (subtract status byte)
                    if written >= 1 {
                        Poll::Ready(Ok((written - 1).min(buf.len())))
                    } else {
                        Poll::Ready(Ok(0))
                    }
                }
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        } else {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        };

        // Record activity on successful write
        if let Poll::Ready(Ok(n)) = &result {
            if *n > 0 {
                self.record_activity();
            }
        }

        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl AsyncPing for H2MuxServerStream {
    fn supports_ping(&self) -> bool {
        false
    }

    fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
        Poll::Ready(Ok(false))
    }
}

impl Unpin for H2MuxServerStream {}

impl AsyncStream for H2MuxServerStream {}
