//! Isolated document for generated widgets and tool workers.

use tauri::http::{header, Method, Request, Response, StatusCode};

const CSP: &str = "default-src 'none'; script-src 'unsafe-inline' blob:; style-src 'unsafe-inline'; img-src data: blob:; worker-src blob:; connect-src 'none'; base-uri 'none'; form-action 'none'; sandbox allow-scripts";

const BOOTSTRAP: &str = r#"<!doctype html><html><head><meta charset="utf-8"><meta name="referrer" content="no-referrer"></head><body><script>
(() => {
  if (parent === window) return;
  const initialize = ({ source, data }) => {
    if (source !== parent || data?.kind !== 'cody-sandbox-init' || typeof data.source !== 'string' || data.source.length > 2097152) return;
    window.removeEventListener('message', initialize);
    const script = document.createElement('script');
    script.textContent = data.source;
    document.body.append(script);
  };
  window.addEventListener('message', initialize);
  parent.postMessage({ kind: 'cody-sandbox-ready' }, '*');
})();
</script></body></html>"#;

pub(crate) fn response(request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let uri = request.uri();
    let allowed = uri.scheme_str() == Some("cody-sandbox")
        && uri.authority().map(|authority| authority.as_str()) == Some("localhost")
        && uri.path() == "/"
        && uri.query().is_none()
        && (request.method() == Method::GET || request.method() == Method::HEAD);
    let status = if allowed {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let body = if request.method() == Method::HEAD {
        Vec::new()
    } else if allowed {
        BOOTSTRAP.as_bytes().to_vec()
    } else {
        b"Not found".to_vec()
    };
    Response::builder()
        .status(status)
        .header(
            header::CONTENT_TYPE,
            if allowed {
                "text/html; charset=utf-8"
            } else {
                "text/plain; charset=utf-8"
            },
        )
        .header(header::CONTENT_SECURITY_POLICY, CSP)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(body)
        .expect("static sandbox response headers are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(uri: &str) -> Request<Vec<u8>> {
        Request::builder().uri(uri).body(Vec::new()).unwrap()
    }

    #[test]
    fn sandbox_has_its_own_restricted_policy_and_parent_only_bootstrap() {
        let response = response(request("cody-sandbox://localhost/"));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_SECURITY_POLICY], CSP);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let html = std::str::from_utf8(response.body()).unwrap();
        assert!(html.contains("source !== parent"));
        assert!(html.contains("removeEventListener('message', initialize)"));
        assert!(html.contains("cody-sandbox-ready"));
        assert!(!html.contains("__TAURI"));
    }

    #[test]
    fn sandbox_never_serves_arbitrary_hosts_paths_or_requests() {
        for uri in [
            "cody-sandbox://other/",
            "cody-sandbox://localhost:80/",
            "cody-sandbox://localhost/anything",
            "cody-sandbox://localhost/?source=alert(1)",
            "https://localhost/",
        ] {
            assert_eq!(
                response(request(uri)).status(),
                StatusCode::NOT_FOUND,
                "{uri}"
            );
        }
        let post = Request::builder()
            .method(Method::POST)
            .uri("cody-sandbox://localhost/")
            .body(Vec::new())
            .unwrap();
        assert_eq!(response(post).status(), StatusCode::NOT_FOUND);
        let head = Request::builder()
            .method(Method::HEAD)
            .uri("cody-sandbox://localhost/")
            .body(Vec::new())
            .unwrap();
        let response = response(head);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.body().is_empty());
    }
}
