#!/usr/bin/env bash
# End-to-end test of the SPIRE mode of ztunnel in kind: SPIRE, Istio ambient
# with the ztunnel image under test, a client and a server on different
# workers. It checks that
#   1. the registered client reaches the server through the mesh;
#   2. the certificates ztunnel holds for both pods are SVIDs issued under the
#      SPIRE bundle, one cache entry per pod;
#   3. a pod in the mesh without a SPIRE registration gets no certificate and
#      cannot reach the server.
#
# Usage: ZTUNNEL_IMAGE=<image> hack/spire-e2e/run.sh [--keep]
# --keep leaves the cluster up for inspection; otherwise it is deleted.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ZTUNNEL_IMAGE=${ZTUNNEL_IMAGE:-ghcr.io/fetchfy-cloud/ztunnel:dev}
KIND=${KIND:-kind}
CLUSTER=ztunnel-spire
ISTIO_VERSION=1.32.0
ISTIO_REPO=https://blob.istio.io/istio-release/charts
SPIRE_REPO=https://spiffe.github.io/helm-charts-hardened/
SPIRE_VERSION=0.31.0
SPIRE_CRDS_VERSION=0.6.1
KEEP=${1:-}

export KUBECONFIG="${KUBECONFIG_E2E:-$HERE/.kubeconfig}"

cleanup() {
  if [[ "$KEEP" != "--keep" ]]; then
    "$KIND" delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    rm -f "$KUBECONFIG"
  fi
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

step "kind cluster"
"$KIND" create cluster --config "$HERE/kind.yaml" --kubeconfig "$KUBECONFIG" --wait 120s
"$KIND" load docker-image "$ZTUNNEL_IMAGE" --name "$CLUSTER"

step "SPIRE $SPIRE_VERSION"
helm upgrade --install spire-crds spire-crds --repo "$SPIRE_REPO" --version "$SPIRE_CRDS_VERSION" \
  -n spire-mgmt --create-namespace --wait
helm upgrade --install spire spire --repo "$SPIRE_REPO" --version "$SPIRE_VERSION" \
  -n spire-mgmt -f "$HERE/spire-values.yaml" \
  --set global.spire.namespaces.create=true --wait --timeout 10m
kubectl apply -f "$HERE/clusterspiffeids.yaml"

step "Istio $ISTIO_VERSION ambient, ztunnel $ZTUNNEL_IMAGE"
helm upgrade --install istio-base base --repo "$ISTIO_REPO" --version "$ISTIO_VERSION" \
  -n istio-system --create-namespace --wait
helm upgrade --install istiod istiod --repo "$ISTIO_REPO" --version "$ISTIO_VERSION" \
  -n istio-system --set profile=ambient --wait
helm upgrade --install istio-cni cni --repo "$ISTIO_REPO" --version "$ISTIO_VERSION" \
  -n istio-system --set profile=ambient --wait
helm upgrade --install ztunnel ztunnel --repo "$ISTIO_REPO" --version "$ISTIO_VERSION" \
  -n istio-system -f "$HERE/ztunnel-values.yaml" \
  --set image="$ZTUNNEL_IMAGE" --set imagePullPolicy=Never --wait --timeout 5m

step "workloads"
kubectl apply -f "$HERE/workloads.yaml"
kubectl -n spire-e2e wait --for=condition=Ready pod/server pod/client --timeout 180s
kubectl -n spire-e2e-unregistered wait --for=condition=Ready pod/client --timeout 180s

step "1. registered client reaches the server"
# The first fetch can come before the controller manager registers the pod;
# ztunnel then retries with a backoff of up to 150s.
ok=""
for _ in $(seq 1 60); do
  if out=$(kubectl -n spire-e2e exec client -- curl -sS --max-time 5 http://server:8080/hostname 2>&1); then
    ok=1; break
  fi
  sleep 5
done
[[ -n "$ok" ]] || fail "client could not reach the server: $out"
echo "server answered: $out"

step "2. certificates are SVIDs under the SPIRE bundle"
bundle=$(mktemp)
kubectl -n spire-mgmt exec spire-server-0 -c spire-server -- \
  /opt/spire/bin/spire-server bundle show -format pem > "$bundle"
for node in ztunnel-spire-worker ztunnel-spire-worker2; do
  pod=$(kubectl -n istio-system get pod -l app=ztunnel --field-selector spec.nodeName="$node" \
    -o jsonpath='{.items[0].metadata.name}')
  # The admin endpoint listens on localhost inside the pod.
  kubectl -n istio-system port-forward "pod/$pod" 25000:15000 >/dev/null 2>&1 &
  pf=$!
  sleep 3
  dump=$(curl -sS --max-time 10 http://localhost:25000/config_dump) || { kill "$pf"; fail "config_dump of $pod"; }
  kill "$pf"
  python3 -I "$HERE/verify_certs.py" "$bundle" "$node" <<<"$dump" || fail "certificates on $node"
done

step "3. unregistered pod gets no certificate"
if kubectl -n spire-e2e-unregistered exec client -- curl -sS --max-time 5 http://server.spire-e2e:8080/hostname; then
  fail "unregistered client reached the server"
fi
echo "unregistered client was refused, as expected"
# ztunnel logs the refusal when its request to SPIRE times out (SPIRE_TIMEOUT,
# 10s), after curl has given up.
logged=""
for _ in $(seq 1 12); do
  if kubectl -n istio-system logs -l app=ztunnel --tail=-1 \
    | grep "ns/spire-e2e-unregistered" | grep -q "Timeout while waiting for SVID"; then
    logged=1; break
  fi
  sleep 5
done
[[ -n "$logged" ]] || fail "ztunnel logs do not show the failed attestation of the unregistered pod"

echo "PASS"
