//! Hostile input. Every byte the protocol decodes comes from the network,
//! so the property under test is always the same: the server may close the
//! connection or answer with an error, but it must never panic and must
//! never keep serving a stream it has lost sync with.
//!
//! `fuzz/fuzz_targets/message_decode.rs` covers the same decoders with
//! `cargo-fuzz`; these cases are the deterministic ones that run in CI on
//! stable.

mod common;

use common::*;
use ferrite_protocol::message::{Frontend, StartupRequest};

/// Deterministic pseudo-random bytes, so a failure is reproducible from the
/// seed alone without pulling in a generator dependency.
fn pseudo_random(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

#[test]
fn decoding_arbitrary_bytes_never_panics() {
    for seed in 0..2_000u64 {
        let body = pseudo_random(seed, (seed % 64) as usize);
        let tag = (seed % 256) as u8;
        let _ = Frontend::decode(tag, &body);
        let _ = StartupRequest::decode(&body);
    }
}

#[test]
fn decoding_every_truncation_of_a_valid_message_never_panics() {
    let valid: Vec<(u8, Vec<u8>)> = vec![
        (b'Q', b"SELECT 1\0".to_vec()),
        (b'P', b"s\0SELECT $1\0\0\x01\0\0\0\x17".to_vec()),
        (b'B', {
            let mut body = b"p\0s\0".to_vec();
            body.extend_from_slice(&1i16.to_be_bytes());
            body.extend_from_slice(&1i16.to_be_bytes());
            body.extend_from_slice(&1i16.to_be_bytes());
            body.extend_from_slice(&4i32.to_be_bytes());
            body.extend_from_slice(&42i32.to_be_bytes());
            body.extend_from_slice(&0i16.to_be_bytes());
            body
        }),
        (b'D', b"S\0".to_vec()),
        (b'E', b"p\0\0\0\0\0".to_vec()),
        (b'C', b"P\0".to_vec()),
    ];
    for (tag, body) in valid {
        for cut in 0..=body.len() {
            let _ = Frontend::decode(tag, &body[..cut]);
        }
    }
}

#[test]
fn a_length_field_claiming_more_than_the_body_is_an_error_not_a_panic() {
    // A Bind whose parameter count promises far more data than follows.
    let mut body = b"p\0s\0".to_vec();
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&i16::MAX.to_be_bytes());
    assert!(Frontend::decode(b'B', &body).is_err());

    // A Parse promising 32767 parameter OIDs it does not carry.
    let mut body = b"s\0SELECT 1\0".to_vec();
    body.extend_from_slice(&i16::MAX.to_be_bytes());
    assert!(Frontend::decode(b'P', &body).is_err());
}

#[test]
fn a_parameter_value_claiming_a_huge_length_is_an_error_not_a_panic() {
    let mut body = b"p\0s\0".to_vec();
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&1i16.to_be_bytes());
    body.extend_from_slice(&i32::MAX.to_be_bytes());
    assert!(Frontend::decode(b'B', &body).is_err());
}

#[tokio::test]
async fn a_garbage_stream_is_dropped_without_taking_the_server_down() {
    let addr = start_plaintext().await;
    for seed in 0..32u64 {
        let mut client = RawClient::connect(addr).await;
        client.write(&pseudo_random(seed, 128)).await;
        // Whatever the server answers, it must eventually close.
        while client.try_read_message().await.is_ok() {}
    }

    // The listener is still healthy for a well-behaved client.
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;
    assert_eq!(
        client.simple_query("SELECT 1").await[1].row_values(),
        vec![Some("1".to_owned())]
    );
}

#[tokio::test]
async fn an_oversized_frame_is_refused_before_it_is_allocated() {
    let addr = start(config(ferrite_protocol::TlsMode::Disabled).with_max_message_size(4096)).await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    // Claims a 512 MiB body without sending it.
    let mut frame = vec![b'Q'];
    frame.extend_from_slice(&(512 * 1024 * 1024i32).to_be_bytes());
    client.write(&frame).await;

    let error = client.read_message().await;
    assert_eq!(error.tag, b'E');
    assert_eq!(error.sqlstate().as_deref(), Some("08P01"));
}

#[tokio::test]
async fn a_frame_shorter_than_its_own_header_is_refused() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;
    client.write(&[b'Q', 0, 0, 0, 0]).await;

    let error = client.read_message().await;
    assert_eq!(error.tag, b'E');
    assert_eq!(error.sqlstate().as_deref(), Some("08P01"));
}

#[tokio::test]
async fn an_unsupported_message_type_is_refused_cleanly() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;
    // 'F' is FunctionCall, which Ferrite does not implement.
    client.write(&[b'F', 0, 0, 0, 4]).await;

    let error = client.read_message().await;
    assert_eq!(error.tag, b'E');
    assert_eq!(error.sqlstate().as_deref(), Some("08P01"));
}

#[tokio::test]
async fn a_startup_packet_that_is_all_zeroes_is_refused() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.write(&[0, 0, 0, 8, 0, 0, 0, 0]).await;
    assert!(client.try_read_message().await.is_err());
}

#[tokio::test]
async fn a_password_message_outside_authentication_is_refused() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;
    client.write(&password_message("late")).await;

    let error = client.read_message().await;
    assert_eq!(error.tag, b'E');
    assert_eq!(error.sqlstate().as_deref(), Some("08P01"));
}

#[tokio::test]
async fn a_client_that_disappears_mid_frame_does_not_wedge_the_server() {
    let addr = start_plaintext().await;
    {
        let mut client = RawClient::connect(addr).await;
        client.login(USER, PASSWORD).await;
        // Half of a Query frame, then drop the socket.
        client.write(&[b'Q', 0, 0, 0, 13, b'S', b'E']).await;
    }
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;
    assert_eq!(
        client.simple_query("SELECT 1").await[1].row_values(),
        vec![Some("1".to_owned())]
    );
}
