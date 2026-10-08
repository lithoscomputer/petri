#!/usr/bin/env python3
"""Exercise the scripted HTTP server with overlapping persistent connections."""

import http.client
import json
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import patch

import mcp_server


class HttpConnections(unittest.TestCase):
    def setUp(self):
        self.connections = []

        def start(server_class, port, handler):
            self.httpd = server_class(("127.0.0.1", port), handler)
            self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
            self.thread.start()

        # Bind port zero without reserving and releasing a port first. The
        # real serve_http chooses the server class and request handler.
        with patch.object(mcp_server, "serve_forever", start):
            mcp_server.serve_http(mcp_server.Server(SimpleNamespace(slow_init=0)), 0)
        self.port = self.httpd.server_address[1]

    def tearDown(self):
        for connection in self.connections:
            connection.close()
        self.httpd.shutdown()
        self.thread.join(timeout=5)
        self.httpd.server_close()
        self.assertFalse(self.thread.is_alive(), "the HTTP server stopped")

    def connection(self):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=2)
        self.connections.append(connection)
        return connection

    def post(self, connection, request, status=200):
        connection.request(
            "POST", "/", json.dumps(request), {"Content-Type": "application/json"}
        )
        response = connection.getresponse()
        self.assertEqual(response.status, status)
        self.assertFalse(response.will_close, "the connection stays open")
        body = response.read()
        return json.loads(body) if body else None

    def initialize(self, connection):
        response = self.post(connection, {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": mcp_server.PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "connection-test", "version": "1"},
            },
        })
        self.assertEqual(response["result"]["serverInfo"], mcp_server.SERVER_INFO)

    def finish_handshake_and_call(self, connection):
        self.post(connection, {
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }, status=202)
        tools = self.post(connection, {
            "jsonrpc": "2.0", "id": 2, "method": "tools/list"
        })
        self.assertIn("echo", [tool["name"] for tool in tools["result"]["tools"]])
        result = self.post(connection, {
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "still reachable"}},
        })
        self.assertEqual(result["result"]["content"][0]["text"], "still reachable")

    def test_idle_readiness_connection_does_not_block_handshake(self):
        probe = self.connection()
        probe.request("GET", "/")
        response = probe.getresponse()
        self.assertEqual(response.status, 405)
        self.assertFalse(response.will_close)
        response.read()
        client = self.connection()
        self.initialize(client)
        self.finish_handshake_and_call(client)

    def test_initialized_can_use_a_new_connection_while_initialize_stays_open(self):
        first = self.connection()
        self.initialize(first)
        # A pooled client can open another socket before reusing the one that
        # carried initialize. Keep the first alive to force that ordering.
        second = self.connection()
        self.finish_handshake_and_call(second)


if __name__ == "__main__":
    unittest.main()
