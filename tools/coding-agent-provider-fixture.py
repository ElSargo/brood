#!/usr/bin/env python3
"""Local Responses fixture: the real Brood runner owns tools and checkpoints."""
import base64
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import subprocess
import sys
import tempfile

CREATE = "Create .brood/image-proof.txt with red green and .brood/agent-actor-proof.txt with actor-ui-worker-ok"
RESUME = "Verify durable agent state"
port_file, evidence_file = map(Path, sys.argv[1:])
evidence = {"requests": 0, "compactions": 0, "image_colors": [], "resumed": False, "cancellation_recovered": False}


def message(text):
    return {"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": text}]}


def tool(name, arguments, call_id):
    return {"type": "function_call", "name": name, "arguments": json.dumps(arguments), "call_id": call_id}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            evidence["requests"] += 1
            items = body["input"]
            users = [item for item in items if item.get("role") == "user"]
            if self.path.endswith("/compact"):
                evidence["compactions"] += 1
                self.reply({"output": users + [{"type": "compaction", "encrypted_content": "fixture-checkpoint"}]})
                return
            prompt = users[-1]["content"]
            if isinstance(prompt, list):
                prompt = next(item["text"] for item in prompt if item["type"] == "input_text")
            results = [item for item in items if item.get("type") == "function_call_output"]
            if prompt == CREATE and not results:
                images = [part for part in users[-1]["content"] if part["type"] == "input_image"]
                assert len(images) == 2
                colors = set()
                for image in images:
                    raw = base64.b64decode(image["image_url"].split(",", 1)[1])
                    assert raw.startswith(b"\x89PNG\r\n\x1a\n")
                    with tempfile.NamedTemporaryFile(suffix=".png") as file:
                        file.write(raw)
                        file.flush()
                        histogram = subprocess.check_output(["magick", file.name, "-format", "%c", "histogram:info:-"], text=True).upper()
                        if "#FF0000" in histogram:
                            colors.add("red")
                        if "#00FF00" in histogram:
                            colors.add("green")
                assert colors == {"red", "green"}
                evidence["image_colors"] = sorted(colors)
                patch = "*** Begin Patch\n*** Add File: .brood/image-proof.txt\n+red green\n*** Add File: .brood/agent-actor-proof.txt\n+actor-ui-worker-ok\n*** End Patch"
                code = "let broodProofValue = 42; const edit = await tools.apply_patch({patch:" + json.dumps(patch) + "}); if (edit.isError) throw new Error(JSON.stringify(edit)); text(edit);"
                output = [tool("js_repl", {"code": code}, "create-proof")]
            elif prompt == CREATE:
                assert not any("Tool failed" in json.dumps(item) for item in results)
                output = [message("Validated the red and green images and wrote the requested proof files through the persistent JavaScript REPL.")]
            elif prompt == RESUME and not any(item.get("call_id") == "resume-proof" for item in results):
                assert any(item.get("encrypted_content") == "fixture-checkpoint" for item in items)
                patch = "*** Begin Patch\n*** Add File: .brood/agent-resume-proof.txt\n+durable-repl-compaction-ok\n*** End Patch"
                code = "if (broodProofValue !== 42) throw new Error('REPL state lost'); const check = await tools.exec_command({command:'cat .brood/agent-actor-proof.txt'}); if (check.isError || !check.output.stdout.includes('actor-ui-worker-ok')) throw new Error('workspace state lost'); text(await tools.apply_patch({patch:" + json.dumps(patch) + "}));"
                output = [tool("js_repl", {"code": code}, "resume-proof")]
            elif prompt == RESUME:
                evidence["resumed"] = True
                output = [message("Verified durable conversation state, compaction, persistent REPL variables, and the real workspace files.")]
            elif prompt == "Verify cancellation":
                command = "printf ready > .brood/cancel-started; (trap '' TERM; sleep 3; printf leaked > .brood/cancel-leaked) & wait"
                output = [tool("exec_command", {"command": command, "yield_time_ms": 30000}, "cancel-command")]
            elif prompt == "Verify cancellation recovery" and not any(item.get("call_id") == "recover-command" for item in results):
                interrupted = next(item for item in results if item.get("call_id") == "cancel-command")
                assert "may have run" in json.dumps(interrupted)
                command = "test -f .brood/cancel-started && test ! -e .brood/cancel-leaked && printf cancellation-recovery-ok > .brood/cancel-recovered"
                output = [tool("exec_command", {"command": command}, "recover-command")]
            elif prompt == "Verify cancellation recovery":
                result = next(item for item in results if item.get("call_id") == "recover-command")
                assert "Tool failed" not in json.dumps(result)
                evidence["cancellation_recovered"] = True
                output = [message("Cancellation stopped the shell descendants. Recovered the saved conversation and verified the workspace without replaying the interrupted command.")]
            else:
                raise AssertionError(f"Unexpected prompt: {prompt!r}")
            self.reply({"status": "completed", "output": output, "usage": {"input_tokens": 100, "output_tokens": 20}})
        except Exception as error:
            self.reply({"error": {"message": str(error)}}, 400)

    def reply(self, body, status=200):
        evidence_file.write_text(json.dumps(evidence, indent=2) + "\n")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(body).encode())


server = HTTPServer(("127.0.0.1", 0), Handler)
port_file.write_text(str(server.server_port))
server.serve_forever()
