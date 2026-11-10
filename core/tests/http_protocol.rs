//! How the reader copes with servers that are unusual, broken or hostile.

mod support;

use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::archive_limits::TAIL_BYTES;
use lazy_zip_core::error::Error;
use lazy_zip_core::http_reader::{ReaderConfig, RemoteHttpReader};
use lazy_zip_core::zip_explorer::{self, ScanResult};
use std::io::SeekFrom;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::*;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// The default for [`ReaderConfig::chunk_bytes`].
const DEFAULT_CHUNK_BYTES: u64 = 1024 * 1024;

async fn scan(url: &str) -> Result<ScanResult, Error> {
    let reader = RemoteHttpReader::open(url, fast_config()).await?;
    let archive = RemoteArchive::from_reader(reader).await?;
    Ok(zip_explorer::scan(&archive))
}

fn small_zip() -> Vec<u8> {
    build_zip(&[stored("a.txt", "hello"), stored("dir/b.txt", "world")])
}

/// An archive whose directory doesn't fit in the first fetch from the end of the file.
fn large_directory_zip() -> Vec<u8> {
    let entries: Vec<Entry> = (0..5000)
        .map(|i| stored(&format!("folder/entry-with-a-long-name-{i:05}.txt"), "x"))
        .collect();
    build_zip(&entries)
}

#[tokio::test]
async fn sends_a_user_agent_and_never_needs_head() {
    let server = serve_with(
        small_zip(),
        Behavior {
            reject_head: true,
            ..Default::default()
        },
    )
    .await;

    let result = scan(&server.url).await.unwrap();

    assert_eq!(result.nodes.len(), 3);
    let requests = server.requests();
    assert!(requests.iter().all(|r| r.method == "GET"), "{requests:?}");
    assert!(requests[0]
        .user_agent
        .as_deref()
        .is_some_and(|agent| agent.starts_with("lazy-zip/")));
}

#[tokio::test]
async fn small_archive_is_listed_with_a_single_request() {
    let server = serve(small_zip()).await;
    scan(&server.url).await.unwrap();
    assert_eq!(server.gets(), 1, "{:?}", server.requests());
}

#[tokio::test]
async fn a_maximum_length_comment_costs_one_request() {
    let archive = build_zip_with_comment(&[stored("a.txt", "x")], &vec![b'c'; 65535]);
    let server = serve(archive).await;
    let result = scan(&server.url).await.unwrap();
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(server.gets(), 1);
}

#[tokio::test]
async fn server_ignoring_range_stops_after_reading_no_data() {
    let archive = vec![7u8; 50 * 1024 * 1024];
    let server = serve_with(
        archive,
        Behavior {
            ignore_range: true,
            ..Default::default()
        },
    )
    .await;
    let config = fast_config();
    let network = config.network_bytes.clone();

    let error = match RemoteHttpReader::open(&server.url, config).await {
        Err(error) => error,
        Ok(_) => panic!("expected an error"),
    };

    assert!(matches!(error, Error::RangeNotSupported), "{error}");
    assert_eq!(network.load(Ordering::Relaxed), 0);
    assert!(server.gets() <= 2, "{}", server.gets());
}

#[tokio::test]
async fn servers_without_suffix_ranges_still_work() {
    let refusing = [400, 416, 501].map(|status| Behavior {
        suffix_range_status: Some(status),
        ..Default::default()
    });
    let ignoring = Behavior {
        no_suffix_ranges: true,
        ..Default::default()
    };
    for behavior in refusing.into_iter().chain([ignoring]) {
        let server = serve_with(small_zip(), behavior).await;
        assert_eq!(scan(&server.url).await.unwrap().nodes.len(), 3);
    }
}

#[tokio::test]
async fn permanent_errors_on_the_suffix_range_are_not_treated_as_missing_support() {
    for status in [401, 403, 404, 410] {
        let server = serve_with(
            small_zip(),
            Behavior {
                suffix_range_status: Some(status),
                ..Default::default()
            },
        )
        .await;
        assert!(
            matches!(scan(&server.url).await, Err(Error::HttpStatus { status: s }) if s == status),
            "{status}"
        );
        assert_eq!(server.gets(), 1);
    }
}

