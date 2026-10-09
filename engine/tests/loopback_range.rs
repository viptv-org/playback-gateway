#![cfg(feature = "range")]
use playback_engine::{LoopbackStream, TorrentRangeSource};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

struct Bytes {
    data: Vec<u8>,
    closed: AtomicBool,
}
impl TorrentRangeSource for Bytes {
    fn file_size(&self, index: u32) -> Option<u64> {
        (index == 0).then_some(self.data.len() as u64)
    }
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    fn read_range(
        &self,
        _: u32,
        offset: u64,
        length: u32,
    ) -> futures::future::BoxFuture<'_, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let start = usize::try_from(offset).map_err(|_| "invalid offset")?;
            let end = start
                .checked_add(length as usize)
                .ok_or("invalid length")?
                .min(self.data.len());
            Ok(self.data.get(start..end).ok_or("invalid range")?.to_vec())
        })
    }
}

#[tokio::test]
async fn private_http_ranges_preserve_bytes_and_revoke_existing_access() {
    let source = Arc::new(Bytes {
        data: (0..200_003).map(|n| (n % 251) as u8).collect(),
        closed: AtomicBool::new(false),
    });
    let stream = LoopbackStream::from_source(source.clone()).await.unwrap();
    let url = stream.url(0);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .get(&url)
        .header("Range", "bytes=65530-131079")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers()["content-range"],
        "bytes 65530-131079/200003"
    );
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        &source.data[65530..131080]
    );
    let response = client
        .get(&url)
        .header("Range", "bytes=-17")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        &source.data[199986..]
    );
    let response = client.head(&url).send().await.unwrap();
    assert_eq!(response.headers()["content-length"], "200003");
    assert!(response.bytes().await.unwrap().is_empty());
    assert_eq!(
        client
            .get(&url)
            .header("Range", "bytes=200003-")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::RANGE_NOT_SATISFIABLE
    );
    let mut wrong = url::Url::parse(&url).unwrap();
    wrong.set_path("/invalid/0/stream.bin");
    assert_eq!(
        client.get(wrong).send().await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
    source.closed.store(true, Ordering::Release);
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        reqwest::StatusCode::GONE
    );
    assert!(
        stream
            .stop_and_wait_until(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
            .await
    );
    assert!(!stream.has_pending_work());
    assert!(client.get(&url).send().await.is_err());
}
