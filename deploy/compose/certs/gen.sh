#!/bin/sh
# Generates a throwaway CA + gateway server certificate into /certs for the
# local compose stack. Test-only material, never committed. Phase 7 adds a
# client cert here for mTLS.
#
# Generation is idempotent on purpose. This runs on every `compose up`, and
# `up` only recreates containers whose configuration changed -- so regenerating
# unconditionally rotated the CA out from under an already-running gateway,
# which kept serving the certificate it loaded at startup. Every call then
# failed with `invalid peer certificate: BadSignature` until something
# restarted the gateway. That race made the watch E2E gate flaky and cost real
# time to diagnose twice; reusing still-valid material removes it entirely.
#
# Expiry is the one case that still forces a rotation, and a rotation against a
# running gateway is exactly the broken state above. Two things keep that out of
# reach: the validity below is long enough that no dev stack or CI run reaches
# it (these are throwaway certificates for a local network, not something whose
# short lifetime buys security), and the check refuses to rotate under a gateway
# that is already running, failing loudly instead. Rotate deliberately with
# `make down`, which drops the volume, or `docker compose rm -sf gateway` first.
set -eu
cd /certs

if [ -s ca.crt ] && [ -s gateway.crt ] && [ -s gateway.key ] &&
   openssl verify -CAfile ca.crt gateway.crt >/dev/null 2>&1; then
  if openssl x509 -in gateway.crt -noout -checkend 86400 >/dev/null 2>&1; then
    echo "certs: reusing existing CA and gateway certificate"
    exit 0
  fi
  # Material exists but is expiring. Replacing it under a running gateway is
  # the BadSignature race, so say so rather than causing it silently.
  echo "certs: existing certificate expires within 24h and cannot be rotated in place." >&2
  echo "certs: run 'make down' (drops the certs volume) and start again." >&2
  exit 1
fi

rm -f ./*.crt ./*.key ./*.csr ./*.srl
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 365 \
  -subj "/CN=axiom-dev-ca" -keyout ca.key -out ca.crt >/dev/null 2>&1
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -subj "/CN=gateway" -keyout gateway.key -out gateway.csr >/dev/null 2>&1
printf 'subjectAltName=DNS:gateway,DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > san.cnf
openssl x509 -req -in gateway.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 365 \
  -extfile san.cnf -out gateway.crt >/dev/null 2>&1
rm -f gateway.csr san.cnf ca.srl ca.key
# The gateway runs as distroless `nonroot` (uid 65532); only it may read the key.
chown 65532:65532 gateway.key gateway.crt
chmod 0600 gateway.key
chmod 0644 gateway.crt ca.crt
echo "certs: generated CA and gateway certificate"