#[tokio::test]
async fn a_non_zip_is_reported_clearly() {
    let server = serve(b"<html>this is a web page</html>".to_vec()).await;
    assert!(matches!(scan(&server.url).await, Err(Error::NotAZip)));
}

#[tokio::test]
async fn an_empty_file_is_reported_clearly() {
    let server = serve(Vec::new()).await;
    assert!(matches!(scan(&server.url).await, Err(Error::NotAZip)));
}

#[tokio::test]
async fn http_errors_keep_their_status_and_are_not_retried_when_permanent() {
    let server = serve_with(
        small_zip(),
        Behavior {
            fail_first: Some((100, 404, None)),
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(
        scan(&server.url).await,
        Err(Error::HttpStatus { status: 404 })
    ));
    assert_eq!(server.gets(), 1);
}

#[tokio::test]
async fn transient_errors_are_retried() {
    for status in [429, 500, 502, 503, 504] {
        let server = serve_with(
            small_zip(),
            Behavior {
                fail_first: Some((2, status, None)),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(scan(&server.url).await.unwrap().nodes.len(), 3, "{status}");
        assert_eq!(server.gets(), 3, "{status}");
    }
}

#[tokio::test]
async fn retries_give_up_with_the_last_error() {
    let server = serve_with(
        small_zip(),
        Behavior {
            fail_first: Some((100, 503, None)),
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(
        scan(&server.url).await,
        Err(Error::HttpStatus { status: 503 })
    ));
    assert_eq!(server.gets(), 3);
}

#[tokio::test]
async fn retry_after_is_honored() {
    let server = serve_with(
        small_zip(),
        Behavior {
            fail_first: Some((1, 429, Some(1))),
            ..Default::default()
        },
    )
    .await;
    let started = Instant::now();
    scan(&server.url).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(900));
}

#[tokio::test]
async fn a_stalled_server_times_out() {
    let server = serve_with(
        small_zip(),
        Behavior {
            stall_after: Some(0),
            ..Default::default()
        },
    )
    .await;
    let mut config = fast_config();
    config.retry.max_attempts = 1;
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        RemoteHttpReader::open(&server.url, config),
    )
    .await
    .expect("the reader should give up on its own");
    assert!(matches!(result, Err(Error::Http(_))));
}

#[tokio::test]
async fn network_errors_do_not_expose_signed_urls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("http://{address}/a.zip?signature=SECRET");
    let mut config = fast_config();
    config.retry.max_attempts = 1;
    let error = match RemoteHttpReader::open(&url, config).await {
        Err(error) => error,
        Ok(_) => panic!("expected an error"),
    };
    assert!(!error.to_string().contains("SECRET"), "{error}");
    // The cause depends on the platform: Linux and macOS refuse the connection, Windows times out.
    assert!(
        !error.to_string().contains("error sending request"),
        "should name the cause: {error}"
    );
}

/// Reads part of the file that isn't covered by the tail fetched at open.
async fn read_start_of_archive(behavior: Behavior) -> Result<Vec<u8>, Error> {
    let archive = large_directory_zip();
    let expected_len = archive.len();
    assert!(expected_len as u64 > lazy_zip_core::archive_limits::TAIL_BYTES);
    let server = serve_with(archive, behavior).await;
    let mut reader = RemoteHttpReader::open(&server.url, fast_config()).await?;
    let mut data = vec![0u8; 64];
    reader.read_exact(&mut data).await?;
    Ok(data)
}

#[tokio::test]
async fn reads_correct_data_from_a_good_server() {
    let data = read_start_of_archive(Behavior::default()).await.unwrap();
    assert_eq!(&data[..4], b"PK\x03\x04");
}

#[tokio::test]
async fn a_response_for_the_wrong_range_is_rejected() {
    // A shift would corrupt data silently if the Content-Range weren't checked. The tail fetched at
    // open tolerates it because a suffix is clamped at the end of the file.
    let error = read_start_of_archive(Behavior {
        shift_range: 10,
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(error, Error::InvalidResponse(_)), "{error}");
}

#[tokio::test]
async fn compressed_partial_responses_are_rejected() {
    let error = read_start_of_archive(Behavior {
        content_encoding: Some("gzip"),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(error, Error::InvalidResponse(_)), "{error}");
}

#[tokio::test]
async fn an_empty_body_is_an_error_not_an_endless_loop() {
    let server = serve_with(
        small_zip(),
        Behavior {
            empty_body: true,
            ..Default::default()
        },
    )
    .await;
    let result = tokio::time::timeout(Duration::from_secs(10), scan(&server.url))
        .await
        .expect("must not loop forever");
    assert!(result.is_err());
    assert!(server.gets() <= 3, "{}", server.gets());
}

#[tokio::test]
async fn a_truncated_body_is_an_error_after_retrying() {
    let error = read_start_of_archive(Behavior {
        truncate_after: Some(10),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(error, Error::TruncatedResponse), "{error}");
}

#[tokio::test]
async fn a_body_longer_than_promised_is_not_buffered() {
    let error = read_start_of_archive(Behavior {
        extra_body_bytes: 10 * 1024 * 1024,
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(error, Error::InvalidResponse(_)), "{error}");
}

#[tokio::test]
async fn a_file_changing_mid_read_is_detected_by_etag() {
    let error = read_start_of_archive(Behavior {
        etag: Some("\"v1\""),
        change_etag_after: Some(1),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(error, Error::RemoteChanged), "{error}");
}

#[tokio::test]
async fn a_file_changing_size_mid_read_is_detected() {
    let error = read_start_of_archive(Behavior {
        change_length_after: Some(1),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(error, Error::RemoteChanged), "{error}");
}

#[tokio::test]
async fn seeking_matches_a_local_file() {
    let archive = large_directory_zip();
    let server = serve(archive.clone()).await;
    let mut reader = RemoteHttpReader::open(&server.url, fast_config())
        .await
        .unwrap();

    let mut buffer = [0u8; 16];
    reader.seek(SeekFrom::Start(100)).await.unwrap();
    reader.read_exact(&mut buffer).await.unwrap();
    assert_eq!(buffer, archive[100..116]);

    reader.seek(SeekFrom::Current(-8)).await.unwrap();
    reader.read_exact(&mut buffer).await.unwrap();
    assert_eq!(buffer, archive[108..124]);

    let position = reader.seek(SeekFrom::End(-16)).await.unwrap();
    assert_eq!(position as usize, archive.len() - 16);
    reader.read_exact(&mut buffer).await.unwrap();
    assert_eq!(buffer, archive[archive.len() - 16..]);

    assert_eq!(reader.read(&mut buffer).await.unwrap(), 0);
    assert!(reader.seek(SeekFrom::Start(0)).await.is_ok());
}

#[tokio::test]
async fn seeking_before_the_start_is_an_error() {
    let server = serve(small_zip()).await;
    let mut reader = RemoteHttpReader::open(&server.url, fast_config())
        .await
        .unwrap();
    assert!(reader.seek(SeekFrom::End(-1_000_000)).await.is_err());
    reader.seek(SeekFrom::Start(0)).await.unwrap();
    assert!(reader.seek(SeekFrom::Current(-1)).await.is_err());
}

#[tokio::test]
async fn reads_up_to_the_tail_without_fetching_it_again() {
    let archive = large_directory_zip();
    let server = serve(archive.clone()).await;
    let mut reader = RemoteHttpReader::open(&server.url, fast_config())
        .await
        .unwrap();
    let mut everything = Vec::new();
    reader.read_to_end(&mut everything).await.unwrap();
    assert_eq!(everything, archive);
    let tail_refetched = server
        .requests()
        .iter()
        .skip(1)
        .filter_map(|r| r.range.clone())
        .filter_map(|r| r.split('-').nth(1)?.parse::<usize>().ok())
        .any(|end| end + 1 == archive.len());
    assert!(!tail_refetched, "{:?}", server.requests());
}

#[tokio::test]
async fn a_limited_reader_stops_at_its_limit_and_fetches_nothing_past_it() {
    let archive = large_directory_zip();
    let server = serve(archive.clone()).await;
    let mut config = fast_config();
    config.chunk_bytes = 1000;
    let mut reader = RemoteHttpReader::open(&server.url, config).await.unwrap();
    let limit = 5500;
    reader.limit_to(limit);

    let mut data = Vec::new();
    reader.read_to_end(&mut data).await.unwrap();

    assert_eq!(data, archive[..limit as usize]);
    let ranges: Vec<_> = server.requests()[1..]
        .iter()
        .map(|r| r.span().unwrap())
        .collect();
    assert_eq!(ranges.len(), 6, "{ranges:?}");
    assert!(ranges.iter().all(|&(_, end)| end < limit), "{ranges:?}");
    assert_eq!(ranges.last().unwrap().1, limit - 1);
}

/// Opens a reader whose fetch limit has been lifted, as it is once the directory has been parsed.
async fn lifted_reader(server: &Server, config: ReaderConfig) -> RemoteHttpReader {
    let reader = RemoteHttpReader::open(&server.url, config).await.unwrap();
    reader.fetch_limit().lift();
    reader
}

/// The ranges requested after the tail fetched at open.
fn later_spans(server: &Server) -> Vec<(u64, u64)> {
    server.requests()[1..]
        .iter()
        .map(|r| r.span().unwrap())
        .collect()
}

#[tokio::test]
async fn a_lifted_reader_streams_up_to_its_limit_in_one_request() {
    let archive = large_directory_zip();
    let tail_start = (archive.len() as u64) - TAIL_BYTES;
    let server = serve(archive.clone()).await;
    let mut config = fast_config();
    config.chunk_bytes = 1000;
    let mut reader = lifted_reader(&server, config).await;

    reader.limit_to(5500);
    let mut data = Vec::new();
    reader.read_to_end(&mut data).await.unwrap();
    reader.limit_to(u64::MAX);
    reader.read_to_end(&mut data).await.unwrap();

    assert_eq!(data, archive);
    assert_eq!(later_spans(&server), [(0, 5499), (5500, tail_start - 1)]);
}

#[tokio::test]
async fn seeking_to_the_current_position_keeps_the_stream() {
    let archive = large_directory_zip();
    let tail_start = (archive.len() as u64) - TAIL_BYTES;
    let server = serve(archive.clone()).await;
    let mut reader = lifted_reader(&server, fast_config()).await;

    let mut start = vec![0u8; 600_000];
    reader.read_exact(&mut start).await.unwrap();
    reader.seek(SeekFrom::Start(600_000)).await.unwrap();
    reader.seek(SeekFrom::Current(0)).await.unwrap();
    let mut more = vec![0u8; 1000];
    reader.read_exact(&mut more).await.unwrap();
    assert_eq!(later_spans(&server), [(0, tail_start - 1)]);

    reader.seek(SeekFrom::Start(10)).await.unwrap();
    reader.read_exact(&mut more).await.unwrap();

    assert_eq!(start, archive[..600_000]);
    assert_eq!(more, archive[10..1010]);
    assert_eq!(
        later_spans(&server),
        [(0, tail_start - 1), (10, tail_start - 1)]
    );
}

#[tokio::test]
async fn an_interrupted_stream_resumes_where_it_stopped_each_time() {
    let archive = large_directory_zip();
    // More interruptions than retries are allowed, but each one makes progress.
    let server = serve_with(
        archive.clone(),
        Behavior {
            truncate_gets: Some((1..5, 1000)),
            ..Default::default()
        },
    )
    .await;
    let mut reader = lifted_reader(&server, fast_config()).await;
    reader.limit_to(10_000);

    let mut data = Vec::new();
    reader.read_to_end(&mut data).await.unwrap();

    assert_eq!(data, archive[..10_000]);
    let starts: Vec<u64> = later_spans(&server).iter().map(|span| span.0).collect();
    assert_eq!(starts, [0, 1000, 2000, 3000, 4000]);
    assert!(later_spans(&server).iter().all(|span| span.1 == 9999));
}

#[tokio::test]
async fn a_stream_failing_without_progress_gives_up_after_retrying() {
    let server = serve_with(
        large_directory_zip(),
        Behavior {
            truncate_gets: Some((1..100, 0)),
            ..Default::default()
        },
    )
    .await;
    let mut reader = lifted_reader(&server, fast_config()).await;

    let mut data = vec![0u8; 64];
    let error = Error::from(reader.read_exact(&mut data).await.unwrap_err());

    assert!(matches!(error, Error::TruncatedResponse), "{error}");
    assert_eq!(server.gets(), 1 + 3);
}

#[tokio::test]
async fn a_stream_claiming_less_than_it_sends_is_refused_without_reading_it_all() {
    let archive = large_directory_zip();
    let tail_start = (archive.len() as u64) - TAIL_BYTES;
    for no_content_length in [false, true] {
        let server = serve_with(
            archive.clone(),
            Behavior {
                overlong_body: Some((1, 600_000)),
                no_content_length,
                ..Default::default()
            },
        )
        .await;
        let config = fast_config();
        let network = config.network_bytes.clone();
        let mut reader = lifted_reader(&server, config).await;
        let opened_with = network.load(Ordering::Relaxed);

        let mut data = Vec::new();
        let error = Error::from(reader.read_to_end(&mut data).await.unwrap_err());

        assert!(matches!(error, Error::InvalidResponse(_)), "{error}");
        assert_eq!(server.gets(), 2);
        let used = network.load(Ordering::Relaxed) - opened_with;
        if no_content_length {
            assert!(used < tail_start, "used {used} bytes");
        } else {
            assert_eq!(used, 0, "refused before reading the body");
        }
    }
}

#[tokio::test]
async fn a_file_changing_before_a_resumed_request_is_detected() {
    let server = serve_with(
        large_directory_zip(),
        Behavior {
            etag: Some("\"v1\""),
            change_etag_after: Some(2),
            truncate_gets: Some((1..2, 1000)),
            ..Default::default()
        },
    )
    .await;
    let mut reader = lifted_reader(&server, fast_config()).await;

    let mut data = Vec::new();
    let error = Error::from(reader.read_to_end(&mut data).await.unwrap_err());

    assert!(matches!(error, Error::RemoteChanged), "{error}");
    assert_eq!(later_spans(&server)[1].0, 1000);
    assert_eq!(server.gets(), 3);
}

#[tokio::test]
async fn a_stalled_stream_times_out_and_resumes() {
    let archive = large_directory_zip();
    let server = serve_with(
        archive.clone(),
        Behavior {
            stall_body: Some((1, 5000)),
            ..Default::default()
        },
    )
    .await;
    let mut reader = lifted_reader(&server, fast_config()).await;

    let mut data = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), reader.read_to_end(&mut data))
        .await
        .expect("the read timeout should end the stall")
        .unwrap();

    assert_eq!(data, archive);
    assert_eq!(later_spans(&server)[1].0, 5000);
}

#[tokio::test]
async fn a_stalled_stream_fails_when_no_retries_are_left() {
    let server = serve_with(
        large_directory_zip(),
        Behavior {
            stall_body: Some((1, 5000)),
            ..Default::default()
        },
    )
    .await;
    let mut config = fast_config();
    config.retry.max_attempts = 1;
    let mut reader = lifted_reader(&server, config).await;

    let mut data = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(15), reader.read_to_end(&mut data))
        .await
        .expect("the read timeout should end the stall");

    let error = Error::from(result.unwrap_err());
    assert!(matches!(error, Error::Http(_)), "{error}");
    assert_eq!(data.len(), 5000);
    assert_eq!(server.gets(), 2);
}

/// A reader config with `headers` that fails fast.
fn config_with_headers(headers: &[(&str, &str)]) -> ReaderConfig {
    let mut config = ReaderConfig::with_headers(headers.iter().copied()).unwrap();
    config.retry = fast_config().retry;
    config
}

#[tokio::test]
async fn custom_headers_are_sent_with_every_request() {
    let server = serve_with(
        large_directory_zip(),
        Behavior {
            truncate_gets: Some((1..2, 1000)),
            ..Default::default()
        },
    )
    .await;
    let config = config_with_headers(&[("X-Token", " secret "), ("Authorization", "Bearer t")]);
    let mut reader = lifted_reader(&server, config).await;

    let mut data = vec![0u8; 5000];
    reader.read_exact(&mut data).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    for request in &requests {
        assert_eq!(request.header("x-token"), Some("secret"), "{request:?}");
        assert_eq!(request.header("authorization"), Some("Bearer t"));
    }
}

#[tokio::test]
async fn headers_the_client_sets_itself_are_refused() {
    for name in ["Range", "accept-encoding", " ACCEPT-ENCODING "] {
        let result = ReaderConfig::with_headers([(name, "x")]);
        assert!(
            matches!(result.err(), Some(Error::InvalidHeader(ref n)) if n == name),
            "{name}"
        );
    }
}

#[tokio::test]
async fn invalid_header_names_and_values_are_refused() {
    let invalid = [
        ("bad name", "x"),
        ("", "x"),
        ("X-Bad:", "x"),
        ("X-Token", "line\nbreak"),
        ("X-Token", "null\0byte"),
    ];
    for (name, value) in invalid {
        let result = ReaderConfig::with_headers([(name, value)]);
        assert!(
            matches!(result.err(), Some(Error::InvalidHeader(_))),
            "{name:?}: {value:?}"
        );
    }
}

#[tokio::test]
async fn a_redirect_to_another_site_is_refused_when_custom_headers_are_set() {
    let other_site = serve(small_zip()).await;
    let server = serve_with(
        small_zip(),
        Behavior {
            redirect_to: Some(other_site.url.clone()),
            ..Default::default()
        },
    )
    .await;

    let config = config_with_headers(&[("X-Token", "secret")]);
    let error = match RemoteHttpReader::open(&server.url, config).await {
        Err(error) => error,
        Ok(_) => panic!("expected an error"),
    };

    assert!(
        error.to_string().contains("redirected to another site"),
        "{error}"
    );
    assert_eq!(other_site.gets(), 0);
    assert_eq!(server.gets(), 1);
}

#[tokio::test]
async fn a_redirect_within_the_same_site_keeps_custom_headers() {
    let server = serve_with(
        small_zip(),
        Behavior {
            redirect_to: Some("/moved.zip".to_string()),
            ..Default::default()
        },
    )
    .await;

    let config = config_with_headers(&[("X-Token", "secret")]);
    let reader = RemoteHttpReader::open(&server.url, config).await.unwrap();
    let archive = RemoteArchive::from_reader(reader).await.unwrap();

    assert_eq!(zip_explorer::scan(&archive).nodes.len(), 3);
    let requests = server.requests();
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/test.zip", "/moved.zip"]);
    assert!(requests
        .iter()
        .all(|r| r.header("x-token") == Some("secret")));
}

#[tokio::test]
async fn a_redirect_to_another_site_drops_authorization_instead_of_failing() {
    let other_site = serve(small_zip()).await;
    let server = serve_with(
        small_zip(),
        Behavior {
            redirect_to: Some(other_site.url.clone()),
            ..Default::default()
        },
    )
    .await;

    let config = config_with_headers(&[("Authorization", "Bearer secret")]);
    RemoteHttpReader::open(&server.url, config).await.unwrap();

    assert_eq!(
        server.requests()[0].header("authorization"),
        Some("Bearer secret")
    );
    let followed = other_site.requests();
    assert_eq!(followed.len(), 1);
    assert_eq!(followed[0].header("authorization"), None);
}

// Regression budgets for the product's core promise. They count bytes, so they aren't flaky.

#[tokio::test]
async fn listing_stays_cheap() {
    let archive = large_directory_zip();
    let directory_bytes = archive.len() / 2 + 1;
    let padded = {
        let mut entries: Vec<Entry> = (0..5000)
            .map(|i| stored(&format!("folder/entry-with-a-long-name-{i:05}.txt"), "x"))
            .collect();
        entries.push(stored("big.bin", vec![0u8; 100 * 1024 * 1024]));
        build_zip(&entries)
    };
    let server = serve(padded.clone()).await;
    let config = fast_config();
    let network = config.network_bytes.clone();
    let reader = RemoteHttpReader::open(&server.url, config).await.unwrap();
    let archive = RemoteArchive::from_reader(reader).await.unwrap();
    let result = zip_explorer::scan(&archive);

    assert_eq!(result.nodes.len(), 5002);
    let used = network.load(Ordering::Relaxed);
    assert!(used < 2 * 1024 * 1024, "listing used {used} bytes");
    assert_eq!(server.gets(), 2, "{:?}", server.requests());
    assert_eq!(result.fetched_size, used);
    assert!(directory_bytes > 0 && (used as usize) < padded.len() / 50);
}

#[tokio::test]
async fn directory_parsing_cannot_fetch_without_limit() {
    // The directory lies about where it starts, so the parser reads from the start of a huge file.
    // A record that can't be parsed must fail promptly rather than reading everything.
    let mut archive = vec![b'x'; 8 * 1024 * 1024];
    let eocd_offset = archive.len();
    archive.extend_from_slice(b"PK\x05\x06");
    archive.extend_from_slice(&[0; 4]);
    archive.extend_from_slice(&2u16.to_le_bytes());
    archive.extend_from_slice(&2u16.to_le_bytes());
    archive.extend_from_slice(&(eocd_offset as u32).to_le_bytes());
    archive.extend_from_slice(&0u32.to_le_bytes());
    archive.extend_from_slice(&[0; 2]);
    let server = serve(archive).await;
    assert!(scan(&server.url).await.is_err());
    assert!(
        server.body_bytes() < 2 * 1024 * 1024,
        "{}",
        server.body_bytes()
    );
}

/// Enough entries with long names for a directory of several chunks.
fn many_long_names() -> Vec<Entry> {
    (0..30_000)
        .map(|i| {
            let name =
                format!("folder/an-entry-whose-name-is-long-enough-to-fill-chunks-{i:05}.txt");
            stored(&name, "x")
        })
        .collect()
}

/// Lists the archive, returning the result and the bytes received.
async fn list_counting(server: &Server, config: ReaderConfig) -> (Result<ScanResult, Error>, u64) {
    let network = config.network_bytes.clone();
    let result = async {
        let reader = RemoteHttpReader::open(&server.url, config).await?;
        let archive = RemoteArchive::from_reader(reader).await?;
        Ok(zip_explorer::scan(&archive))
    }
    .await;
    (result, network.load(Ordering::Relaxed))
}

#[tokio::test]
async fn a_directory_larger_than_a_chunk_is_fetched_in_one_request() {
    let archive = build_zip(&many_long_names());
    let (_, size, offset) = end_record(&archive);
    let (len, offset) = (archive.len() as u64, u64::from(offset));
    assert!(u64::from(size) > 3 * DEFAULT_CHUNK_BYTES);
    let server = serve(archive).await;

    let (result, used) = list_counting(&server, fast_config()).await;

    assert_eq!(result.unwrap().nodes.len(), 30_001);
    assert_eq!(later_spans(&server), [(offset, len - TAIL_BYTES - 1)]);
    assert_eq!(used, len - offset);
}

#[tokio::test]
async fn a_zip64_directory_is_fetched_in_one_request() {
    let entries = many_long_names();
    let offset: u64 = entries.iter().map(local_span).sum();
    let archive = build_zip64(&entries);
    let len = archive.len() as u64;
    let server = serve(archive).await;

    let (result, used) = list_counting(&server, fast_config()).await;

    let result = result.unwrap();
    assert_eq!(result.nodes.len(), 30_001);
    assert!(result
        .nodes
        .iter()
        .any(|node| node.path.ends_with("-29999.txt")));
    assert_eq!(later_spans(&server), [(offset, len - TAIL_BYTES - 1)]);
    assert_eq!(used, len - offset);
}

#[tokio::test]
async fn records_past_the_claimed_directory_size_are_fetched_in_chunks() {
    let mut archive = build_zip(&many_long_names());
    let (entries, size, offset) = end_record(&archive);
    set_end_record(&mut archive, entries, size / 2, offset);
    let (len, offset, claimed) = (archive.len() as u64, u64::from(offset), u64::from(size / 2));
    let server = serve(archive).await;
    let mut config = fast_config();
    config.chunk_bytes = 256 * 1024;

    let (result, used) = list_counting(&server, config).await;

    assert_eq!(result.unwrap().nodes.len(), 30_001);
    let spans = later_spans(&server);
    assert_eq!(spans[0], (offset, offset + claimed - 1));
    for pair in spans.windows(2) {
        assert_eq!(pair[1].0, pair[0].1 + 1, "{spans:?}");
        assert!(pair[1].1 - pair[1].0 < 256 * 1024, "{spans:?}");
    }
    assert_eq!(spans.last().unwrap().1, len - TAIL_BYTES - 1);
    assert_eq!(used, len - offset);
}

#[tokio::test]
async fn claiming_more_records_than_there_are_fails_without_fetching_more() {
    let mut archive = build_zip(&many_long_names());
    let (entries, _, offset) = end_record(&archive);
    set_end_record(&mut archive, entries + 1000, 400 * 1024 * 1024, offset);
    let (len, offset) = (archive.len() as u64, u64::from(offset));
    let server = serve(archive).await;

    let (result, used) = list_counting(&server, fast_config()).await;

    assert!(result.is_err());
    assert_eq!(later_spans(&server), [(offset, len - TAIL_BYTES - 1)]);
    assert_eq!(used, len - offset);
}

#[tokio::test]
async fn a_lying_directory_size_is_abandoned_at_the_first_bad_record() {
    let mut archive = vec![b'x'; 8 * 1024 * 1024];
    archive.extend_from_slice(&[0; 22]);
    set_end_record(&mut archive, 1000, 400 * 1024 * 1024, 0);
    let at = archive.len() - 22;
    archive[at..at + 4].copy_from_slice(b"PK\x05\x06");
    let server = serve(archive).await;

    let (result, used) = list_counting(&server, fast_config()).await;

    assert!(result.is_err());
    assert_eq!(server.gets(), 2, "{:?}", server.requests());
    assert!(used < TAIL_BYTES + DEFAULT_CHUNK_BYTES, "used {used} bytes");
}

#[tokio::test]
async fn a_directory_offset_pointing_into_file_data_fails_at_the_first_record() {
    let mut archive = build_zip(&many_long_names());
    let (entries, size, offset) = end_record(&archive);
    set_end_record(&mut archive, entries, size, offset - 1_000_000);
    let server = serve(archive).await;

    let (result, used) = list_counting(&server, fast_config()).await;

    assert!(result.is_err());
    assert_eq!(server.gets(), 2, "{:?}", server.requests());
    assert!(used < TAIL_BYTES + DEFAULT_CHUNK_BYTES, "used {used} bytes");
}
