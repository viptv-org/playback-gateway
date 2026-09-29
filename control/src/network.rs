//! Loopback-only egress proxy: resolve once, validate every address and connect to
//! the validated socket address. HTTPS stays end-to-end through CONNECT tunnels.
//! Engines must use this proxy for all HTTP(S), including nested media resources.
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinHandle,
    time::timeout,
};

#[derive(Clone, Default)]
pub struct NetworkPolicy {
    #[cfg(test)]
    fixture_loopback: bool,
}
impl NetworkPolicy {
    #[cfg(test)]
    pub(crate) fn loopback_fixture() -> Self {
        Self {
            fixture_loopback: true,
        }
    }
    pub fn permits(&self, address: IpAddr) -> bool {
        #[cfg(test)]
        if self.fixture_loopback && address.is_loopback() {
            return true;
        }
        match address {
            IpAddr::V4(ip) => {
                let [a, b, c, _] = ip.octets();
                !ip.is_private()
                    && !ip.is_loopback()
                    && !ip.is_link_local()
                    && !ip.is_multicast()
                    && !ip.is_broadcast()
                    && !ip.is_unspecified()
                    && a != 0
                    && a < 224
                    && !(a == 100 && (64..=127).contains(&b))
                    && !(a == 192 && b == 0 && (c == 0 || c == 2))
                    && !(a == 192 && b == 88 && c == 99)
                    && !(a == 198 && (b == 18 || b == 19))
                    && !(a == 198 && b == 51 && c == 100)
                    && !(a == 203 && b == 0 && c == 113)
            }
            IpAddr::V6(ip) => {
                if let Some(v4) = ip.to_ipv4_mapped() {
                    return self.permits(IpAddr::V4(v4));
                }
                let segments = ip.segments();
                (segments[0] & 0xe000) == 0x2000
                    && segments[0] != 0x2002
                    && !(segments[0] == 0x2001 && (segments[1] < 0x200 || segments[1] == 0xdb8))
            }
        }
    }
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, ()> {
        if port == 0 {
            return Err(());
        }
        let addresses = timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?
        .take(17)
        .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses.len() > 16
            || addresses.iter().any(|address| !self.permits(address.ip()))
        {
            return Err(());
        }
        Ok(addresses)
    }
}

pub struct EgressProxy {
    address: SocketAddr,
    task: JoinHandle<()>,
}
impl EgressProxy {
    pub async fn start(policy: NetworkPolicy) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(128));
            let mut peers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    incoming=listener.accept()=>{
                        let Ok((stream,_))=incoming else{break};
                        let Ok(permit)=slots.clone().try_acquire_owned() else{drop(stream);continue};
                        let policy=policy.clone();
                        peers.spawn(async move {let _permit=permit;let _=forward(stream,policy).await;});
                    },
                    _=peers.join_next(),if !peers.is_empty()=>{}
                }
            }
        });
        Ok(Self { address, task })
    }
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}
impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn forward(mut downstream: TcpStream, policy: NetworkPolicy) -> Result<(), ()> {
    let mut buffer = Vec::with_capacity(2048);
    let header = timeout(Duration::from_secs(5), async {
        while !buffer.ends_with(b"\r\n\r\n") {
            if buffer.len() >= 16 * 1024 {
                return Err(());
            }
            let byte = downstream.read_u8().await.map_err(|_| ())?;
            buffer.push(byte);
        }
        Ok(())
    })
    .await;
    if !matches!(header, Ok(Ok(()))) {
        return Err(());
    }
    let text = std::str::from_utf8(&buffer).map_err(|_| ())?;
    let mut lines = text.split("\r\n");
    let line = lines.next().ok_or(())?;
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 3 || !matches!(parts[2], "HTTP/1.0" | "HTTP/1.1") {
        return Err(());
    }
    let connect = parts[0] == "CONNECT";
    if !connect && !matches!(parts[0], "GET" | "HEAD") {
        return deny(&mut downstream).await;
    }
    let target = if connect {
        format!("https://{}", parts[1])
    } else {
        parts[1].to_owned()
    };
    let url = url::Url::parse(&target).map_err(|_| ())?;
    if (connect && (url.path() != "/" || url.query().is_some()))
        || (!connect && url.scheme() != "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return deny(&mut downstream).await;
    }
    let host = match url.host().ok_or(())? {
        url::Host::Domain(host) => host.to_owned(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(ip) => ip.to_string(),
    };
    let addresses = match policy
        .resolve(&host, url.port_or_known_default().ok_or(())?)
        .await
    {
        Ok(value) => value,
        Err(_) => return deny(&mut downstream).await,
    };
    let mut upstream = None;
    for address in addresses {
        if let Ok(Ok(stream)) = timeout(Duration::from_secs(3), TcpStream::connect(address)).await {
            upstream = Some(stream);
            break;
        }
    }
    let Some(mut upstream) = upstream else {
        return deny(&mut downstream).await;
    };
    if connect {
        downstream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .map_err(|_| ())?;
    } else {
        let target = match url.query() {
            Some(query) => format!("{}?{}", url.path(), query),
            None => url.path().to_owned(),
        };
        let mut request = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            parts[0],
            target,
            url[url::Position::BeforeHost..url::Position::AfterPort].to_owned()
        );
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').ok_or(())?;
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "host" | "connection" | "proxy-connection" | "proxy-authorization" | "keep-alive"
            ) {
                continue;
            }
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "content-length" | "transfer-encoding" | "upgrade"
            ) {
                return deny(&mut downstream).await;
            }
            request.push_str(name);
            request.push(':');
            request.push_str(value);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        upstream
            .write_all(request.as_bytes())
            .await
            .map_err(|_| ())?;
    }
    tokio::io::copy_bidirectional(&mut downstream, &mut upstream)
        .await
        .map_err(|_| ())?;
    Ok(())
}
async fn deny(stream: &mut TcpStream) -> Result<(), ()> {
    stream
        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .map_err(|_| ())?;
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denies_private_metadata_reserved_and_mapped_addresses() {
        let policy = NetworkPolicy::default();
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.1.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "2002:7f00:1::",
            "2001:db8::1",
        ] {
            assert!(!policy.permits(address.parse().unwrap()), "{address}");
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(policy.permits(address.parse().unwrap()));
        }
    }
    #[tokio::test]
    async fn rejects_loopback_without_contacting_it() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = EgressProxy::start(NetworkPolicy::default()).await.unwrap();
        let mut stream = TcpStream::connect(proxy.address).await.unwrap();
        stream
            .write_all(
                format!(
                    "GET http://{}/secret HTTP/1.1\r\nHost: ignored\r\n\r\n",
                    target.local_addr().unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 403"));
        assert!(timeout(Duration::from_millis(30), target.accept())
            .await
            .is_err());
    }
    #[tokio::test]
    async fn fixture_forwarding_preserves_target_without_leaking_proxy_headers() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let receiver = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let count = stream.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]).into_owned();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            request
        });
        let proxy = EgressProxy::start(NetworkPolicy {
            fixture_loopback: true,
        })
        .await
        .unwrap();
        let mut stream = TcpStream::connect(proxy.address).await.unwrap();
        stream.write_all(format!("GET http://{address}/movie?q=1 HTTP/1.1\r\nHost: untrusted\r\nProxy-Authorization: secret\r\nRange: bytes=0-1\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("ok"));
        let request = receiver.await.unwrap();
        assert!(request.starts_with("GET /movie?q=1"));
        assert!(request.contains("Range: bytes=0-1"));
        assert!(!request.contains("secret") && !request.contains("untrusted"));
    }
}
