//! Establishes the local-socket API this crate will use, on the platform
//! actually running the tests. Everything in the read service copies this
//! shape, so it is proven once here rather than assumed in five places.

use std::io::{BufRead, BufReader, Write};

use infigraph_core::daemon::read_endpoint::ReadEndpoint;

#[test]
fn a_local_socket_round_trips_one_line() {
    let endpoint = ReadEndpoint::for_root(std::path::Path::new("/tmp/interprocess-smoke"));

    // Server: accept one connection, echo one line back.
    let listener = endpoint.bind().expect("bind");
    let server = std::thread::spawn(move || {
        let stream = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let mut stream = reader.into_inner();
        write!(stream, "echo:{line}").expect("write");
        stream.flush().expect("flush");
    });

    let mut client = endpoint.connect().expect("connect");
    writeln!(client, "hello").expect("write");
    client.flush().expect("flush");
    let mut reader = BufReader::new(client);
    let mut got = String::new();
    reader.read_line(&mut got).expect("read");
    assert_eq!(got.trim_end(), "echo:hello");
    server.join().unwrap();
}

/// The endpoint name must actually be bindable, not merely short. On macOS
/// `GenericNamespaced` is a pseudo-namespace backed by a real file, so a
/// name that satisfies the length assertion in `read_endpoint`'s unit tests
/// could still fail at `bind` -- this is the test that would catch that.
#[test]
fn a_deep_project_root_still_binds() {
    let deep = std::env::temp_dir()
        .join("infigraph-smoke/scratchpad/wt-a-very-long-worktree-name/nested/deeper/deeper-still");
    let endpoint = ReadEndpoint::for_root(&deep);
    let listener = endpoint
        .bind()
        .expect("a deep project root must still produce a bindable endpoint");
    drop(listener);
}
