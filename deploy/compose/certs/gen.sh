#!/bin/sh
# Generates a throwaway CA + gateway server certificate into /certs for the
# local compose stack. Test-only material: regenerated on every `up`, never
# committed. Phase 6 adds a client cert here for mTLS.
set -eu
cd /certs
rm -f ./*.crt ./*.key ./*.csr ./*.srl
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 2 \
  -subj "/CN=axiom-dev-ca" -keyout ca.key -out ca.crt >/dev/null 2>&1
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -subj "/CN=gateway" -keyout gateway.key -out gateway.csr >/dev/null 2>&1
printf 'subjectAltName=DNS:gateway,DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > san.cnf
openssl x509 -req -in gateway.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 2 \
  -extfile san.cnf -out gateway.crt >/dev/null 2>&1
rm -f gateway.csr san.cnf ca.srl ca.key
# The gateway runs as distroless `nonroot` (uid 65532); only it may read the key.
chown 65532:65532 gateway.key gateway.crt
chmod 0600 gateway.key
chmod 0644 gateway.crt ca.crt
echo "certs: generated CA and gateway certificate"
