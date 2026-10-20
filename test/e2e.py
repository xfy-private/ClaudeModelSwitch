#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""ClaudeRelay 端到端验收脚本（Phase 4）。

本地起一个 mock Anthropic 上游 + 启动 relay（mock 配置），一次走通
「配置 → 启动 → 切换 → 对话 → 工具」全链路，全部断言通过输出 PASS。

验证项：
  1. relay 启动，/healthz 正常
  2. GET /v1/models 聚合别名（含前缀 mock-mock-chat）
  3. POST /v1/messages（流式对话 + 工具）：
       - model 改写 mock-mock-chat -> mock-chat（mock 侧断言）
       - 认证头替换为 Bearer MOCK_SECRET_KEY_123，剥离 CC 的 x-api-key（mock 侧断言）
       - anthropic-version 白名单透传（mock 侧断言）
       - max_tokens 原样透传（不再 clamp，32000 保持 32000）（mock 侧断言）
       - tools 数组原样透传（mock 侧断言）
       - SSE 事件序列完整（message_start..message_stop），中文无损
       - message_start 里 model 回写为别名 mock-mock-chat（客户端断言）
  4. count_tokens 透传 -> {"input_tokens": 42}
  5. 断线重连：客户端中途断开 SSE，relay 不崩，下一请求正常
  6. 未知模型 -> 400 + 别名列表提示

用法: python test/e2e.py [relay_exe]
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import urlopen, Request

ROOT = os.path.dirname(os.path.abspath(__file__))
PROJ = os.path.dirname(ROOT)
RELAY = sys.argv[1] if len(sys.argv) > 1 else os.path.join(PROJ, "target", "debug", "claude-relay.exe")
CFG = os.path.join(ROOT, "config.mock.json")
PORT = 8788
MOCK_PORT = 9099
KEY = "MOCK_SECRET_KEY_123"
REAL_MODEL = "mock-chat"
ALIAS = "mock-mock-chat"
LOG = os.path.join(ROOT, "relay_e2e.log")

PASS = 0


def ok(name):
    global PASS
    PASS += 1
    print(f"  PASS  {name}")


def fail(name, detail):
    print(f"  FAIL  {name}: {detail}")
    sys.exit(1)


def check(name, cond, detail=""):
    if cond:
        ok(name)
    else:
        fail(name, detail)


# ---------------- mock 上游 ----------------
received = []  # (path, headers, body_dict) —— POST
received_get = []  # (path, headers) —— GET（Phase 28 coding-plan）


