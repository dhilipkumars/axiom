// Package cli holds the gateway's command-line surface.
//
// It exists so the flag definitions have exactly one home. The binary
// registers them here to parse its arguments, and `make docs-generate` walks
// the same FlagSet to produce docs/generated/gateway-flags.md (docs/PLAN.md
// Phase 6 Part 2). The reference page therefore reports the real defaults and
// the real help text: a flag renamed, defaulted differently or removed changes
// the page, and CI fails on the uncommitted diff.
//
// Nothing here reads the environment or touches the cluster, so the generator
// can register the flags without starting a gateway.
package cli

import (
	"flag"
	"time"

	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// Options holds the parsed command line. The fields are pointers because they
// are bound by flag.FlagSet before Parse runs.
type Options struct {
	// Listen is the TCP address the gRPC server binds.
	Listen *string
	// CertPath and KeyPath locate the PEM server keypair. TLS is mandatory
	// (docs/RULES.md §3), so both are required.
	CertPath *string
	KeyPath  *string
	// Kubeconfig selects an explicit kubeconfig. Empty is the in-cluster path,
	// which is how the gateway runs as a Deployment.
	Kubeconfig *string
	// NoCluster serves Ping only, for plumbing checks without a cluster.
	NoCluster *bool
	// Serve bounds which resources the gateway offers, before RBAC.
	Serve *string
	// DiscoveryTTL is how long a group-version's resource list is trusted.
	DiscoveryTTL *time.Duration
}

// Register binds every gateway flag onto fs and returns the bound options.
func Register(fs *flag.FlagSet, defaultServe string) *Options {
	return &Options{
		Listen:     fs.String("listen", ":8443", "TCP address to listen on"),
		CertPath:   fs.String("tls-cert", "", "path to PEM server certificate (required)"),
		KeyPath:    fs.String("tls-key", "", "path to PEM server private key (required)"),
		Kubeconfig: fs.String("kubeconfig", "", "path to a kubeconfig; empty means in-cluster config"),
		NoCluster: fs.Bool("no-cluster", false,
			"serve Ping only; Get/List fail with FAILED_PRECONDITION (Phase 0 plumbing mode)"),
		DiscoveryTTL: fs.Duration("discovery-ttl", k8s.DefaultResourceTTL,
			"how long a cached list of an API group's resources is trusted before "+
				"being refetched; lower it to notice a deleted custom resource sooner, "+
				"at the cost of more discovery traffic"),
		Serve: fs.String("serve", defaultServe,
			"comma-separated resources this gateway serves, as `plural[.group]` "+
				"(e.g. \"pods,configmaps,widgets.example.com\"); \"*.group\" covers a whole group. "+
				"Scope the ServiceAccount's RBAC to match: this bounds what is offered, RBAC enforces it"),
	}
}
