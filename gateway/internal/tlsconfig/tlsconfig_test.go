package tlsconfig

import (
	"crypto/tls"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/dhilipkumars/axiom/gateway/internal/testcert"
)

func TestLoad(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	certPath, keyPath := testcert.Write(t, dir)
	garbage := filepath.Join(dir, "garbage.pem")
	if err := os.WriteFile(garbage, []byte("not pem"), 0o600); err != nil {
		t.Fatal(err)
	}

	tests := []struct {
		name          string
		cert, key     string
		wantErr       bool
		wantErrIs     error
		wantErrSubstr string
	}{
		{name: "valid pair", cert: certPath, key: keyPath},
		{name: "empty cert path", cert: "", key: keyPath, wantErr: true, wantErrIs: ErrMissingPath},
		{name: "empty key path", cert: certPath, key: "", wantErr: true, wantErrIs: ErrMissingPath},
		{name: "missing cert file", cert: filepath.Join(dir, "nope.crt"), key: keyPath, wantErr: true, wantErrIs: os.ErrNotExist},
		{name: "missing key file", cert: certPath, key: filepath.Join(dir, "nope.key"), wantErr: true, wantErrIs: os.ErrNotExist},
		{name: "garbage key", cert: certPath, key: garbage, wantErr: true, wantErrSubstr: "parse key pair"},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			cfg, err := Load(tc.cert, tc.key)
			if tc.wantErr {
				if err == nil {
					t.Fatal("expected error")
				}
				if tc.wantErrIs != nil && !errors.Is(err, tc.wantErrIs) {
					t.Fatalf("err = %v, want errors.Is %v", err, tc.wantErrIs)
				}
				if tc.wantErrSubstr != "" && !strings.Contains(err.Error(), tc.wantErrSubstr) {
					t.Fatalf("err = %v, want substring %q", err, tc.wantErrSubstr)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if cfg.MinVersion != tls.VersionTLS13 {
				t.Errorf("MinVersion = %x, want TLS1.3", cfg.MinVersion)
			}
			if len(cfg.Certificates) != 1 {
				t.Errorf("certificates = %d, want 1", len(cfg.Certificates))
			}
		})
	}
}
