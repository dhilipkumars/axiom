// Package tlsconfig builds the server-side TLS configuration for the gateway.
//
// The Postgres↔gateway hop is a real network hop from Phase 0, so it is TLS
// from Phase 0 (docs/RULES.md §3). Phase 6 upgrades this to mTLS by adding a
// client CA + RequireAndVerifyClientCert; nothing here should need to be torn
// out for that.
package tlsconfig

import (
	"crypto/tls"
	"errors"
	"fmt"
	"os"
)

// ErrMissingPath is returned when a cert or key path is empty.
var ErrMissingPath = errors.New("tlsconfig: certificate and key paths are both required")

// Load reads a PEM certificate chain and private key from disk and returns a
// TLS 1.3-minimum server configuration.
//
// Contract: returns ErrMissingPath if either path is empty; returns a wrapped
// error if the files cannot be read or do not parse as a matching key pair.
// Error messages include the file *paths* but never key material.
func Load(certPath, keyPath string) (*tls.Config, error) {
	if certPath == "" || keyPath == "" {
		return nil, ErrMissingPath
	}
	certPEM, err := os.ReadFile(certPath) //nolint:gosec // path comes from the operator's CLI flag, by design
	if err != nil {
		return nil, fmt.Errorf("tlsconfig: read certificate %q: %w", certPath, err)
	}
	keyPEM, err := os.ReadFile(keyPath) //nolint:gosec // path comes from the operator's CLI flag, by design
	if err != nil {
		return nil, fmt.Errorf("tlsconfig: read key %q: %w", keyPath, err)
	}
	cert, err := tls.X509KeyPair(certPEM, keyPEM)
	if err != nil {
		// tls.X509KeyPair errors describe structure, not contents; safe to wrap.
		return nil, fmt.Errorf("tlsconfig: parse key pair (%q, %q): %w", certPath, keyPath, err)
	}
	return &tls.Config{
		Certificates: []tls.Certificate{cert},
		MinVersion:   tls.VersionTLS13,
		NextProtos:   []string{"h2"},
	}, nil
}
