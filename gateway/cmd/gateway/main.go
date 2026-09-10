// Command gateway runs the Axiom gateway gRPC server.
//
// Phase 0: serves only GatewayService.Ping, over TLS. There is deliberately no
// plaintext option (docs/RULES.md §3): the Postgres↔gateway hop is encrypted
// from the first phase that has a network hop.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"syscall"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/health"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/server"
	"github.com/dhilipkumars/axiom/gateway/internal/tlsconfig"
)

// version is overridden at build time via -ldflags "-X main.version=...".
var version = "dev"

func main() {
	if err := run(context.Background(), os.Args[1:], os.Stderr); err != nil {
		fmt.Fprintln(os.Stderr, "gateway:", err)
		os.Exit(1)
	}
}

func run(ctx context.Context, args []string, stderr *os.File) error {
	fs := flag.NewFlagSet("gateway", flag.ContinueOnError)
	fs.SetOutput(stderr)
	listen := fs.String("listen", ":8443", "TCP address to listen on")
	certPath := fs.String("tls-cert", "", "path to PEM server certificate (required)")
	keyPath := fs.String("tls-key", "", "path to PEM server private key (required)")
	if err := fs.Parse(args); err != nil {
		return err
	}

	logger := slog.New(slog.NewJSONHandler(stderr, nil))

	tlsCfg, err := tlsconfig.Load(*certPath, *keyPath)
	if err != nil {
		return err
	}

	lis, err := net.Listen("tcp", *listen)
	if err != nil {
		return fmt.Errorf("listen %q: %w", *listen, err)
	}

	gs := grpc.NewServer(grpc.Creds(credentials.NewTLS(tlsCfg)))
	axiomv1.RegisterGatewayServiceServer(gs, server.New(version, nil))
	hs := health.NewServer()
	hs.SetServingStatus(axiomv1.GatewayService_ServiceDesc.ServiceName, healthpb.HealthCheckResponse_SERVING)
	healthpb.RegisterHealthServer(gs, hs)

	ctx, stop := signal.NotifyContext(ctx, syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	errCh := make(chan error, 1)
	go func() { errCh <- gs.Serve(lis) }()
	logger.Info("gateway listening", "addr", lis.Addr().String(), "version", version, "tls", "required")

	select {
	case <-ctx.Done():
		logger.Info("shutting down")
		gs.GracefulStop()
		// Serve returns nil after GracefulStop; drain the channel so the goroutine exits.
		if serr := <-errCh; serr != nil && !errors.Is(serr, grpc.ErrServerStopped) {
			return serr
		}
		return nil
	case serr := <-errCh:
		return fmt.Errorf("serve: %w", serr)
	}
}
