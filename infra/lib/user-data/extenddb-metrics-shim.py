#!/usr/bin/env python3
"""extenddb-metrics-shim — translate ExtendDB JSON /metrics to Prometheus.

Runs on the SUT, scrapes https://127.0.0.1:8000/metrics?window=LastMinute on
each Prometheus scrape, and serves Prometheus exposition on :9101.

Why a custom shim instead of prometheus-json-exporter?
The live JSON shape (single `metrics` array with tagged-enum `dimensions`,
no `le` buckets, no populated percentiles) doesn't fit the off-the-shelf
exporter's mapping language cleanly. See docs/v0.3-status.md (D1).

Emitted families per `MetricSnapshot`:
  <name>_count     counter -- JSON `count`
  <name>_sum_us    counter -- JSON `sum` (micros for latency, raw for everything else)
  <name>_max_us    gauge   -- JSON `max`
  <name>_min_us    gauge   -- JSON `min`

Labels: operation, table_name, index_name (only when the dim is present).
"""

import json
import ssl
import sys
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

import os

# Last5Minutes is the smallest window that returns reliably populated
# `metrics` snapshots from ExtendDB's MetricsStore. LastMinute can return
# empty even mid-traffic depending on flush timing.
UPSTREAM = os.environ.get(
    "EXTENDDB_METRICS_UPSTREAM",
    "https://127.0.0.1:8000/metrics?window=Last5Minutes",
)
LISTEN = ("0.0.0.0", 9101)
TIMEOUT_SECS = 5

# camelCase -> snake_case for prometheus metric names.
def to_snake(name: str) -> str:
    out = []
    for i, ch in enumerate(name):
        if ch.isupper() and i > 0 and not name[i - 1].isupper():
            out.append("_")
        out.append(ch.lower())
    return "".join(out)


# Insecure context: the SUT cert is self-signed. We're scraping the loopback.
SSL_CTX = ssl.create_default_context()
SSL_CTX.check_hostname = False
SSL_CTX.verify_mode = ssl.CERT_NONE


def fetch_metrics() -> dict:
    req = urllib.request.Request(UPSTREAM, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, context=SSL_CTX, timeout=TIMEOUT_SECS) as resp:
        return json.loads(resp.read().decode("utf-8"))


def labels_for(dimensions: list) -> dict:
    """Flatten ExtendDB's tagged-enum dimension array into label kv."""
    out = {}
    for dim in dimensions or []:
        if not isinstance(dim, dict):
            continue
        for k, v in dim.items():
            # operation, table_name, index_name -- the only Dimension variants.
            if k == "Operation":
                out["operation"] = str(v)
            elif k == "TableName":
                out["table_name"] = str(v)
            elif k == "GlobalSecondaryIndexName":
                out["index_name"] = str(v)
    return out


def fmt_labels(labels: dict) -> str:
    if not labels:
        return ""
    parts = ",".join(f'{k}="{v}"' for k, v in sorted(labels.items()))
    return "{" + parts + "}"


def render(snapshot: dict) -> list:
    """One MetricSnapshot -> several Prometheus exposition lines."""
    metric = snapshot.get("metric")
    if not metric:
        return []
    base = "extenddb_" + to_snake(metric)
    labels = labels_for(snapshot.get("dimensions", []))
    lab = fmt_labels(labels)
    out = [
        f"# TYPE {base}_count counter",
        f"{base}_count{lab} {snapshot.get('count', 0)}",
        f"# TYPE {base}_sum_us counter",
        f"{base}_sum_us{lab} {snapshot.get('sum', 0.0)}",
        f"# TYPE {base}_min_us gauge",
        f"{base}_min_us{lab} {snapshot.get('min', 0.0)}",
        f"# TYPE {base}_max_us gauge",
        f"{base}_max_us{lab} {snapshot.get('max', 0.0)}",
    ]
    return out


def render_all(payload: dict) -> bytes:
    lines = [f"# HELP extenddb_metrics_shim ExtendDB JSON /metrics translated to Prometheus"]
    seen_help = set()
    for snap in payload.get("metrics", []):
        for line in render(snap):
            if line.startswith("# TYPE "):
                # Dedup TYPE declarations.
                family = line.split()[2]
                if family in seen_help:
                    continue
                seen_help.add(family)
            lines.append(line)
    # Liveness gauge.
    lines.append("# TYPE extenddb_metrics_shim_up gauge")
    lines.append("extenddb_metrics_shim_up 1")
    body = ("\n".join(lines) + "\n").encode("utf-8")
    return body


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/metrics":
            self.send_response(404)
            self.end_headers()
            return
        try:
            payload = fetch_metrics()
            body = render_all(payload)
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; version=0.0.4")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except Exception as e:
            msg = f"# extenddb_metrics_shim_up 0\n# error: {e}\n".encode("utf-8")
            self.send_response(503)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(msg)))
            self.end_headers()
            self.wfile.write(msg)

    def log_message(self, format, *args):
        # Quiet by default; rely on systemd journal for noteworthy events.
        return


def main():
    server = HTTPServer(LISTEN, Handler)
    sys.stderr.write(f"extenddb-metrics-shim listening on {LISTEN[0]}:{LISTEN[1]}\n")
    sys.stderr.flush()
    server.serve_forever()


if __name__ == "__main__":
    main()
