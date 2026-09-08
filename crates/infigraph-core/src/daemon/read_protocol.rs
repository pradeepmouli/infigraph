//! Framing for the daemon read service.
//!
//! Length-prefixed JSON frames. The explicit `End` frame is the point: a
//! stream that stops without one is a truncation, and the client must be
//! able to tell that from a query that legitimately returned no rows.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Which of the daemon's two stores a request is for. `search` with
/// `scope='all'` touches both in one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Store {
    Graph,
    Docs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadRequest {
    pub store: Store,
    pub query: String,
    pub params: Vec<(String, serde_json::Value)>,
    pub chunk_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadFrame {
    Rows(Vec<Vec<String>>),
    End,
    Error(String),
}

fn write_len_prefixed<W: Write>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

fn read_len_prefixed<R: Read>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .map_err(|_| anyhow::anyhow!("truncated frame: expected {len} bytes"))?;
    Ok(Some(body))
}

pub fn write_request<W: Write>(w: &mut W, req: &ReadRequest) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(req)?)
}

pub fn read_request<R: Read>(r: &mut R) -> Result<ReadRequest> {
    let body = read_len_prefixed(r)?.ok_or_else(|| anyhow::anyhow!("no request"))?;
    Ok(serde_json::from_slice(&body)?)
}

pub fn write_frame<W: Write>(w: &mut W, frame: &ReadFrame) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(frame)?)
}

pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<ReadFrame>> {
    match read_len_prefixed(r)? {
        None => Ok(None),
        Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
    }
}

/// Read frames until `End`. A stream that ends first is an error.
pub fn collect_rows<R: Read>(r: &mut R) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    loop {
        match read_frame(r)? {
            Some(ReadFrame::Rows(mut chunk)) => rows.append(&mut chunk),
            Some(ReadFrame::End) => return Ok(rows),
            Some(ReadFrame::Error(msg)) => anyhow::bail!("read service error: {msg}"),
            None => anyhow::bail!(
                "truncated result stream: the daemon closed the connection before sending \
                 an end-of-results frame. This is not an empty result set."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips() {
        let req = ReadRequest {
            store: Store::Graph,
            query: "MATCH (f:File) RETURN f.id".to_string(),
            params: vec![],
            chunk_size: 1024,
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let got = read_request(&mut buf.as_slice()).unwrap();
        assert_eq!(got.query, req.query);
        assert_eq!(got.store, Store::Graph);
    }

    /// A stream that ends without an `End` frame must be an error, never an
    /// empty result. Returning zero rows silently is the "0-symbol graph
    /// served as healthy" failure `Infigraph::init` already shipped once.
    #[test]
    fn a_truncated_stream_is_an_error_not_an_empty_result() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a".to_string()]])).unwrap();
        // Deliberately no End frame, and cut mid-frame.
        buf.truncate(buf.len() - 2);

        let err = collect_rows(&mut buf.as_slice())
            .expect_err("a truncated stream must not read as a complete empty result");
        assert!(err.to_string().contains("truncated"), "unexpected: {err}");
    }

    #[test]
    fn a_complete_stream_collects_all_rows() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a".to_string()]])).unwrap();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["b".to_string()]])).unwrap();
        write_frame(&mut buf, &ReadFrame::End).unwrap();
        let rows = collect_rows(&mut buf.as_slice()).unwrap();
        assert_eq!(rows, vec![vec!["a".to_string()], vec!["b".to_string()]]);
    }
}
