//! Default clients keep HTTP usable when a runtime has no platform trust store.

/// Build a default client without panicking on unavailable platform roots.
///
/// Platform verification remains the first choice. If initialization fails,
/// retry with an empty explicit trust store: plain HTTP still works, while
/// HTTPS cannot authenticate any server certificate. This never disables
/// certificate or hostname verification. Other initialization failures remain
/// ordinary errors. Callers with private roots can still inject their own client.
pub fn client() -> crate::error::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(redirects())
        .build()
        .or_else(|_| {
            reqwest::Client::builder()
                .redirect(redirects())
                .tls_certs_only(std::iter::empty())
                .build()
        })?)
}

/// reqwest's default policy, except that a redirect to the compromised cache
/// is refused before it is contacted: lookups refuse the host itself, but a
/// configured mirror could still redirect there. An injected client keeps its
/// own policy.
fn redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if crate::config::is_compromised_cache(attempt.url().as_str()) {
            attempt.error("refusing a redirect to a compromised cache")
        } else {
            reqwest::redirect::Policy::default().redirect(attempt)
        }
    })
}

#[cfg(test)]
mod tests {
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn redirects_to_the_compromised_cache_are_refused() {
        let mirror = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hash.narinfo"))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "location",
                "https://attic.xuyh0120.win/lantian/hash.narinfo",
            ))
            .mount(&mirror)
            .await;
        let error = super::client()
            .unwrap()
            .get(format!("{}/hash.narinfo", mirror.uri()))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_redirect(), "{error}");
        assert!(format!("{error:?}").contains("compromised"), "{error:?}");
    }
}
