#!/usr/bin/env python3
"""`python3 -m http.server PORT [--bind ADDR] [--directory DIR]` for scripts/test/e2e.sh, without
the reverse DNS lookup of its own address that http.server does between bind() and listen()
(socket.getfqdn in HTTPServer.server_bind). Where that lookup is slow (GitHub's macOS runners),
the port is taken but not listening for seconds, and macOS drops connections to it unanswered
instead of refusing them, so a test that asks too early hangs."""
import argparse
import functools
import http.server
import socketserver


class Server(http.server.ThreadingHTTPServer):
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("--bind", default="")
    ap.add_argument("--directory", default=None)
    a = ap.parse_args()
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=a.directory)
    Server((a.bind, a.port), handler).serve_forever()


if __name__ == "__main__":
    main()
