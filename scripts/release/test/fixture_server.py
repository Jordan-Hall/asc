#!/usr/bin/env python3
"""A local release host for the installer tests.

    fixture_server.py <directory> <port file> <request log>

Serves <directory> read-only on 127.0.0.1 and an ephemeral port. The port is written to <port file>
once the socket listens, and every request path is appended to <request log>, so a test can say what
the installer did and did not ask for. The test kills it when done.
"""

from __future__ import annotations

import functools
import http.server
import sys


class Handler(http.server.SimpleHTTPRequestHandler):
    log_path = ""

    def do_GET(self) -> None:  # noqa: N802 (stdlib name)
        with open(self.log_path, "a", encoding="utf-8") as f:
            f.write(self.path + "\n")
        super().do_GET()

    def log_message(self, fmt: str, *args: object) -> None:  # silence the access log
        pass


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    directory, port_file, log_file = argv
    open(log_file, "w", encoding="utf-8").close()
    Handler.log_path = log_file
    handler = functools.partial(Handler, directory=directory)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    with open(port_file, "w", encoding="utf-8") as f:
        f.write(str(server.server_address[1]))
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
