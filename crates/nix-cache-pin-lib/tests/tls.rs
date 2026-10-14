//! Bound the backend migration to certificate validation and real ALPN behavior.
use std::{convert::Infallible, sync::Arc, time::Duration};

use http_body_util::Full;
use hyper::{body::Bytes, service::service_fn, Response};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use reqwest::{Certificate, Client, Version};
use tokio::net::TcpListener;
use tokio_rustls::{
    rustls::{pki_types::PrivatePkcs8KeyDer, ServerConfig},
    TlsAcceptor,
};

async fn server(http2: bool) -> (String, Certificate, tokio::task::JoinHandle<()>) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("fixture certificate");
    let certificate = Certificate::from_der(cert.der()).expect("client root");
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
        )
        .expect("server certificate");
    config.alpn_protocols = if http2 {
        vec![b"h2".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("fixture listener");
    let port = listener.local_addr().expect("local address").port();
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (stream, _) = listener.accept().await.expect("accept");
            // Rejected certificates terminate the handshake on purpose.
            if let Ok(stream) = TlsAcceptor::from(Arc::new(config)).accept(stream).await {
                let service = service_fn(|_| async {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                        b"cache metadata",
                    ))))
                });
                let _ = Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            }
        })
        .await
        .expect("bounded server lifetime");
    });
    (format!("https://localhost:{port}/"), certificate, task)
}

fn builder() -> reqwest::ClientBuilder {
    Client::builder().no_proxy().timeout(Duration::from_secs(5))
}

#[tokio::test]
async fn platform_verifier_rejects_untrusted_certificate() {
    let (url, _, server) = server(false).await;
    let error = builder()
        .build()
        .expect("default client")
        .get(url)
        .send()
        .await
        .expect_err("untrusted root");
    assert!(
        error.is_connect(),
        "certificate failure must be a connection error: {error}"
    );
    server.await.expect("server task");
}

#[tokio::test]
async fn configured_trust_keeps_http1_and_negotiates_http2() {
    for (http2, expected) in [(false, Version::HTTP_11), (true, Version::HTTP_2)] {
        let (url, certificate, server) = server(http2).await;
        let client = builder()
            .tls_certs_merge([certificate])
            .build()
            .expect("trusted client");
        let response = client.get(url).send().await.expect("trusted TLS request");
        assert_eq!(response.version(), expected);
        assert_eq!(
            response.text().await.expect("streamed body"),
            "cache metadata"
        );
        drop(client);
        server.await.expect("server task");
    }
}

#[tokio::test]
async fn configured_root_does_not_bypass_hostname_validation() {
    let (url, certificate, server) = server(false).await;
    let client = builder()
        .tls_certs_merge([certificate])
        .build()
        .expect("trusted client");
    let error = client
        .get(url.replace("localhost", "127.0.0.1"))
        .send()
        .await
        .expect_err("wrong hostname");
    assert!(
        error.is_connect(),
        "hostname failure must be a connection error: {error}"
    );
    server.await.expect("server task");
}

#[tokio::test]
async fn explicit_http_proxy_still_routes_requests() {
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };
    let proxy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/nix-cache-info"))
        .respond_with(ResponseTemplate::new(200).set_body_string("StoreDir: /nix/store\n"))
        .expect(1)
        .mount(&proxy)
        .await;
    let client = builder()
        .proxy(reqwest::Proxy::http(proxy.uri()).expect("HTTP proxy"))
        .build()
        .expect("proxy client");
    let response = client
        .get("http://cache.invalid/nix-cache-info")
        .send()
        .await
        .expect("proxy request");
    assert_eq!(
        response.text().await.expect("proxy body"),
        "StoreDir: /nix/store\n"
    );
}
