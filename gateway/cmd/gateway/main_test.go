package main

import (
	"context"
	"errors"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/dhilipkumars/axiom/gateway/internal/testcert"
	"github.com/dhilipkumars/axiom/gateway/internal/tlsconfig"
)

func TestRunRejectsMissingTLS(t *testing.T) {
	t.Parallel()
	err := run(context.Background(), []string{"-listen", "127.0.0.1:0"}, os.Stderr)
	if !errors.Is(err, tlsconfig.ErrMissingPath) {
		t.Fatalf("err = %v, want ErrMissingPath (plaintext must be impossible)", err)
	}
}

func TestRunRejectsBadFlag(t *testing.T) {
	t.Parallel()
	devnull, err := os.OpenFile(os.DevNull, os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if cerr := devnull.Close(); cerr != nil {
			t.Error(cerr)
		}
	}()
	err = run(context.Background(), []string{"-no-such-flag"}, devnull)
	if err == nil || !strings.Contains(err.Error(), "no-such-flag") {
		t.Fatalf("err = %v, want flag parse error", err)
	}
}

func TestRunStopsOnContextCancel(t *testing.T) {
	t.Parallel()
	certPath, keyPath := testcert.Write(t, t.TempDir())
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- run(ctx, []string{"-listen", "127.0.0.1:0", "-tls-cert", certPath, "-tls-key", keyPath}, os.Stderr)
	}()
	time.Sleep(200 * time.Millisecond)
	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("run returned %v after cancel, want nil", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("run did not return after context cancel")
	}
}
