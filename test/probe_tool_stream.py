"""Probe configured OpenAI tool streams without printing credentials or arguments."""
import concurrent.futures
import json
import sys
import urllib.request
from pathlib import Path


def probe(provider):
    body = {
        "model": provider["models"][0]["real"], "stream": True, "max_tokens": 512,
        "messages": [{"role": "user", "content": "Call read_file with path README.md. Do not answer in text."}],
        "tools": [{"type": "function", "function": {
            "name": "read_file", "description": "Read a file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
        }}],
        "tool_choice": {"type": "function", "function": {"name": "read_file"}},
    }
    headers = {"Content-Type": "application/json", "Authorization": "Bearer " + provider["api_key"],
               "User-Agent": "ClaudeRelay/diagnostic", "x-opencode-session": "relay-stream-diagnostic"}
    result = {"provider": provider["id"], "chunks": []}
    arguments = {}
    try:
        request = urllib.request.Request(provider["base_url"].rstrip("/") + "/chat/completions",
                                         data=json.dumps(body).encode(), headers=headers)
        with urllib.request.urlopen(request, timeout=40) as response:
            for line in response:
                if not line.startswith(b"data:"):
                    continue
                raw = line[5:].strip()
                if raw == b"[DONE]":
                    result["done"] = True
                    break
                value = json.loads(raw)
                for choice in value.get("choices", []):
                    delta = choice.get("delta") or {}
                    chunk = {"keys": list(delta), "empty_content": delta.get("content") == "",
                             "finish": choice.get("finish_reason"), "tools": []}
                    for tool in delta.get("tool_calls", []):
                        function = tool.get("function") or {}
                        args = function.get("arguments") or ""
                        index = tool.get("index", 0)
                        arguments[index] = arguments.get(index, "") + args
                        chunk["tools"].append({"index": index, "id_present": bool(tool.get("id")),
                                               "name": function.get("name"), "args_len": len(args)})
                    result["chunks"].append(chunk)
        result["argument_keys"] = {str(i): list(json.loads(a)) for i, a in arguments.items()}
    except Exception as error:
        result["error_type"] = type(error).__name__
        result["http"] = getattr(error, "code", None)
    return result


if __name__ == "__main__":
    config = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8-sig"))
    providers = [p for p in config["providers"] if p.get("protocol") == "openai"]
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        for result in executor.map(probe, providers):
            print(json.dumps(result, ensure_ascii=False))