class Mock(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *a):
        pass

    def _sse(self):
        lines = [
            ("message_start", {"type": "message_start", "message": {"id": "msg_1", "type": "message",
                               "role": "assistant", "model": REAL_MODEL, "content": [],
                               "stop_reason": None, "stop_sequence": None,
                               "usage": {"input_tokens": 5, "output_tokens": 1}}}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                                     "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                     "delta": {"type": "text_delta", "text": "你好，这是透传的流式回复"}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("content_block_start", {"type": "content_block_start", "index": 1,
                                     "content_block": {"type": "tool_use", "id": "toolu_01",
                                                      "name": "get_weather", "input": {}}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 1,
                                     "delta": {"type": "input_json_delta", "partial_json": '{"city":"北京"}'}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 1}),
            ("message_delta", {"type": "message_delta",
                               "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                               "usage": {"output_tokens": 22}}),
            ("message_stop", {"type": "message_stop"}),
        ]
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        try:
            for event, data in lines:
                self.wfile.write(f"event: {event}\ndata: {json.dumps(data, ensure_ascii=False)}\n\n".encode("utf-8"))
                self.wfile.flush()
                time.sleep(0.05)  # 慢速流式，给断线测试留出"中途断开"窗口
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, OSError):
            pass  # 客户端中途断开（断线测试场景）

    def do_GET(self):
        # Phase 28：模拟火山方舟 Coding Plan 用量接口（返回用户提供的真实样例）
        received_get.append((self.path, dict(self.headers)))
        if self.path.endswith("/GetCodingPlanUsage"):
            b = json.dumps({
                "ResponseMetadata": {"Action": "GetCodingPlanUsage"},
                "Result": {
                    "Status": "Running",
                    "UpdateTimestamp": 1785826887,
                    "QuotaUsage": [
                        {"Level": "session", "Percent": 3.487035, "ResetTimestamp": 1785841012},
                        {"Level": "weekly", "Percent": 3.5022089999999997, "ResetTimestamp": 1786291200},
                        {"Level": "monthly", "Percent": 1.7511044999999998, "ResetTimestamp": 1788451199},
                    ],
                },
            }).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            # Phase 53/53d：真实上游带 Set-Cookie 续期——只回填 sessionid（同名更新）+ theme（新 key），
            # 存档里的 region 应被保留（按 key 合并，而非整体替换）
            self.send_header("set-cookie", "sessionid=e2e-renewed-mock; Path=/; HttpOnly")
            self.send_header("set-cookie", "theme=dark; Path=/")
            self.send_header("content-length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
        elif self.path.split("?")[0].endswith("/coding-plan/list"):
            # Phase 30：模拟讯飞星辰 Coding Plan 列表（两行：一行 apiKey 匹配 xfmock，一行不匹配）
            b = json.dumps({
                "code": 0,
                "data": {
                    "page": 1,
                    "rows": [
                        {"appId": "mcc165a0", "codingPlanAppCredentialDTO": {"apiKey": "XF_E2E_KEY"},
                         "codingPlanUsageDTO": {"appId": "mcc165a0", "channel": "astron-code-latest",
                                                "dailyLimit": None, "dailyUsage": None,
                                                "packageLeft": 13346.6, "packageLimit": 18000,
                                                "packageUsage": 4653.4, "rp5hLimit": 1200,
                                                "rp5hUsage": 22, "rpwLimit": 9000, "rpwUsage": 1940.4},
                         "createBy": 1517199, "createTime": "2026-07-14 10:00:14",
                         "expiresAt": "2026-08-14 10:00:36", "name": "专业版-包月"},
                        {"appId": "other", "codingPlanAppCredentialDTO": {"apiKey": "OTHER_KEY"},
                         "codingPlanUsageDTO": {"packageLeft": 0, "packageLimit": 0, "packageUsage": 0},
                         "name": "其他套餐"},
                    ],
                    "size": 6,
                    "total": 2,
                },
                "failed": False, "message": "OK", "messageEn": "OK", "succeed": True,
            }, ensure_ascii=False).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("set-cookie", "sessionid=e2e-renewed-xf; Path=/; HttpOnly")
            self.send_header("content-length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
        elif "/workspace/" in self.path and self.path.endswith("/go"):
            # Phase 48：模拟 OpenCode /go 页（SolidStart HTML，用量内嵌在 lite.subscription.get 脚本）
            # Phase 52：另含 `monthlyUsage: null` 占位（其它对象里）——relay 须跳过它命中真实用量对象
            b = """<html><body><script>
              $R[24] = { balance: 0, monthlyUsage: null, subscription: null };
              $R[28]($R[18], $R[34] = {
                  mine: !0, useBalance: !1,
                  region: $R[35] = ["us", "eu", "sg", "cn"],
                  rollingUsage: $R[36] = { status: "ok", resetInSec: 12114, usagePercent: 2 },
                  weeklyUsage: $R[37] = { status: "ok", resetInSec: 421682, usagePercent: 15 },
                  monthlyUsage: $R[38] = { status: "ok", resetInSec: 2587650, usagePercent: 7 }
              });
            </script></body></html>""".encode()
            self.send_response(200)
            self.send_header("content-type", "text/html")
            self.send_header("set-cookie", "sessionid=e2e-renewed-oc; Path=/; HttpOnly")
            self.send_header("content-length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
        else:
            self.send_response(404)
            self.send_header("content-length", "0")
            self.end_headers()

    def _openai_sse(self):
        # Phase 36：OpenAI 流式 chunk（文本 + finish + usage + [DONE]）→ relay 翻译成 Anthropic SSE
        chunks = [
            {"id": "chatcmpl-1", "object": "chat.completion.chunk",
             "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}]},
            {"id": "chatcmpl-1", "object": "chat.completion.chunk",
             "choices": [{"index": 0, "delta": {"content": "你好"}, "finish_reason": None}]},
            {"id": "chatcmpl-1", "object": "chat.completion.chunk",
             "choices": [{"index": 0, "delta": {"content": "，OpenAI"}, "finish_reason": "stop"}]},
            {"id": "chatcmpl-1", "object": "chat.completion.chunk",
             "choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 6}},
        ]
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        try:
            for c in chunks:
                self.wfile.write(b"data: " + json.dumps(c, ensure_ascii=False).encode() + b"\n\n")
                self.wfile.flush()
                time.sleep(0.03)
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def do_POST(self):
        ln = int(self.headers.get("Content-Length", 0) or 0)
        body = json.loads(self.rfile.read(ln))
        received.append((self.path, dict(self.headers), body))
        if self.path.endswith("/count_tokens"):
            b = json.dumps({"input_tokens": 42}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
        elif self.path.split("?")[0].endswith(":streamGenerateContent"):
            # Phase 56：Gemini 流式（alt=sse，data: {json}，无 [DONE]）
            chunks = [
                {"candidates": [{"content": {"role": "model", "parts": [{"text": "你好"}]},
                                 "finishReason": "STOP"}],
                 "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 1}},
                {"candidates": [{"content": {"role": "model",
                                             "parts": [{"functionCall": {"name": "get_weather",
                                                                         "args": {"city": "北京"}}}]}}]},
                {"candidates": [{"finishReason": "STOP"}],
                 "usageMetadata": {"promptTokenCount": 9, "candidatesTokenCount": 6,
                                   "totalTokenCount": 15}},
            ]
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.end_headers()
            try:
                for c in chunks:
                    self.wfile.write(b"data: " + json.dumps(c, ensure_ascii=False).encode() + b"\n\n")
                    self.wfile.flush()
                    time.sleep(0.03)
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass
        elif self.path.split("?")[0].endswith(":generateContent"):
            # Phase 56：Gemini 非流式 JSON 响应
            b = json.dumps({
                "responseId": "resp_e2e",
                "candidates": [{"content": {"role": "model", "parts": [{"text": "结果"}]},
                                "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3,
                                  "totalTokenCount": 10},
            }, ensure_ascii=False).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
        elif "/trunc/chat/completions" in self.path:
            # Phase 59：上游中途断流 mock——声明超大 Content-Length，写 2 个合法
            # OpenAI chunk 后返回（HTTP/1.0 连接关闭）→ reqwest 在流中途产出 Err
            # （声明的 body 长度未满足）。期望 relay 发规范 error 事件 + 干净收尾。
            chunks = [
                {"id": "chatcmpl-t", "object": "chat.completion.chunk",
                 "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""},
                              "finish_reason": None}]},
                {"id": "chatcmpl-t", "object": "chat.completion.chunk",
                 "choices": [{"index": 0, "delta": {"content": "你好"}, "finish_reason": None}]},
            ]
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.send_header("content-length", "999999")
            self.end_headers()
            try:
                for c in chunks:
                    self.wfile.write(b"data: " + json.dumps(c, ensure_ascii=False).encode() + b"\n\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass
            # 故意不写完声明的 body → 连接关闭 → reqwest body 截断 Err
        elif "/eof/chat/completions" in self.path:
            # Phase 61：上游「干净断流」mock——不声明 Content-Length（HTTP/1.0
            # close-delimited），写 2 个合法 OpenAI chunk 后正常关闭连接 → reqwest
            # 以 EOF 结束流（Ok(None)，非 Err）。期望 relay 识别「EOF 但未收到
            # finish_reason/[DONE]」→ 发规范 error 事件而非伪装完整收尾。
            chunks = [
                {"id": "chatcmpl-e", "object": "chat.completion.chunk",
                 "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""},
                              "finish_reason": None}]},
                {"id": "chatcmpl-e", "object": "chat.completion.chunk",
                 "choices": [{"index": 0, "delta": {"content": "你好"}, "finish_reason": None}]},
            ]
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.end_headers()
            try:
                for c in chunks:
                    self.wfile.write(b"data: " + json.dumps(c, ensure_ascii=False).encode() + b"\n\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass
            # 不写 finish_reason、不写 [DONE]——干净 EOF 截断（FIN）
        elif self.path.endswith("/chat/completions"):
            self._openai_sse()
        else:
            self._sse()


srv = ThreadingHTTPServer(("127.0.0.1", MOCK_PORT), Mock)
threading.Thread(target=srv.serve_forever, daemon=True).start()
print(f"[e2e] mock upstream on :{MOCK_PORT}")


# ---------------- 启动 relay ----------------
if os.path.exists(LOG):
    os.remove(LOG)
# Phase 27：清掉上次运行的用量持久化文件，保证重启保留断言从干净状态开始。
# 文件名 = 配置文件名去掉最后扩展名 + "-usage.json"（config.mock.json → config.mock-usage.json）
USAGE_FILE = os.path.join(ROOT, "config.mock-usage.json")
if os.path.exists(USAGE_FILE):
    os.remove(USAGE_FILE)
logf = open(LOG, "ab")
flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
proc = subprocess.Popen([RELAY, "--config", CFG, "--debug"],
                        stdout=logf, stderr=logf, creationflags=flags)


def http_req(path, data=None, method="GET", extra=None, timeout=30):
    body = None if data is None else json.dumps(data).encode("utf-8")
    req = Request(f"http://127.0.0.1:{PORT}{path}", data=body, method=method)
    req.add_header("content-type", "application/json")
    req.add_header("anthropic-version", "2023-06-01")
    if extra:
        for k, v in extra.items():
            req.add_header(k, v)
    with urlopen(req, timeout=timeout) as r:
        return r.status, r.headers, r.read()


try:
    # 1. 就绪
    for _ in range(150):
        if proc.poll() is not None:
            fail("relay 启动", f"进程提前退出 rc={proc.returncode}，日志见 {LOG}")
        try:
            if urlopen(f"http://127.0.0.1:{PORT}/healthz", timeout=1).read() == b"ok\n":
                break
        except Exception:
            time.sleep(0.2)
    else:
        fail("relay 就绪", "healthz 30s 内未就绪")
    ok("relay 启动 + healthz")

    # 2. /v1/models 聚合别名
    st, _, body = http_req("/v1/models")
    ids = [m["id"] for m in json.loads(body)["data"]]
    check("GET /v1/models 聚合别名", st == 200 and ALIAS in ids, f"ids={ids}")

    # 3. 主对话（流式 + 工具透传）——请求体携带 CC 风格头 + tools
    sent_tools = [
        {"name": "get_weather", "description": "查询天气",
         "input_schema": {"type": "object",
                          "properties": {"city": {"type": "string"}},
                          "required": ["city"]}}
    ]
    req_body = {
        "model": ALIAS,
        "max_tokens": 32000,
        "stream": True,
        "messages": [{"role": "user", "content": "北京的天气如何？"}],
        "tools": sent_tools,
    }
    st, _, resp = http_req("/v1/messages?beta=true", data=req_body, method="POST",
                           extra={"x-api-key": "CC-SENDS-THIS"})
    check("对话 200", st == 200, f"status={st}")
    text = resp.decode("utf-8")

    # mock 侧：model 改写 / key 替换 / x-api-key 剥离 / anthropic-version / clamp / tools
    path, hdrs, bd = received[-1]
    H = {k.lower(): v for k, v in hdrs.items()}  # 头名大小写不敏感
    check("mock 收到 /v1/messages", path.endswith("/v1/messages"), path)
    check("model 已改写", bd.get("model") == REAL_MODEL, f"model={bd.get('model')!r}")
    check("认证头替换为 Bearer Key", H.get("authorization") == f"Bearer {KEY}",
          f"Authorization={H.get('authorization')!r}")
    check("CC 的 x-api-key 已剥离", "x-api-key" not in H, f"headers={list(H)}")
    check("anthropic-version 透传", H.get("anthropic-version") == "2023-06-01",
          f"anthropic-version={H.get('anthropic-version')!r}")
    check("max_tokens 原样透传 32000", bd.get("max_tokens") == 32000,
          f"max_tokens={bd.get('max_tokens')!r}")
    check("tools 原样透传", bd.get("tools") == sent_tools, f"tools={bd.get('tools')!r}")

    # 客户端侧：SSE 事件序列 / 中文无损 / model 回写别名
    for ev in ["message_start", "content_block_start", "content_block_delta",
               "content_block_stop", "message_delta", "message_stop"]:
        check(f"SSE 含 {ev}", f"event: {ev}" in text, "")
    check("中文无损", "你好，这是透传的流式回复" in text and "北京" in text, "")
    for line in text.splitlines():
        if line.startswith("data:") and '"message_start"' in line:
            start = json.loads(line[5:])
            check("message_start model 回写别名",
                  start["message"]["model"] == ALIAS,
                  f"model={start['message']['model']!r}")
    check("tool_use 块透传", "get_weather" in text and "tool_use" in text, "")

    # 4. count_tokens 透传
    st, _, body = http_req("/v1/messages/count_tokens", data={
        "model": ALIAS, "messages": [{"role": "user", "content": "hi"}]}, method="POST")
    ct = json.loads(body)
    check("count_tokens 透传 42", st == 200 and ct.get("input_tokens") == 42, f"{body!r}")

    # 5. 断线重连：中途断开 SSE
    sock = socket.create_connection(("127.0.0.1", PORT), timeout=5)
    rb = json.dumps(req_body).encode()
    req = (f"POST /v1/messages?beta=true HTTP/1.1\r\n"
           f"Host: 127.0.0.1:{PORT}\r\n"
           f"Content-Type: application/json\r\n"
           f"Content-Length: {len(rb)}\r\n"
           f"Connection: close\r\n"
           "\r\n").encode() + rb
    sock.sendall(req)
    time.sleep(0.2)
    got = sock.recv(4096)
    sock.close()  # 读一部分即断开
    time.sleep(0.5)
    check("断线后 relay 仍存活", proc.poll() is None, "relay 已退出")
    st, _, _ = http_req("/healthz")
    check("断线后 healthz 正常", st == 200, f"status={st}")
    st, _, resp = http_req("/v1/messages", data=req_body, method="POST")
    check("断线后新请求 200", st == 200 and b"message_stop" in resp, f"status={st}")

    # 6. 未知模型 -> 400 + 别名提示（urllib 对非 2xx 抛 HTTPError，需捕获读 body）
    import urllib.error

    try:
        http_req("/v1/messages", data={"model": "nope-x",
                                       "messages": [{"role": "user", "content": "hi"}]}, method="POST")
        fail("未知模型 400", "预期 400 却返回 2xx")
    except urllib.error.HTTPError as e:
        check("未知模型 400", e.code == 400, f"code={e.code}")
        body = e.read()
        check("400 提示含别名", ALIAS.encode() in body, body[:200])

    # 7. Phase 22：上下文后缀剥离 + 哨兵路由 claude-relay-model → 当前使用模型
    #   mock 配置无 default_model → 回退第一个服务商第一个模型别名 mock-mock-chat
    sentinel_resp = ""
    for mname, expect_model in [
        ("claude-relay-model[1m]", "mock-chat"),  # 哨兵 + 上下文后缀
        ("claude-relay-model", "mock-chat"),      # 裸哨兵
        ("mock-mock-chat[1m]", "mock-chat"),      # 具体模型带后缀 → 剥后缀路由
    ]:
        st, _, resp = http_req("/v1/messages", data={
            "model": mname, "messages": [{"role": "user", "content": "hi"}]}, method="POST")
        check(f"model {mname} 200", st == 200, f"status={st}")
        _, _, bd = received[-1]
        check(f"{mname} → 上游 {expect_model}", bd.get("model") == expect_model,
              f"model={bd.get('model')!r}")
        if mname == "claude-relay-model[1m]":
            sentinel_resp = resp.decode("utf-8")
    check("哨兵 SSE 回写当前模型 mock-mock-chat[200k]",
          "mock-mock-chat[200k]" in sentinel_resp, sentinel_resp[:200])

    # 8. Phase 26：用量统计（/api/status.usage）+ 余额（/api/balance）
    st, _, body = http_req("/api/status")
    usage = [u for u in json.loads(body).get("usage", []) if u.get("provider") == "mock"]
    check("api/status 含 mock 用量",
          st == 200 and usage and usage[0].get("requests", 0) >= 1,
          f"status={st} usage={body[:160]}")
    st, _, body = http_req("/api/balance")
    bl = json.loads(body)
    mock_bl = [b for b in bl.get("balances", []) if b.get("provider") == "mock"]
    check("api/balance 只返回有接口服务商（mock 无接口被隐藏）",
          st == 200 and not mock_bl, f"status={st} body={body[:160]}")

    # 8b. Phase 54：/api/usage 时间桶（每小时/每日）
    st, _, body = http_req("/api/usage")
    uu = json.loads(body)
    mock_usage = [u for u in uu.get("usage", []) if u.get("provider") == "mock"]
    m0 = mock_usage[0].get("models", [{}])[0] if mock_usage else {}
    hours = m0.get("time", {}).get("hours", []) if m0 else []
    days = m0.get("time", {}).get("days", []) if m0 else []
    check("api/usage 含 mock 小时桶",
          st == 200 and mock_usage and hours and hours[0].get("requests", 0) >= 1,
          f"status={st} usage={body[:220]}")
    check("api/usage 含 mock 天桶",
          days and days[0].get("requests", 0) >= 1, f"status={st} usage={body[:220]}")

    # 9. Phase 27：用量持久化——落盘 + 重启不清零
    st, _, body = http_req("/api/status")
    usage_now = [u for u in json.loads(body).get("usage", []) if u.get("provider") == "mock"]
    req_before = usage_now[0].get("requests", 0) if usage_now else 0
    deadline = time.time() + 8  # 周期落盘任务 5s 间隔，等它把最新统计写进 config-usage.json
    file_ok = False
    while time.time() < deadline:
        if os.path.exists(USAGE_FILE):
            try:
                with open(USAGE_FILE, encoding="utf-8") as f:
                    uf = json.load(f)
                if uf.get("providers", {}).get("mock", {}).get("requests", 0) >= req_before:
                    file_ok = True
                    break
            except Exception:
                pass
        time.sleep(0.5)
    check("用量文件落盘 config.mock-usage.json", file_ok, f"req_before={req_before}")

    # 重启 relay 子进程（模拟服务重启）→ usage 保留不清零
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    proc = subprocess.Popen([RELAY, "--config", CFG, "--debug"],
                            stdout=logf, stderr=logf, creationflags=flags)
    for _ in range(150):
        if proc.poll() is not None:
            fail("relay 重启", "进程提前退出")
        try:
            if urlopen(f"http://127.0.0.1:{PORT}/healthz", timeout=1).read() == b"ok\n":
                break
        except Exception:
            time.sleep(0.2)
    else:
        fail("relay 重启就绪", "healthz 30s 内未就绪")
    st, _, body = http_req("/api/status")
    usage_after = [u for u in json.loads(body).get("usage", []) if u.get("provider") == "mock"]
    req_after = usage_after[0].get("requests", 0) if usage_after else 0
    check("重启后用量不清零", req_after >= req_before, f"before={req_before} after={req_after}")

    # 10. Phase 28/30：Coding Plan 用量——火山方舟（ark）+ 讯飞星辰（xf）
    st, _, body = http_req("/api/coding-plan")
    cps = json.loads(body).get("coding_plans", [])
    mock_cp = [c for c in cps if c.get("provider") == "mock"]
    check("api/coding-plan 200 + mock 条目", st == 200 and mock_cp, f"status={st} body={body[:160]}")
    ok_cp = mock_cp and mock_cp[0].get("ok") is True and len(mock_cp[0].get("levels", [])) == 3
    check("coding-plan 解析出 3 档用量", ok_cp, f"{body[:200]}")
    labels = [lv.get("label") for lv in mock_cp[0].get("levels", [])] if mock_cp else []
    check("coding-plan 档位标签 5小时/一周/一月",
          labels == ["5小时", "一周", "一月"], f"labels={labels}")
    # 火山方舟 GET 必须带存档 Cookie（received_get 里含 xf 请求，按路径挑 ark 那条）
    ark_get = [g for g in received_get if g[0].endswith("/GetCodingPlanUsage")]
    if ark_get:
        gpath, ghdrs = ark_get[-1]
        gcookie = {k.lower(): v for k, v in ghdrs.items()}.get("cookie", "")
        check("coding-plan GET 带 Cookie", gcookie == "sessionid=e2e-cookie; region=cn",
              f"path={gpath} cookie={gcookie!r}")
    else:
        fail("coding-plan 请求到 mock", "未收到 GET /GetCodingPlanUsage")
    # Phase 30：讯飞星辰——按 rows 的 codingPlanAppCredentialDTO.apiKey 匹配 xfmock.api_key
    xfmock_cp = [c for c in cps if c.get("provider") == "xfmock"]
    check("api/coding-plan 含讯飞条目", xfmock_cp and xfmock_cp[0].get("ok") is True,
          f"body={body[:200]}")
    xf_plans = xfmock_cp[0].get("plans", []) if xfmock_cp else []
    check("讯飞按 apiKey 过滤只留匹配套餐", len(xf_plans) == 1, f"plans={xf_plans}")
    if xf_plans:
        p0 = xf_plans[0]
        check("讯飞套餐名", p0.get("plan") == "专业版-包月", f"plan={p0.get('plan')!r}")
        check("讯飞包量百分比 25.9", abs(p0.get("percent", 0) - 25.9) < 0.01,
              f"percent={p0.get('percent')}")
        check("讯飞剩余/配额字段", p0.get("package_left") == 13346.6
              and p0.get("rp5h_usage") == 22 and p0.get("rpw_limit") == 9000, f"{p0}")
    xf_get = [g for g in received_get if "/coding-plan/list" in g[0]]
    if xf_get:
        gpath, ghdrs = xf_get[-1]
        gd = {k.lower(): v for k, v in ghdrs.items()}
        check("讯飞 GET 带 Cookie + Referer",
              gd.get("cookie", "") == "sessionid=xf-e2e-cookie"
              and gd.get("referer", "") == "https://maas.xfyun.cn/",
              f"path={gpath} cookie={gd.get('cookie')!r} referer={gd.get('referer')!r}")
    else:
        fail("讯飞 coding-plan 请求到 mock", "未收到 GET /coding-plan/list")
    # Phase 48：OpenCode——/go 页 HTML 提取 滚动/每周/每月 用量（按 /workspace/ 路径分派）
    ocmock_cp = [c for c in cps if c.get("provider") == "ocmock"]
    check("api/coding-plan 含 OpenCode 条目", ocmock_cp and ocmock_cp[0].get("ok") is True,
          f"body={body[:200]}")
    oc_levels = ocmock_cp[0].get("levels", []) if ocmock_cp else []
    check("OpenCode 解析出 3 档用量", len(oc_levels) == 3, f"levels={oc_levels}")
    oc_labels = [lv.get("label") for lv in oc_levels]
    check("OpenCode 档位标签 5小时/一周/一月",
          oc_labels == ["5小时", "一周", "一月"], f"labels={oc_labels}")
    oc_pct = [lv.get("percent") for lv in oc_levels]
    check("OpenCode 百分比 2/15/7", oc_pct == [2, 15, 7], f"percent={oc_pct}")
    oc_get = [g for g in received_get if "/workspace/" in g[0]]
    if oc_get:
        gpath, ghdrs = oc_get[-1]
        gd = {k.lower(): v for k, v in ghdrs.items()}
        check("OpenCode GET 带 Cookie + Referer",
              gd.get("cookie", "") == "sessionid=oc-e2e-cookie"
              and gd.get("referer", "") == "https://opencode.ai/",
              f"path={gpath} cookie={gd.get('cookie')!r} referer={gd.get('referer')!r}")
    else:
        fail("OpenCode coding-plan 请求到 mock", "未收到 GET /workspace/.../go")
    # Phase 53/53d：响应 Set-Cookie → 存档 Cookie 自动续期——**按 key 合并**：
    # sessionid 更新、theme 追加、region 保留（整体替换会丢 region）。磁盘也更新。
    http_req("/api/coding-plan")
    ark_get2 = [g for g in received_get if g[0].endswith("/GetCodingPlanUsage")]
    gcookie2 = {k.lower(): v for k, v in ark_get2[-1][1].items()}.get("cookie", "")
    check("响应 Set-Cookie 按 key 合并续期存档 Cookie",
          gcookie2 == "sessionid=e2e-renewed-mock; region=cn; theme=dark",
          f"cookie={gcookie2!r}")
    disk_cfg = json.load(open(CFG, encoding="utf-8"))
    mock_disk = [p for p in disk_cfg["providers"] if p["id"] == "mock"]
    check("续期 Cookie 已写盘并热重载",
          mock_disk and mock_disk[0].get("console_cookie") == "sessionid=e2e-renewed-mock; region=cn; theme=dark",
          f"disk={mock_disk[0].get('console_cookie') if mock_disk else None!r}")
    # 还原 mock 配置的存档 Cookie——保证 e2e 可重复运行（下次启动 relay 读回原始值）。
    # 只改三个 mock 服务商；其余（如 oa）原本无 console_cookie 键 → 删除键，勿写成 null（relay 解析 String 会失败）。
    RESTORE_COOKIE = {"mock": "sessionid=e2e-cookie; region=cn", "xfmock": "sessionid=xf-e2e-cookie",
                      "ocmock": "sessionid=oc-e2e-cookie"}
    for p in disk_cfg["providers"]:
        if p["id"] in RESTORE_COOKIE:
            p["console_cookie"] = RESTORE_COOKIE[p["id"]]
        else:
            p.pop("console_cookie", None)
    with open(CFG, "w", encoding="utf-8") as f:
        json.dump(disk_cfg, f, ensure_ascii=False, indent=2)
    # import-cookie：自动读浏览器（环境相关——本机无 volcengine 浏览器 Cookie 时 400，有则 200；都合法）
    try:
        st, _, body = http_req("/api/import-cookie", data={"provider": "mock"}, method="POST")
        imp = json.loads(body)
        check("import-cookie 响应合法", st in (200, 400) and "ok" in imp,
              f"status={st} body={body[:160]}")
    except urllib.error.HTTPError as e:
        imp = json.loads(e.read())
        check("import-cookie 响应合法", e.code in (200, 400) and "ok" in imp,
              f"status={e.code} body={imp}")

    # 11. Phase 36：OpenAI 协议翻译（oa 服务商 protocol=openai → /chat/completions）
    oa_body = {
        "model": "oa-gpt-4o-mini",
        "system": "你是助手",
        "max_tokens": 32000,
        "stream": True,
        "messages": [{"role": "user", "content": "你好"}],
    }
    st, _, resp = http_req("/v1/messages", data=oa_body, method="POST")
    oa_text = resp.decode("utf-8")
    check("openai 翻译 200 + message_stop", st == 200 and "event: message_stop" in oa_text,
          f"status={st}")
    path, hdrs, bd = received[-1]
    H = {k.lower(): v for k, v in hdrs.items()}
    check("openai mock 收到 /chat/completions", path.endswith("/chat/completions"), path)
    check("openai 请求已翻译 model", bd.get("model") == "gpt-4o-mini",
          f"model={bd.get('model')!r}")
    msgs = bd.get("messages", [])
    check("openai system 转 system 消息",
          msgs and msgs[0].get("role") == "system" and msgs[0].get("content") == "你是助手",
          f"msgs={msgs[:1]}")
    check("openai 认证 Bearer OA_KEY_123", H.get("authorization") == "Bearer OA_KEY_123",
          f"auth={H.get('authorization')!r}")
    check("openai 剥离 anthropic-version", "anthropic-version" not in H, f"headers={list(H)}")
    check("anthropic 事件 message_start", "event: message_start" in oa_text, "")
    check("anthropic text_delta 含你好", "你好" in oa_text, "")
    check("anthropic 收尾 end_turn", '"stop_reason":"end_turn"' in oa_text, "")
    # 用量提取：OpenAI usage chunk（prompt/completion tokens）→ 共享统计
    st, _, body = http_req("/api/status")
    oa_usage = [u for u in json.loads(body).get("usage", []) if u.get("provider") == "oa"]
    check("openai 用量提取", oa_usage and oa_usage[0].get("output_tokens", 0) >= 6,
          f"usage={oa_usage[:1]}")
    # count_tokens：OpenAI 无端点 → 启发式回退（D8）
    st, _, body = http_req("/v1/messages/count_tokens", data={
        "model": "oa-gpt-4o-mini", "messages": [{"role": "user", "content": "hi"}]}, method="POST")
    check("openai count_tokens 启发式 200",
          st == 200 and json.loads(body).get("input_tokens", 0) >= 1, f"body={body[:80]}")

    # 11b. Phase 59：上游中途断流 → relay 发规范 error 事件 + 干净收尾（不再 malformed 截断）
    ot_body = {
        "model": "ot-trunc-model",
        "max_tokens": 32000,
        "stream": True,
        "messages": [{"role": "user", "content": "你好"}],
    }
    st, _, resp = http_req("/v1/messages", data=ot_body, method="POST")
    ot_text = resp.decode("utf-8")
    check("断流 200 + 已产出事件", st == 200 and "event: message_start" in ot_text, f"status={st}")
    check("断流产出 error 事件", "event: error" in ot_text and '"api_error"' in ot_text,
          ot_text[-200:])
    check("断流错误含上游原因", "上游流中断" in ot_text, "")
    check("断流不发 message_stop", "event: message_stop" not in ot_text, "")
    path, _, bd = received[-1]
    check("断流 mock 收到 /trunc/chat/completions", path.endswith("/trunc/chat/completions"), path)
    check("断流请求已翻译 model", bd.get("model") == "trunc-model", f"model={bd.get('model')!r}")

    # 11b-2. Phase 61：上游「干净断流」（EOF 无终止事件，FIN 掐流）→ 同样发规范 error 事件
    oe_body = {
        "model": "oe-eof-model",
        "max_tokens": 32000,
        "stream": True,
        "messages": [{"role": "user", "content": "你好"}],
    }
    st, _, resp = http_req("/v1/messages", data=oe_body, method="POST")
    oe_text = resp.decode("utf-8")
    check("EOF断流 200 + 已产出事件", st == 200 and "event: message_start" in oe_text, f"status={st}")
    check("EOF断流产出 error 事件", "event: error" in oe_text and '"api_error"' in oe_text,
          oe_text[-200:])
    check("EOF断流错误含提前结束提示", "上游流提前结束" in oe_text, "")
    check("EOF断流不发 message_stop", "event: message_stop" not in oe_text, "")
    path, _, bd = received[-1]
    check("EOF断流 mock 收到 /eof/chat/completions", path.endswith("/eof/chat/completions"), path)
    check("EOF断流请求已翻译 model", bd.get("model") == "eof-model", f"model={bd.get('model')!r}")

    # 11c. Phase 60：模型行级协议覆盖——anthropic 服务商的模型行指定 openai → 走 /chat/completions
    mo_body = {
        "model": "mock-chat-openai",
        "max_tokens": 32000,
        "stream": True,
        "messages": [{"role": "user", "content": "你好"}],
    }
    st, _, resp = http_req("/v1/messages", data=mo_body, method="POST")
    mo_text = resp.decode("utf-8")
    check("模型行协议覆盖 200 + message_stop",
          st == 200 and "event: message_stop" in mo_text, f"status={st}")
    path, _, bd = received[-1]
    check("模型行协议覆盖 mock 收到 /chat/completions",
          path.endswith("/chat/completions") and "/trunc" not in path, path)
    check("模型行协议覆盖请求 model=真实名", bd.get("model") == "mock-chat-oa",
          f"model={bd.get('model')!r}")
    check("模型行协议覆盖产出 anthropic 事件",
          "event: message_start" in mo_text and "text_delta" in mo_text, "")

    # 12. Phase 56：Gemini 原生协议翻译（gem 服务商 protocol=gemini -> :streamGenerateContent）
    gem_body = {
        "model": "gem-pro",
        "system": "你是助手",
        "max_tokens": 32000,
        "stream": True,
        "messages": [
            {"role": "user", "content": "你好"},
            {"role": "assistant", "content": [{"type": "text", "text": "查一下"}]},
        ],
    }
    st, _, resp = http_req("/v1/messages", data=gem_body, method="POST")
    gem_text = resp.decode("utf-8")
    check("gemini 翻译 200 + message_stop", st == 200 and "event: message_stop" in gem_text,
          f"status={st}")
    path, hdrs, bd = received[-1]
    H = {k.lower(): v for k, v in hdrs.items()}
    check("gemini mock 收到 :streamGenerateContent", ":streamGenerateContent" in path, path)
    check("gemini URL 含真实模型名", "/v1beta/models/gemini-2.5-pro:" in path, path)
    contents = bd.get("contents", [])
    check("gemini 请求已翻译 contents",
          contents and contents[0].get("role") == "user"
          and contents[0].get("parts", [{}])[0].get("text") == "你好",
          f"contents={contents[:1]}")
    check("gemini assistant -> model 角色",
          len(contents) > 1 and contents[1].get("role") == "model", f"contents={contents}")
    check("gemini system -> systemInstruction",
          bd.get("systemInstruction", {}).get("parts", [{}])[0].get("text") == "你是助手",
          f"systemInstruction={bd.get('systemInstruction')}")
    check("gemini max_tokens -> generationConfig.maxOutputTokens",
          bd.get("generationConfig", {}).get("maxOutputTokens") == 32000,
          f"generationConfig={bd.get('generationConfig')}")
    check("gemini 认证 x-goog-api-key", H.get("x-goog-api-key") == "GEMINI_KEY_123",
          f"x-goog-api-key={H.get('x-goog-api-key')!r}")
    check("gemini 剥离 anthropic-version", "anthropic-version" not in H, f"headers={list(H)}")
    check("gemini anthropic 事件 message_start", "event: message_start" in gem_text, "")
    check("gemini text_delta 翻译", "你好" in gem_text, gem_text[:200])
    check("gemini functionCall -> tool_use", '"type":"tool_use"' in gem_text and "get_weather" in gem_text, "")
    check("gemini input_json_delta 翻译", '"input_json_delta"' in gem_text and "北京" in gem_text, "")
    check("gemini 收尾 end_turn", '"stop_reason":"end_turn"' in gem_text, "")
    # 用量提取：Gemini usageMetadata（prompt/candidates tokens）-> 共享统计
    st, _, body = http_req("/api/status")
    gem_usage = [u for u in json.loads(body).get("usage", []) if u.get("provider") == "gem"]
    check("gemini 用量提取", gem_usage and gem_usage[0].get("output_tokens", 0) >= 5,
          f"usage={gem_usage[:1]}")
    # 非流式：stream=false -> :generateContent JSON -> Anthropic message
    gem_ns = {"model": "gem-pro", "stream": False, "max_tokens": 100,
              "messages": [{"role": "user", "content": "你好"}]}
    st, _, resp = http_req("/v1/messages", data=gem_ns, method="POST")
    ns = json.loads(resp)
    check("gemini 非流式翻译 message",
          st == 200 and ns.get("type") == "message" and ns.get("model") == "gem-pro"
          and ns.get("stop_reason") == "end_turn"
          and ns.get("content", [{}])[0].get("text") == "结果"
          and ns.get("usage", {}).get("input_tokens") == 7,
          f"status={st} body={resp[:200]}")
    check("gemini 非流式 URL :generateContent",
          received[-1][0].endswith(":generateContent"), received[-1][0])
    # count_tokens：Gemini 无端点 -> 启发式回退（D8）
    st, _, body = http_req("/v1/messages/count_tokens", data={
        "model": "gem-pro", "messages": [{"role": "user", "content": "hi"}]}, method="POST")
    check("gemini count_tokens 启发式 200",
          st == 200 and json.loads(body).get("input_tokens", 0) >= 1, f"body={body[:80]}")

    # 13. Phase 58：OpenCode Go 网关——注入 x-opencode-session + 专属 UA
    #   ocmock 的 console_usage_url 含 /workspace/（/go 页地址）→ is_opencode() 命中
    #   a) 入站带 CC 原生会话头 x-claude-code-session-id + claude-cli UA → 原样映射为
    #      x-opencode-session / 透传 UA 到上游
    sess_id = "session-e2e-opencode-001"
    oc_body = {"model": "ocmock-oc-chat", "max_tokens": 100, "stream": True,
               "messages": [{"role": "user", "content": "你好"}]}
    st, _, resp = http_req("/v1/messages", data=oc_body, method="POST",
                           extra={"x-claude-code-session-id": sess_id,
                                  "user-agent": "claude-cli/2.1.96 (external, cli)"})
    check("opencode 会话请求 200", st == 200, f"status={st}")
    path, hdrs, bd = received[-1]
    H = {k.lower(): v for k, v in hdrs.items()}
    check("opencode mock 收到 /chat/completions", path.endswith("/chat/completions"), path)
    check("opencode x-opencode-session = CC 会话 id", H.get("x-opencode-session") == sess_id,
          f"x-opencode-session={H.get('x-opencode-session')!r}")
    check("opencode UA 透传 claude-cli", H.get("user-agent") == "claude-cli/2.1.96 (external, cli)",
          f"user-agent={H.get('user-agent')!r}")
    # b) 不带会话头 → 进程级回退值（非空、不以 reqwest 命名），UA 兜底 ClaudeRelay/<v>
    st, _, resp = http_req("/v1/messages", data=oc_body, method="POST")
    check("opencode 无会话头仍 200", st == 200, f"status={st}")
    path, hdrs, bd = received[-1]
    H = {k.lower(): v for k, v in hdrs.items()}
    fallback_sess = H.get("x-opencode-session", "")
    check("opencode 回退会话 id 非空", bool(fallback_sess), f"x-opencode-session={fallback_sess!r}")
    ua = H.get("user-agent", "")
    check("opencode UA 非 reqwest 通用库名", ua.startswith("ClaudeRelay/") or "claude" in ua.lower(),
          f"user-agent={ua!r}")
    # c) 非 opencode 服务商（ds mock）不受影响——不注入 x-opencode-session，UA 保持 reqwest 透传
    st, _, resp = http_req("/v1/messages", data={"model": ALIAS, "max_tokens": 100, "stream": True,
                                                 "messages": [{"role": "user", "content": "hi"}]},
                           method="POST")
    check("非 opencode 仍 200", st == 200, f"status={st}")
    path, hdrs, bd = received[-1]
    H = {k.lower(): v for k, v in hdrs.items()}
    check("非 opencode 不注入 x-opencode-session", "x-opencode-session" not in H,
          f"headers={list(H)}")

    print(f"\n[e2e] ALL PASS ({PASS} checks)")
finally:
    srv.shutdown()
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    logf.close()
