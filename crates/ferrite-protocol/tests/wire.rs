//! End-to-end tests over a real TCP connection to a real listener: startup,
//! authentication, the simple query flow and the extended query flow.

mod common;

use common::*;
use ferrite_protocol::types::oid;

#[tokio::test]
async fn a_client_can_log_in_and_reach_ready_for_query() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    let messages = client.login(USER, PASSWORD).await;

    let tags: Vec<u8> = messages.iter().map(|m| m.tag).collect();
    assert_eq!(tags.first(), Some(&b'R'), "AuthenticationOk comes first");
    assert_eq!(tags.last(), Some(&b'Z'), "ReadyForQuery comes last");
    assert!(tags.contains(&b'K'), "BackendKeyData is required");

    let params: Vec<String> = messages
        .iter()
        .filter(|m| m.tag == b'S')
        .map(|m| m.strings()[0].clone())
        .collect();
    for expected in ["server_version", "client_encoding", "DateStyle", "TimeZone"] {
        assert!(params.contains(&expected.to_owned()), "missing {expected}");
    }
    assert_eq!(messages.last().unwrap().body, b"I", "session starts idle");
}

#[tokio::test]
async fn a_simple_query_returns_a_described_result_set() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    let messages = client.simple_query("SELECT 1").await;
    let tags: Vec<u8> = messages.iter().map(|m| m.tag).collect();
    assert_eq!(tags, vec![b'T', b'D', b'C', b'Z']);
    assert_eq!(messages[0].field_names(), vec!["?column?"]);
    assert_eq!(messages[0].field_oids(), vec![oid::INT4]);
    assert_eq!(messages[1].row_values(), vec![Some("1".to_owned())]);
    assert_eq!(messages[2].strings(), vec!["SELECT 1"]);
}

#[tokio::test]
async fn every_scalar_type_survives_the_round_trip_in_text_format() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    let messages = client.simple_query("SELECT * FROM pets").await;
    let description = &messages[0];
    assert_eq!(
        description.field_names(),
        vec![
            "id",
            "name",
            "adopted",
            "weight_kg",
            "born_at",
            "external_id",
            "profile"
        ]
    );
    assert_eq!(
        description.field_oids(),
        vec![
            oid::INT4,
            oid::TEXT,
            oid::BOOL,
            oid::FLOAT8,
            oid::TIMESTAMPTZ,
            oid::UUID,
            oid::JSON
        ]
    );

    let rows: Vec<_> = messages.iter().filter(|m| m.tag == b'D').collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].row_values(),
        vec![
            Some("1".to_owned()),
            Some("Rex".to_owned()),
            Some("t".to_owned()),
            Some("12.5".to_owned()),
            Some("2025-08-10 10:15:00.123456+00".to_owned()),
            Some("01901b2c-3d4e-5f60-7182-93a4b5c6d7e8".to_owned()),
            Some(r#"{"species":"dog"}"#.to_owned()),
        ]
    );
    // Non-ASCII text must survive as UTF-8.
    assert_eq!(rows[1].row_values()[1], Some("Mœbius".to_owned()));
    // A row of nothing but NULLs.
    assert_eq!(rows[2].row_values(), vec![None; 7]);
    assert_eq!(
        messages.iter().find(|m| m.tag == b'C').unwrap().strings(),
        vec!["SELECT 3"]
    );
}

#[tokio::test]
async fn a_failing_statement_yields_an_error_and_the_session_survives() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    let messages = client.simple_query("SELECT nonsense").await;
    assert_eq!(
        messages.iter().map(|m| m.tag).collect::<Vec<_>>(),
        vec![b'E', b'Z']
    );
    assert_eq!(messages[0].sqlstate().as_deref(), Some("42601"));

    // The same connection keeps working afterwards.
    let after = client.simple_query("SELECT 1").await;
    assert_eq!(after[1].row_values(), vec![Some("1".to_owned())]);
}

