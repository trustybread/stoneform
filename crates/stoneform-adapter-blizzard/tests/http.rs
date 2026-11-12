//! The HTTP adapter against local mock servers.

use std::sync::Mutex;
use std::time::Duration;

use stoneform_adapter_blizzard::{BlizzardHttp, HttpError, Retry, VersionTable};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FILE: &str = "tpr/wow/data/ab/cd/abcdef";

fn client(retries: usize) -> BlizzardHttp {
    BlizzardHttp::new().unwrap().with_retry(Retry::with_delays(vec![Duration::ZERO; retries]))
}

async fn serving(status: u16, body: &[u8]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{FILE}")))
        .respond_with(ResponseTemplate::new(status).set_body_bytes(body.to_vec()))
        .mount(&server)
        .await;
    server
}

fn ranged(start: u64, end: u64, total: u64, body: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(206)
        .insert_header("Content-Range", format!("bytes {start}-{end}/{total}"))
        .set_body_bytes(body.to_vec())
}

#[tokio::test]
async fn reads_a_version_table() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/products/wowf/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Region!STRING:0|BuildConfig!HEX:16\neu|abc\n"))
        .mount(&server)
        .await;

    let text = client(0).with_version_base(server.uri()).version_table("eu", "wowf", VersionTable::Versions).await;

    assert_eq!(text.unwrap(), "Region!STRING:0|BuildConfig!HEX:16\neu|abc\n");
}

#[tokio::test]
async fn retries_a_busy_version_service() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).up_to_n_times(2).mount(&server).await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string("ok")).mount(&server).await;

    let text = client(3).with_version_base(server.uri()).version_table("eu", "wowf", VersionTable::Cdns).await;

    assert_eq!(text.unwrap(), "ok");
}

#[tokio::test]
async fn gives_up_after_the_last_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).expect(3).mount(&server).await;

    let error = client(2).fetch(&[server.uri()], FILE, None).await.unwrap_err();

    assert!(matches!(error, HttpError::Status { status: 503, .. }), "{error}");
}

#[tokio::test]
async fn skips_a_mirror_that_lacks_the_file() {
    let missing = serving(404, b"").await;
    let good = serving(200, b"payload").await;

    let body = client(0).fetch(&[missing.uri(), good.uri()], FILE, None).await.unwrap();

    assert_eq!(&body[..], b"payload");
}

#[tokio::test]
async fn reports_not_found_when_no_mirror_has_it() {
    let first = serving(404, b"").await;
    let second = serving(404, b"").await;

    let error = client(3).fetch(&[first.uri(), second.uri()], FILE, None).await.unwrap_err();

    assert!(matches!(error, HttpError::NotFound { .. }), "{error}");
}

#[tokio::test]
async fn fetches_a_byte_range() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("Range", "bytes=2-4"))
        .respond_with(ranged(2, 4, 10, b"cde"))
        .mount(&server)
        .await;

    let body = client(0).fetch(&[server.uri()], FILE, Some(2..=4)).await.unwrap();

    assert_eq!(&body[..], b"cde");
}

#[tokio::test]
async fn rejects_a_range_answer_for_other_bytes_and_tries_the_next_mirror() {
    let wrong = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ranged(0, 2, 10, b"abc")).mount(&wrong).await;
    let right = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ranged(2, 4, 10, b"cde")).mount(&right).await;

    let body = client(0).fetch(&[wrong.uri(), right.uri()], FILE, Some(2..=4)).await.unwrap();
    assert_eq!(&body[..], b"cde");

    let error = client(0).fetch(&[wrong.uri()], FILE, Some(2..=4)).await.unwrap_err();
    assert!(matches!(error, HttpError::Malformed { .. }), "{error}");
}

#[tokio::test]
async fn rejects_a_whole_file_where_a_range_was_asked() {
    let server = serving(200, b"abcdefghij").await;

    let error = client(0).fetch(&[server.uri()], FILE, Some(2..=4)).await.unwrap_err();

    assert!(matches!(error, HttpError::Status { status: 200, .. }), "{error}");
}

#[tokio::test]
async fn downloads_a_file_and_reports_progress() {
    let server = serving(200, b"0123456789").await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file");
    let seen = Mutex::new(Vec::new());

    let size =
        client(0).fetch_to_file(&[server.uri()], FILE, &dest, &|bytes| seen.lock().unwrap().push(bytes)).await.unwrap();

    assert_eq!(size, 10);
    assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
    assert_eq!(seen.lock().unwrap().last(), Some(&10));
}

#[tokio::test]
async fn resumes_a_partial_download() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("Range", "bytes=4-"))
        .respond_with(ranged(4, 9, 10, b"456789"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file");
    std::fs::write(&dest, b"0123").unwrap();

    let size = client(0).fetch_to_file(&[server.uri()], FILE, &dest, &|_| {}).await.unwrap();

    assert_eq!(size, 10);
    assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
}

#[tokio::test]
async fn starts_over_when_the_server_ignores_the_range() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header_exists("Range"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"0123456789".to_vec()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file");
    std::fs::write(&dest, b"0123").unwrap();

    client(0).fetch_to_file(&[server.uri()], FILE, &dest, &|_| {}).await.unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
}

#[tokio::test]
async fn starts_over_when_the_partial_file_cannot_be_resumed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header_exists("Range"))
        .respond_with(ResponseTemplate::new(416))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"0123456789".to_vec()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file");
    std::fs::write(&dest, b"stale data, longer than the file").unwrap();

    client(0).fetch_to_file(&[server.uri()], FILE, &dest, &|_| {}).await.unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
}
