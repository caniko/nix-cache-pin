//! Default clients keep HTTP usable when a runtime has no platform trust store.

/// Build a default client without panicking on unavailable platform roots.
///
/// Platform verification remains the first choice. If initialization fails,
/// retry with an empty explicit trust store: plain HTTP still works, while
/// HTTPS cannot authenticate any server certificate. This never disables
/// certificate or hostname verification. Other initialization failures remain
/// ordinary errors. Callers with private roots can still inject their own client.
pub fn client() -> crate::error::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().build().or_else(|_| {
        reqwest::Client::builder()
            .tls_certs_only(std::iter::empty())
            .build()
    })?)
}
