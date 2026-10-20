#!/usr/bin/env python3
"""Mock Anthropic 兼容上游：验证 ClaudeRelay 的模型改写 / Key 替换 / SSE 透传 / model 回写。

用法: python mock_upstream.py <port>  <expected_key>  <real_model>
- 校验认证头（Authorization: Bearer <expected_key>）
- 读 body.model，断言等于 <real_model>（验证中继已改写）
- 回一个完整 SSE 序列，message_start 里 model=<real_model>
  （配合 relay 的 rewrite_model，应被改回别名）
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def main():
    port = int(sys.argv[1])
    expected_key = sys.argv[2]
    real_model = sys.argv[3]

    class H(BaseHTTPRequestHandler):
        def _send_json(self, obj):
            b = json.dumps(obj).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)

        def log_message(self, *a):
            pass

        def do_POST(self):
            ln = int(self.headers.get("content-length", 0))
            raw = self.rfile.read(ln)
            # 断言认证头
            auth = self.headers.get("Authorization", "")
            assert auth == f"Bearer {expected_key}", f"AUTH MISMATCH: {auth!r}"
            body = json.loads(raw)
            got_model = body.get("model")
            assert got_model == real_model, f"MODEL MISMATCH: {got_model!r} != {real_model!r}"
            print(f"[mock] got model={got_model!r} max_tokens={body.get('max_tokens')}", flush=True)

            if self.path.endswith("/count_tokens"):
                self._send_json({"input_tokens": 42})
                return

            # /v1/messages：SSE 流式回复，模拟工具调用 + 文本 + 结束
            lines = [
                ("event: message_start",
                 {"type": "message_start", "message": {"id": "msg_1", "type": "message",
                  "role": "assistant", "model": real_model,
                  "content": [], "stop_reason": None, "stop_sequence": None,
                  "usage": {"input_tokens": 5, "output_tokens": 1}}}),
                ("event: content_block_start",
                 {"type": "content_block_start", "index": 0,
                  "content_block": {"type": "text", "text": ""}}),
                ("event: content_block_delta",
                 {"type": "content_block_delta", "index": 0,
                  "delta": {"type": "text_delta", "text": "你好，这是透传的流式回复"}}),
                ("event: content_block_stop",
                 {"type": "content_block_stop", "index": 0}),
                ("event: message_delta",
                 {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                  "usage": {"output_tokens": 12}}),
                ("event: message_stop", {"type": "message_stop"}),
            ]
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.send_header("connection", "keep-alive")
            self.end_headers()
            for event, data in lines:
                self.wfile.write(f"{event}\ndata: {json.dumps(data, ensure_ascii=False)}\n\n".encode())

        do_GET = do_POST

    print(f"mock upstream on :{port}, expect Bearer {expected_key}, model={real_model}")
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()


if __name__ == "__main__":
    main()
