//! The recall deputy behind the egress: a small model the server may spend on a
//! session's behalf, reached through the session's own destination with the same
//! ceiling check, credential injection and usage record as the harness's calls. The
//! launcher builds one when the route gives it a credential of its own, a key or the
//! gateway; a subscription login's credential is the harness's alone, and no deputy is
//! built.

use crate::model;
use dm_egress::Egress;
use dm_server::recall::Deputy;
use std::future::Future;
use std::pin::Pin;

/// A deputy on one destination and model.
pub struct EgressDeputy {
    egress: Egress,
    destination: String,
    /// The model asked for, if any; otherwise resolved on first use.
    given: Option<String>,
    model: tokio::sync::OnceCell<String>,
}

impl EgressDeputy {
    /// A deputy through `egress` to `destination`, on `model` or, resolved when it is
    /// first asked, the destination's small default ([`model::resolve`]).
    #[must_use]
    pub fn new(egress: Egress, destination: &str, model: Option<&str>) -> EgressDeputy {
        EgressDeputy {
            egress,
            destination: destination.to_string(),
            given: model.map(str::to_string),
            model: tokio::sync::OnceCell::new(),
        }
    }

    async fn model_now(&self) -> &str {
        self.model
            .get_or_init(|| model::resolve(&self.egress, &self.destination, self.given.as_deref()))
            .await
    }
}

impl Deputy for EgressDeputy {
    fn model(&self) -> String {
        self.model.get().cloned().unwrap_or_else(|| {
            self.given
                .clone()
                .unwrap_or_else(|| model::default_model(&self.destination).to_string())
        })
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let m = self.model_now().await;
            let (path, body) = model::request(&self.destination, m, prompt);
            let answer = self
                .egress
                .call(&self.destination, &path, &body)
                .await
                .map_err(|e| e.to_string())?;
            let text = model::answer_text(&self.destination, &answer);
            if text.trim().is_empty() {
                return Err(format!(
                    "{} answered without text: {answer}",
                    self.destination
                ));
            }
            Ok(text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};
    use dm_egress::{Auth, Destination};
    use dm_server::Memory;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// A fake chat-completions upstream through the egress, under the starter grants'
    /// `openai` ceiling.
    async fn egress_to(answer: serde_json::Value) -> (tempfile::TempDir, Egress) {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let answer = answer.clone();
                async move { Json(answer) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(Memory::init(dir.path(), "human:test:me", "dom-me").unwrap());
        let mut destinations = HashMap::new();
        destinations.insert(
            "openai".to_string(),
            Destination::forward(
                format!("http://{addr}"),
                Auth::Inject {
                    header: axum::http::header::AUTHORIZATION,
                    value: "Bearer sk-test".into(),
                },
            ),
        );
        let egress = Egress::new(memory, destinations, &dir.path().join("t.db")).unwrap();
        (dir, egress)
    }

    #[tokio::test]
    async fn the_deputy_asks_through_the_egress_and_reads_the_answer() {
        let (_dir, egress) = egress_to(serde_json::json!({
            "model": "gpt-5-mini",
            "choices": [{ "message": { "content": "{\"brief\": \"R retries.\"}" } }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
        }))
        .await;
        let deputy = EgressDeputy::new(egress.clone(), "openai", None);
        assert_eq!(
            deputy.model(),
            "gpt-5-mini",
            "the destination's small default"
        );
        let text = deputy.complete("how does R retry").await.unwrap();
        assert!(text.contains("R retries"));
        let rows = egress.telemetry().summary().unwrap();
        assert_eq!(rows[0].destination, "openai", "the call was metered");
        assert_eq!(rows[0].input_tokens, 12);
        assert_eq!(
            EgressDeputy::new(egress, "openai", Some("m-x")).model(),
            "m-x"
        );
    }

    #[tokio::test]
    async fn an_answer_without_text_is_an_error_with_the_body_in_it() {
        let (_dir, egress) = egress_to(serde_json::json!({ "error": "overloaded" })).await;
        let deputy = EgressDeputy::new(egress, "openai", None);
        let err = deputy.complete("anything").await.unwrap_err();
        assert!(
            err.contains("without text") && err.contains("overloaded"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_destination_the_egress_lacks_is_refused() {
        let (_dir, egress) = egress_to(serde_json::json!({})).await;
        let deputy = EgressDeputy::new(egress, "xai", None);
        let err = deputy.complete("anything").await.unwrap_err();
        assert!(err.contains("xai"), "{err}");
    }
}