#[tokio::test]
async fn a_permission_denial_maps_to_its_own_sqlstate() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    let messages = client.simple_query("SELECT secret").await;
    assert_eq!(messages[0].sqlstate().as_deref(), Some("42501"));
}

#[tokio::test]
async fn an_empty_statement_gets_an_empty_query_response() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    let messages = client.simple_query("   ").await;
    assert_eq!(
        messages.iter().map(|m| m.tag).collect::<Vec<_>>(),
        vec![b'I', b'Z']
    );
}

#[tokio::test]
async fn transaction_state_is_reported_in_ready_for_query() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    assert_eq!(
        client.simple_query("BEGIN").await.last().unwrap().body,
        b"T"
    );
    // A failure inside a transaction block flips the state to failed.
    assert_eq!(client.simple_query("boom").await.last().unwrap().body, b"E");
    assert_eq!(
        client.simple_query("ROLLBACK").await.last().unwrap().body,
        b"I"
    );
}

#[tokio::test]
async fn command_tags_report_affected_rows() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    for (sql, tag) in [
        ("INSERT INTO pets VALUES (1)", "INSERT 0 1"),
        ("UPDATE pets SET name = 'x'", "UPDATE 2"),
        ("DELETE FROM pets", "DELETE 0"),
        ("CREATE TABLE t (id int)", "CREATE TABLE"),
    ] {
        let messages = client.simple_query(sql).await;
        assert_eq!(
            messages.iter().find(|m| m.tag == b'C').unwrap().strings(),
            vec![tag.to_owned()],
            "for {sql}"
        );
    }
}

#[tokio::test]
async fn a_wrong_password_is_refused_with_a_fatal_error() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.write(&startup_packet(USER, DATABASE)).await;
    client.read_message().await;
    client.write(&password_message("not the password")).await;

    let error = client.read_message().await;
    assert_eq!(error.tag, b'E');
    assert_eq!(error.sqlstate().as_deref(), Some("28P01"));
    assert_eq!(error.severity().as_deref(), Some("FATAL"));
    assert!(
        client.try_read_message().await.is_err(),
        "the server must hang up after a failed login"
    );
}

#[tokio::test]
async fn a_role_without_the_connect_permission_cannot_open_a_session() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.write(&startup_packet("nologin", DATABASE)).await;
    client.read_message().await;
    client.write(&password_message("nologin-pw")).await;

    let error = client.read_message().await;
    assert_eq!(error.tag, b'E');
    assert_eq!(error.sqlstate().as_deref(), Some("28P01"));
}

#[tokio::test]
async fn a_protocol_version_2_client_is_told_the_version_is_unsupported() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    let mut packet = 12i32.to_be_bytes().to_vec();
    packet.extend_from_slice(&131_072i32.to_be_bytes());
    packet.extend_from_slice(&[0, 0, 0, 0]);
    client.write(&packet).await;
    assert!(
        client.try_read_message().await.is_err(),
        "the connection must be dropped, not left half-open"
    );
}

#[tokio::test]
async fn a_cancel_request_is_acknowledged_by_closing() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.write(&cancel_request(1, 2)).await;
    assert!(client.try_read_message().await.is_err());
}

#[tokio::test]
async fn terminate_ends_the_session_cleanly() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;
    client.write(&terminate()).await;
    assert!(client.try_read_message().await.is_err());
}

// --- extended query flow -------------------------------------------------

#[tokio::test]
async fn the_extended_flow_parses_binds_describes_and_executes() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    client.write(&parse("s1", "SELECT 1", &[])).await;
    client.write(&describe(b'S', "s1")).await;
    client.write(&bind("p1", "s1", &[])).await;
    client.write(&execute("p1", 0)).await;
    client.write(&sync()).await;

    let messages = client.read_until_ready().await;
    let tags: Vec<u8> = messages.iter().map(|m| m.tag).collect();
    assert_eq!(tags, vec![b'1', b't', b'T', b'2', b'D', b'C', b'Z']);
    assert_eq!(messages[2].field_oids(), vec![oid::INT4]);
    assert_eq!(messages[4].row_values(), vec![Some("1".to_owned())]);
    assert_eq!(messages[5].strings(), vec!["SELECT 1"]);
}

