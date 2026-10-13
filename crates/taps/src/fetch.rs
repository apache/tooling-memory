//! The fetch tool, native: one page fetched for an agent by this binary itself, behind
//! the same gate as the component (PLAN M19).
//!
//! WebAssembly is chosen per tool, not as a rule. Its case is isolation for code the
//! project does not fully trust, connectors others contribute; fetch is this project's
//! own, so a tool server may run it natively and carry no runtime for it. Nothing a
//! caller sees differs: the gate decides every request, each redirect hop a request of
//! its own, the request connects to the addresses the gate was shown
//! ([`crate::pinned`]), a credential the grant names goes on after the decision, and
//! the answer is the one [`dm_fetch::answer`] shapes for the component too, with every
//! URL reached.

use crate::tool::{admit, credential_header, Answered, Call, Gate};
use crate::{Error, Limits};
use dm_fetch::{Config, Fetched};
use http::{HeaderName, HeaderValue};
use http_body_util::BodyExt;
use std::sync::Arc;

/// The tools this binary runs natively, by their names in the grants.
pub const NATIVE: &[&str] = &["fetch"];

/// Whether `tool` is one this binary runs natively.
#[must_use]
pub fn is_native(tool: &str) -> bool {
    NATIVE.contains(&tool)
}

/// One response, as far as the tool reads it.
struct Got {
    status: u16,
    content_type: String,
    location: Option<String>,
    body: Vec<u8>,
    truncated: bool,
}

/// One call under way: the gate, the credential, and what was reached and refused.
struct Fetching {
    tool: String,
    gate: Arc<dyn Gate>,
    credential: Option<(HeaderName, HeaderValue)>,
    reached: Vec<String>,
    refused: Vec<String>,
    hop: u32,
}

impl Fetching {
    /// GET one URL, reading at most `cap` bytes of its body, if the gate lets it go.
    async fn get(&mut self, url: &str, cap: usize) -> Result<Got, String> {
        let (scheme, authority, path) = dm_fetch::split(url)?;
        let uri: http::Uri = format!("{scheme}://{authority}{path}")
            .parse()
            .map_err(|e| format!("{url:?}: {e}"))?;
        let hop = self.hop;
        self.hop += 1;
        let addrs = match admit(self.gate.as_ref(), &self.tool, "GET", &uri, hop).await {
            Ok((addrs, reached)) => {
                self.reached.push(reached);
                addrs
            }
            Err(why) => {
                self.refused.push(why);
                return Err(format!("request to {url}: refused"));
            }
        };
        let body = http_body_util::Empty::<hyper::body::Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync();
        let mut request = http::Request::get(uri)
            .header(http::header::ACCEPT, dm_fetch::ACCEPT)
            .header(http::header::USER_AGENT, dm_fetch::USER_AGENT)
            .body(body)
            .map_err(|e| format!("request to {url}: {e}"))?;
        if let Some((name, value)) = &self.credential {
            request.headers_mut().insert(name.clone(), value.clone());
        }
        let (response, _io) = crate::pinned::send(request, None, &addrs)
            .await
            .map_err(|e| format!("request to {url}: {e}"))?;
        let status = response.status().as_u16();
        // read before the body is streamed: a borrow of the response held across an
        // await would make this future !Send, and the handler needs it Send
        let (content_type, location) = {
            let header = |name: http::header::HeaderName| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            (
                header(http::header::CONTENT_TYPE).unwrap_or_default(),
                header(http::header::LOCATION),
            )
        };
        let mut stream = response.into_body();
        let mut body = Vec::new();
        let mut truncated = false;
        while let Some(frame) = stream.frame().await {
            let frame = frame.map_err(|e| format!("reading {url}: {e}"))?;
            if let Ok(data) = frame.into_data() {
                body.extend_from_slice(&data);
                if body.len() > cap {
                    body.truncate(cap);
                    truncated = true;
                    break;
                }
            }
        }
        Ok(Got {
            status,
            content_type,
            location,
            body,
            truncated,
        })
    }

    /// GET the URL, following redirects up to the limit, and shape the answer.
    async fn follow(&mut self, url: &str, config: &Config) -> Result<String, String> {
        let mut at = url.to_string();
        let mut redirects = Vec::new();
        loop {
            let got = self.get(&at, config.max_bytes).await?;
            if !dm_fetch::is_redirect(got.status) {
                return dm_fetch::answer(&Fetched {
                    url,
                    final_url: &at,
                    status: got.status,
                    content_type: &got.content_type,
                    body: &got.body,
                    truncated: got.truncated,
                    redirects: &redirects,
                });
            }
            let location = got
                .location
                .ok_or_else(|| format!("HTTP {} from {at} without a Location", got.status))?;
            if redirects.len() == config.max_redirects {
                return Err(format!(
                    "more than {} redirects from {url}",
                    config.max_redirects
                ));
            }
            at = dm_fetch::resolve(&at, &location);
            redirects.push(at.clone());
        }
    }
}

/// Fetch once with `gate` deciding each request: the tool's JSON, and every URL
/// reached, as [`crate::tool::call_gated`] answers for the component.
///
/// # Errors
///
/// As [`crate::tool::call_gated`]: the wall clock, a request the gate refused, or the
/// tool's own failure (a status of 400 or above, a body that is not text, too many
/// redirects).
pub async fn call(gate: Arc<dyn Gate>, c: Call<'_>, limits: Limits) -> Result<Answered, Error> {
    let config = Config::parse(c.config).map_err(Error::Tap)?;
    let args = dm_fetch::Args::parse(c.args).map_err(Error::Tap)?;
    let mut fetching = Fetching {
        tool: c.tool.to_string(),
        gate,
        credential: credential_header(c.grant, c.credential)?,
        reached: Vec::new(),
        refused: Vec::new(),
        hop: 0,
    };
    let outcome = tokio::time::timeout(limits.wall, fetching.follow(&args.url, &config)).await;
    match outcome {
        Err(_) => Err(Error::Runtime(format!(
            "the call exceeded its wall clock of {:?}",
            limits.wall
        ))),
        Ok(Ok(answer)) => Ok(Answered {
            answer,
            reached: fetching.reached,
        }),
        Ok(Err(e)) if fetching.refused.is_empty() => Err(Error::Tap(e)),
        Ok(Err(e)) => Err(Error::Refused(fetching.refused.join("; "), e)),
    }
}
