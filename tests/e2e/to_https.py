"""Production Caddyfile -> local e2e variant.

Routing, matchers, headers and TLS behaviour stay as rendered by the infra
module; only the certificate source (Caddy's local CA instead of Let's
Encrypt), backend addresses and file roots change.
"""
import re
import sys

src, root = sys.argv[1], sys.argv[2]
s = open(src).read()
s = re.sub(r"^\{\n", "{\n\tlocal_certs\n\tadmin off\n", s, count=1, flags=re.M)
s = s.replace("ask http://gateway:8080/_internal/tls-ask", "ask http://127.0.0.2:8080/_internal/tls-ask")
for name, addr in {
    "gateway:8080": "127.0.0.2:8080",
    "oauth2-proxy-connect:4180": "127.0.0.6:4180",
    "oauth2-proxy:4180": "127.0.0.3:4180",
    "platform-mcp:8080": "127.0.0.5:8080",
}.items():
    s = s.replace(name, addr)
s = s.replace("/srv/apps/", root + "/apps/").replace("root * /srv/llms", "root * " + root + "/llms")
s = s.replace("root * /srv\n", "root * " + root + "\n")
print(s)