#[tokio::test]
async fn bound_parameters_reach_the_handler() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    client
        .write(&parse("s1", "SELECT $1::int4", &[oid::INT4]))
        .await;
    client.write(&bind("p1", "s1", &[Some("41")])).await;
    client.write(&execute("p1", 0)).await;
    client.write(&sync()).await;

    let messages = client.read_until_ready().await;
    let row = messages.iter().find(|m| m.tag == b'D').unwrap();
    assert_eq!(row.row_values(), vec![Some("41".to_owned())]);
}

#[tokio::test]
async fn a_null_parameter_is_delivered_as_null() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    client
        .write(&parse("s1", "SELECT $1::int4", &[oid::INT4]))
        .await;
    client.write(&bind("p1", "s1", &[None])).await;
    client.write(&execute("p1", 0)).await;
    client.write(&sync()).await;

    let messages = client.read_until_ready().await;
    let row = messages.iter().find(|m| m.tag == b'D').unwrap();
    assert_eq!(row.row_values(), vec![None]);
}

#[tokio::test]
async fn describe_reports_parameter_types_before_execution() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    client
        .write(&parse("s1", "SELECT $1::text, $2::int4", &[]))
        .await;
    client.write(&describe(b'S', "s1")).await;
    client.write(&sync()).await;

    let messages = client.read_until_ready().await;
    let params = messages.iter().find(|m| m.tag == b't').unwrap();
    let count = i16::from_be_bytes([params.body[0], params.body[1]]);
    assert_eq!(count, 2);
    assert_eq!(
        i32::from_be_bytes(params.body[2..6].try_into().unwrap()),
        oid::TEXT
    );
    assert_eq!(
        i32::from_be_bytes(params.body[6..10].try_into().unwrap()),
        oid::INT4
    );
}

#[tokio::test]
async fn a_row_limited_execute_suspends_and_resumes_the_portal() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    client
        .write(&parse("s1", "SELECT * FROM generate_series", &[]))
        .await;
    client.write(&bind("p1", "s1", &[])).await;
    client.write(&execute("p1", 4)).await;
    client.write(&sync()).await;

    let first = client.read_until_ready().await;
    assert_eq!(first.iter().filter(|m| m.tag == b'D').count(), 4);
    assert!(
        first.iter().any(|m| m.tag == b's'),
        "expected PortalSuspended"
    );

    client.write(&execute("p1", 0)).await;
    client.write(&sync()).await;
    let rest = client.read_until_ready().await;
    assert_eq!(rest.iter().filter(|m| m.tag == b'D').count(), 6);
    assert_eq!(
        rest.iter().find(|m| m.tag == b'C').unwrap().strings(),
        vec!["SELECT 10"],
        "the tag counts every row actually delivered"
    );
}

#[tokio::test]
async fn an_error_mid_batch_is_skipped_until_the_next_sync() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    // Binding a statement that was never parsed fails; the Execute that
    // follows must be dropped rather than answered.
    client.write(&bind("p1", "ghost", &[])).await;
    client.write(&execute("p1", 0)).await;
    client.write(&sync()).await;

    let messages = client.read_until_ready().await;
    assert_eq!(
        messages.iter().map(|m| m.tag).collect::<Vec<_>>(),
        vec![b'E', b'Z']
    );

    // And the connection is still usable.
    client.write(&parse("s2", "SELECT 1", &[])).await;
    client.write(&bind("p2", "s2", &[])).await;
    client.write(&execute("p2", 0)).await;
    client.write(&sync()).await;
    assert_eq!(
        client
            .read_until_ready()
            .await
            .iter()
            .filter(|m| m.tag == b'D')
            .count(),
        1
    );
}

