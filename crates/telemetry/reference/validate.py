#!/usr/bin/env python3
"""Offline validation against the pinned, unmodified Harbor Python models."""
import base64
import copy
import hashlib
import json
from pathlib import Path
import sys
sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / "harbor"))
from harbor.models.trajectories import Trajectory
from harbor.models.agent.context import AgentContext
from google.protobuf.json_format import ParseDict
from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import ExportTraceServiceRequest

def verify_sources():
    for directory in ["harbor", "genai"]:
        pin = json.loads((ROOT / directory / "PIN.json").read_text())
        for name, expected in pin["files_sha256"].items():
            actual = hashlib.sha256((ROOT / directory / name).read_bytes()).hexdigest()
            assert actual == expected, f"reference source changed: {directory}/{name}"
    actual = Trajectory.model_json_schema()
    assert actual == json.loads((ROOT / "atif-v1.8.schema.json").read_text()), "schema differs from pinned official models"

def validate_otlp(document):
    # OTLP JSON deliberately differs from ordinary protobuf JSON for IDs:
    # the OTLP wire spelling is hex, whereas protobuf's generic parser expects
    # base64 for bytes. Translate ONLY those documented special fields.
    converted = copy.deepcopy(document)
    for resource in converted["resourceSpans"]:
        for scope in resource["scopeSpans"]:
            pin = {a["key"]: a["value"].get("stringValue") for a in scope["scope"]["attributes"]}
            assert pin["excalibur.gen_ai.conventions.commit"] == "3fe84604a581b3d50dd5b4bd29383b462d2a5018"
            for span in scope["spans"]:
                for key, size in [("traceId", 16), ("spanId", 8), ("parentSpanId", 8)]:
                    if key in span:
                        value = bytes.fromhex(span[key]); assert len(value) == size and any(value)
                        span[key] = base64.b64encode(value).decode()
                assert int(span["endTimeUnixNano"]) >= int(span["startTimeUnixNano"])
                attrs = {a["key"]: a["value"] for a in span["attributes"]}
                op = attrs["gen_ai.operation.name"]["stringValue"]
                assert op in ["invoke_agent", "chat", "execute_tool"]
                if op == "chat":
                    assert span["kind"] == 3 and attrs["gen_ai.provider.name"]["stringValue"]
                if op == "execute_tool":
                    assert span["kind"] == 1 and attrs["gen_ai.tool.name"]["stringValue"]
                    assert attrs["gen_ai.conversation.id"]["stringValue"]
                if op == "invoke_agent":
                    assert span["kind"] == 1 and attrs["gen_ai.agent.name"]["stringValue"]
                if span.get("status", {}).get("code") == 2:
                    assert attrs["error.type"]["stringValue"]
    ParseDict(converted, ExportTraceServiceRequest(), ignore_unknown_fields=False)

def main():
    verify_sources()
    paths = [Path(p) for p in sys.argv[1:]] or [ROOT.parent / "fixtures"]
    counts = {"ATIF": 0, "AgentContext": 0, "OTLP": 0}
    files = set()
    for path in paths:
        files.update(path.rglob("*.json") if path.is_dir() else [path])
    for path in sorted(files):
        if not (path.name.endswith(("trajectory.json", "agent-context.json")) or path.name == "otel.json"):
            continue
        document = json.loads(path.read_text())
        if path.name.endswith("trajectory.json"):
            assert document["schema_version"] == "ATIF-v1.8"
            Trajectory.model_validate(document)
            counts["ATIF"] += 1
        elif path.name.endswith("agent-context.json"):
            AgentContext.model_validate(document)
            counts["AgentContext"] += 1
        elif path.name == "otel.json":
            validate_otlp(document)
            counts["OTLP"] += 1
    assert all(counts.values()), "ATIF, AgentContext and OTLP fixtures must all be present"
    print(json.dumps({"harbor_commit": "509b119db0d97b141dcd9c80f4f7ab861c47c2d6", "validated": counts}, sort_keys=True))
if __name__ == "__main__":
    main()
