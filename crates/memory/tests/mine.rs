//! Mining a transcript through a fake gateway: the model's answer becomes entries at raw
//! tier and low confidence, with the transcript as source.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

fn memory() -> Command {
    Command::new(env!("CARGO_BIN_EXE_amem"))
}

/// A one-request HTTP server that answers like an OpenAI-compatible gateway with two
/// proposals, one of which is too thin to keep.
fn fake_gateway() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(1) {
            let mut s = stream.unwrap();
            let mut buf = vec![0u8; 65536];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("POST /v1/chat/completions"), "{req}");
            assert!(
                req.contains("Bearer gw-key"),
                "the key the egress injected: {req}"
            );
            let proposals = r#"[{"kind":"fact","title":"The import has five steps","body":"Steps one to five of the import; the fourth, digests, is the slow one and the one that failed before."},{"kind":"decision","title":"x","body":"thin"}]"#;
            let body = serde_json::json!({
                "model": "fake-mini",
                "choices": [{ "message": { "content": format!("```json\n{proposals}\n```") } }],
                "usage": { "prompt_tokens": 300, "completion_tokens": 60 }
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
        }
    });
    format!("http://{addr}")
}

fn init_root(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("memory");
    let out = memory()
        .args([
            "--root",
            root.to_str().unwrap(),
            "init",
            "--owner",
            "human:test:me",
            "--domain",
            "dom-me",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    root
}

#[test]
fn a_transcript_is_mined_through_the_gateway_into_low_confidence_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(
        &transcript,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"Finish the import."}}"#, "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Step 4, digests, failed last time; retrying with a smaller batch."}]}}"#, "\n",
        ),
    )
    .unwrap();
    let gateway = fake_gateway();
    let out = memory()
        .args([
            "--root",
            root.to_str().unwrap(),
            "mine",
            transcript.to_str().unwrap(),
        ])
        .args([
            "--auth",
            "gateway",
            "--provider",
            "llmao",
            "--model",
            "fake-mini",
        ])
        .env("LLMAO_URL", &gateway)
        .env("LLMAO_KEY", "gw-key")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let ids: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ids.len(), 1, "the thin proposal is dropped: {stderr}");
    assert!(stderr.contains("mined fact"), "{stderr}");

    let entry = memory()
        .args(["--root", root.to_str().unwrap(), "get", &ids[0]])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&entry.stdout);
    assert!(text.contains("confidence: low"), "{text}");
    assert!(
        text.contains("transcript:"),
        "the transcript is the source: {text}"
    );
    assert!(text.contains("digests"), "{text}");
    assert!(text.contains("Mined from a session transcript"), "{text}");

    // the call was metered against the gateway destination
    let usage = memory()
        .args(["--root", root.to_str().unwrap(), "usage"])
        .output()
        .unwrap();
    let u = String::from_utf8_lossy(&usage.stdout);
    assert!(u.contains("llmao") && u.contains("fake-mini"), "{u}");
}