#[tokio::test]
async fn closing_a_statement_also_drops_its_portals() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    client.write(&parse("s1", "SELECT 1", &[])).await;
    client.write(&bind("p1", "s1", &[])).await;
    client.write(&close(b'S', "s1")).await;
    client.write(&execute("p1", 0)).await;
    client.write(&sync()).await;

    let messages = client.read_until_ready().await;
    assert!(
        messages.iter().any(|m| m.tag == b'3'),
        "expected CloseComplete"
    );
    let error = messages.iter().find(|m| m.tag == b'E').expect("an error");
    assert!(error.error_message().unwrap().contains("portal"));
}

#[tokio::test]
async fn redefining_a_live_named_statement_is_refused() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    client.write(&parse("s1", "SELECT 1", &[])).await;
    client.write(&parse("s1", "SELECT version()", &[])).await;
    client.write(&sync()).await;

    let messages = client.read_until_ready().await;
    assert!(messages.iter().any(|m| m.tag == b'E'));
}

#[tokio::test]
async fn the_unnamed_statement_can_be_reused() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.login(USER, PASSWORD).await;

    for sql in ["SELECT 1", "SELECT version()"] {
        client.write(&parse("", sql, &[])).await;
        client.write(&bind("", "", &[])).await;
        client.write(&execute("", 0)).await;
        client.write(&sync()).await;
        let messages = client.read_until_ready().await;
        assert!(
            messages.iter().all(|m| m.tag != b'E'),
            "unnamed statements are replaceable"
        );
    }
}

#[tokio::test]
async fn concurrent_connections_are_served_independently() {
    let addr = start_plaintext().await;
    let mut handles = Vec::new();
    for _ in 0..8 {
        handles.push(tokio::spawn(async move {
            let mut client = RawClient::connect(addr).await;
            client.login(USER, PASSWORD).await;
            let messages = client.simple_query("SELECT 1").await;
            assert_eq!(messages[1].row_values(), vec![Some("1".to_owned())]);
        }));
    }
    for handle in handles {
        handle.await.expect("connection task");
    }
}

/// Over the wire, the whole point of the limiter: past the threshold the
/// server stops offering a password prompt at all, so a guessing loop no
/// longer gets a guess per connection.
#[tokio::test]
async fn repeated_wrong_passwords_lock_the_source_out() {
    use ferrite_protocol::{AuthThrottle, ThrottlePolicy, TlsMode};
    use std::time::Duration;

    let policy = ThrottlePolicy {
        max_failures: 3,
        window: Duration::from_secs(60),
        lockout: Duration::from_secs(60),
        // The progressive delay is tested in the unit tests; here it would
        // only make this test slow.
        delay_step: Duration::ZERO,
        max_delay: Duration::ZERO,
    };
    let addr = start(config(TlsMode::Disabled).with_throttle(AuthThrottle::new(policy))).await;

    for attempt in 0..3 {
        let mut client = RawClient::connect(addr).await;
        client.write(&startup_packet(USER, DATABASE)).await;
        assert_eq!(
            client.read_message().await.tag,
            b'R',
            "attempt {attempt} is under the threshold, so a password is still asked for"
        );
        client.write(&password_message("not the password")).await;
        assert_eq!(client.read_message().await.tag, b'E');
    }

    let mut client = RawClient::connect(addr).await;
    client.write(&startup_packet(USER, DATABASE)).await;
    let response = client.read_message().await;
    assert_eq!(
        response.tag, b'E',
        "past the threshold the error replaces the password prompt"
    );
    assert_eq!(response.sqlstate().as_deref(), Some("28P01"));

    // The lockout is on the source, not on the guess: the right password
    // does not get through it either.
    let mut client = RawClient::connect(addr).await;
    client.write(&startup_packet(USER, DATABASE)).await;
    assert_eq!(client.read_message().await.tag, b'E');

    // The lockout covers every account from that address, not just the one
    // that was guessed at — otherwise spraying user names would walk
    // straight around it. The price is that clients behind one outbound
    // address share the limit; see `throttle.rs`.
    let mut client = RawClient::connect(addr).await;
    client.write(&startup_packet("reader", DATABASE)).await;
    assert_eq!(client.read_message().await.tag, b'E');
}
