"""Checks the certificates in a ztunnel config_dump (read from stdin).

Usage: verify_certs.py <spire bundle pem> <node name>

The pod of the test on the node must have a workload-keyed entry (one per pod,
the SPIRE mode) that is Available, whose leaf names its SPIFFE ID and chains
to the SPIRE bundle, and whose roots are that bundle.
"""

import base64
import json
import os
import subprocess
import sys
import tempfile

EXPECTED = {
    "ztunnel-spire-worker": "spiffe://cluster.local/ns/spire-e2e/sa/client",
    "ztunnel-spire-worker2": "spiffe://cluster.local/ns/spire-e2e/sa/server",
}


def pems(text):
    out, cur = [], []
    for line in text.splitlines():
        if "BEGIN CERTIFICATE" in line:
            cur = [line]
        elif "END CERTIFICATE" in line:
            cur.append(line)
            out.append("\n".join(cur) + "\n")
            cur = []
        elif cur:
            cur.append(line.strip())
    return out


def der(pem):
    """The certificate itself, whatever the line wrapping of its PEM."""
    return "".join(pem.split("\n")[1:-2])


def dumped(cert):
    """The dump holds each PEM base64-encoded, as the Envoy admin API does."""
    return pems(base64.b64decode(cert["pem"]).decode())


def openssl(*args, data=None):
    return subprocess.run(["openssl", *args], input=data, capture_output=True, text=True)


def main():
    bundle_path, node = sys.argv[1], sys.argv[2]
    want = EXPECTED[node]
    dump = json.load(sys.stdin)
    entries = [c for c in dump.get("certificates", []) if c["identity"] == want]
    if not entries:
        sys.exit(f"{node}: no certificate for {want}")
    for e in entries:
        if "uid" not in e:
            sys.exit(f"{node}: {want} is identity-keyed, not per pod (Istio CA path?)")
        if e["state"] != "Available":
            sys.exit(f"{node}: {want} is {e['state']}")

        bundle = {der(p) for p in pems(open(bundle_path).read())}
        roots = {der(p) for r in e["rootCerts"] for p in dumped(r)}
        if roots != bundle:
            sys.exit(f"{node}: roots of {want} are not the SPIRE bundle")

        chain = [p for c in e["certChain"] for p in dumped(c)]
        if not chain:
            sys.exit(f"{node}: {want} has an empty chain")
        text = openssl("x509", "-noout", "-text", data=chain[0]).stdout
        if f"URI:{want}" not in text:
            sys.exit(f"{node}: leaf does not name {want}")
        with tempfile.TemporaryDirectory() as d:
            leaf, inter = os.path.join(d, "leaf.pem"), os.path.join(d, "inter.pem")
            open(leaf, "w").write(chain[0])
            args = ["verify", "-CAfile", bundle_path]
            if len(chain) > 1:
                open(inter, "w").write("".join(chain[1:]))
                args += ["-untrusted", inter]
            res = openssl(*args, leaf)
            if res.returncode != 0:
                sys.exit(f"{node}: leaf of {want} does not chain to the bundle: {res.stdout}{res.stderr}")
        print(f"{node}: {want} uid={e['uid']} chains to the SPIRE bundle")


if __name__ == "__main__":
    main()
