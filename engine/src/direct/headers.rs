use super::*;

pub(super) fn cross_origin_referer(headers: &HeaderMap, destination: &Url) -> Option<HeaderValue> {
    let value = headers.get(header::REFERER)?.to_str().ok()?;
    let referer = Url::parse(value).ok()?;
    let host = referer.host_str()?.trim_end_matches('.');
    if !matches!(referer.scheme(), "http" | "https")
        || !referer.username().is_empty()
        || referer.password().is_some()
        || !super::playlist::public_host(&referer)
        || host.eq_ignore_ascii_case("localhost")
        || [".localhost", ".local", ".internal", ".localdomain"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
        || (referer.scheme() == "https" && destination.scheme() != "https")
    {
        return None;
    }
    HeaderValue::from_str(&format!("{}/", referer.origin().ascii_serialization())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_only_reference_never_contains_viewer_path_query_or_fragment() {
        let headers = HeaderMap::from_iter([(
            header::REFERER,
            HeaderValue::from_static("https://player.example/watch/private?token=fixture#private"),
        )]);
        let result =
            cross_origin_referer(&headers, &Url::parse("https://cdn.example/asset").unwrap());
        assert_eq!(result.unwrap(), "https://player.example/");
    }

    #[test]
    fn credentialed_local_non_http_and_downgraded_references_are_not_forwarded() {
        let destination = Url::parse("https://cdn.example/asset").unwrap();
        for value in [
            "https://user:secret@player.example/watch",
            "http://127.0.0.1/private",
            "http://[::1]/private",
            "http://192.168.1.1/private",
            "http://169.254.169.254/private",
            "http://service.internal/private",
            "http://service.local./private",
            "file:///private/secret",
            "not a URL",
        ] {
            let headers =
                HeaderMap::from_iter([(header::REFERER, HeaderValue::from_str(value).unwrap())]);
            assert!(cross_origin_referer(&headers, &destination).is_none());
        }
        let headers = HeaderMap::from_iter([(
            header::REFERER,
            HeaderValue::from_static("https://player.example/"),
        )]);
        assert!(
            cross_origin_referer(&headers, &Url::parse("http://cdn.example/asset").unwrap())
                .is_none()
        );
    }
}
