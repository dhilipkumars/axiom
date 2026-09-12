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

	"k8s.io/client-go/discovery"
	"k8s.io/client-go/discovery/cached/memory"
	authv1client "k8s.io/client-go/kubernetes/typed/authorization/v1"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
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
	kubeconfig := fs.String("kubeconfig", "", "path to a kubeconfig; empty means in-cluster config")
	noCluster := fs.Bool("no-cluster", false, "serve Ping only; Get/List fail with FAILED_PRECONDITION (Phase 0 plumbing mode)")
	serve := fs.String("serve", k8s.DefaultServe,
		"comma-separated resources this gateway serves, as `plural[.group]` "+
			"(e.g. \"pods,configmaps,widgets.example.com\"); \"*.group\" covers a whole group. "+
			"Scope the ServiceAccount's RBAC to match: this bounds what is offered, RBAC enforces it")
	if err := fs.Parse(args); err != nil {
		return err
	}

	logger := slog.New(slog.NewJSONHandler(stderr, nil))

	// Cluster access is explicit: either credentials load successfully or the
	// operator opted into --no-cluster. A silently unconfigured gateway would
	// turn every SELECT into a misleading error (docs/RULES.md §1).
	// The serve list bounds which kinds discovery may offer. It is parsed
	// before any cluster contact so a typo fails at startup rather than
	// turning into a silently narrower gateway (docs/RULES.md §1, §3).
	allow, err := k8s.ParseAllowlist(*serve)
	if err != nil {
		return err
	}
	if allow.IsEmpty() {
		return fmt.Errorf("--serve is empty: a gateway that serves no resources cannot answer any query")
	}

	var client k8s.Client
	if *noCluster {
		client = k8s.Unconfigured{}
		logger.Warn("running with --no-cluster: Get/List will fail with FAILED_PRECONDITION")
	} else {
		cfg, err := k8s.Config(*kubeconfig)
		if err != nil {
			return fmt.Errorf("%w (pass --no-cluster to run without cluster access)", err)
		}
		// Never log the loaded config: it holds the bearer token / client key.
		disco, err := discovery.NewDiscoveryClientForConfig(cfg)
		if err != nil {
			return fmt.Errorf("k8s: discovery client: %w", err)
		}
		// RBAC is the boundary: the gateway asks the API server which kinds
		// its own identity may list, so the served set follows the
		// ServiceAccount rather than a second list that must be hand-synced.
		authz, err := authv1client.NewForConfig(cfg)
		if err != nil {
			return fmt.Errorf("k8s: authorization client: %w", err)
		}
		access := k8s.NewSelfAccess(authz.SelfSubjectAccessReviews())
		mapper := k8s.NewDiscovery(memory.NewMemCacheClient(disco), allow, access, logger)
		dyn, err := k8s.NewFromConfig(cfg, mapper)
		if err != nil {
			return err
		}
		client = dyn
		// Which of the two bounds is actually narrowing is worth knowing when
		// a kind is unexpectedly missing.
		logger.Info("kubernetes client configured",
			"api_server", cfg.Host,
			"serve", allow.String(),
			"bounded_by", map[bool]string{true: "rbac", false: "rbac+serve"}[allow.ServesEverything()])
	}

	tlsCfg, err := tlsconfig.Load(*certPath, *keyPath)
	if err != nil {
		return err
	}

	lis, err := net.Listen("tcp", *listen)
	if err != nil {
		return fmt.Errorf("listen %q: %w", *listen, err)
	}

	gs := grpc.NewServer(grpc.Creds(credentials.NewTLS(tlsCfg)))
	axiomv1.RegisterGatewayServiceServer(gs, server.New(version, nil, client, logger))
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
