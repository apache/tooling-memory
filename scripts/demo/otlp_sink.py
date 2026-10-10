"""A collector for the demo: accepts OTLP/HTTP posts on a loopback port and appends
each body to a file, so the act can count the spans that arrived by name. Not a real
collector; point OTEL_EXPORTER_OTLP_ENDPOINT at Jaeger or an OpenTelemetry collector
for that.

    python3 otlp_sink.py <out-file> <port>"""
import http.server, sys
out = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        body = self.rfile.read(n)
        with open(out, "ab") as f:
            f.write(body)
        self.send_response(200); self.send_header("content-type", "application/x-protobuf"); self.end_headers()
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[2])), H).serve_forever()
