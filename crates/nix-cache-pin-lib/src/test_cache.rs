//! Valid cache metadata for orchestration tests; availability cannot be mocked
//! with HTTP headers alone.
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

pub(crate) async fn hit(server: &MockServer, store_path: &str) {
    let hash = crate::narinfo::store_path_narinfo_hash(store_path);
    Mock::given(method("GET"))
        .and(path(format!("/{hash}.narinfo")))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "StorePath: {store_path}\nURL: nar/test.nar\nNarHash: sha256:{}\nNarSize: 1\nReferences: \n",
            "0".repeat(52)
        )))
        .mount(server)
        .await;
}
