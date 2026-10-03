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
    model: String,
}

impl EgressDeputy {
    /// A deputy through `egress` to `destination`, on `model` or the destination's
    /// small default.
    #[must_use]
    pub fn new(egress: Egress, destination: &str, model: Option<&str>) -> EgressDeputy {
        EgressDeputy {
            egress,
            destination: destination.to_string(),
            model: model.map_or_else(
                || model::default_model(destination).to_string(),
                str::to_string,
            ),
        }
    }
}

impl Deputy for EgressDeputy {
    fn model(&self) -> String {
        self.model.clone()
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let (path, body) = model::request(&self.destination, &self.model, prompt);
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
