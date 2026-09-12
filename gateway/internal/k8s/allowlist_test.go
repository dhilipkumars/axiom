package k8s

import (
	"strings"
	"testing"
)

func TestParseAllowlist(t *testing.T) {
	t.Parallel()
	for _, tc := range []struct {
		name    string
		in      string
		wantErr string
		// permits/denies are "group/plural" probes.
		permits []string
		denies  []string
	}{
		{
			// As of Phase 5 the served set is bounded by the gateway's RBAC,
			// so the allowlist defaults to imposing no narrowing of its own.
			name:    "the default narrows nothing; RBAC is the boundary",
			in:      DefaultServe,
			permits: []string{"/pods", "/configmaps", "/secrets", "example.com/widgets", "apps/deployments"},
		},
		{
			name:    "the every-group wildcard spans groups",
			in:      "*.*",
			permits: []string{"/pods", "a.io/things", "b.io/others"},
		},
		{
			name:    "a narrower list still hides kinds RBAC would allow",
			in:      "pods,configmaps",
			permits: []string{"/pods", "/configmaps"},
			denies:  []string{"/secrets", "/nodes", "example.com/widgets"},
		},
		{
			name:    "qualified entry binds to its group only",
			in:      "widgets.example.com",
			permits: []string{"example.com/widgets"},
			denies:  []string{"/widgets", "other.com/widgets"},
		},
		{
			name:    "wildcard covers a whole group but never another group",
			in:      "*.example.com",
			permits: []string{"example.com/widgets", "example.com/gadgets"},
			denies:  []string{"/pods", "other.com/widgets"},
		},
		{
			name:    "bare wildcard covers the core group only",
			in:      "*",
			permits: []string{"/pods", "/secrets"},
			denies:  []string{"example.com/widgets"},
		},
		{
			name:    "matching is case-insensitive on the plural",
			in:      "Pods",
			permits: []string{"/pods", "/PODS"},
		},
		{
			name:    "whitespace and empty entries are tolerated",
			in:      " pods , , configmaps ,",
			permits: []string{"/pods", "/configmaps"},
		},
		{
			name:   "the empty list permits nothing",
			in:     "",
			denies: []string{"/pods", "example.com/widgets"},
		},
		{
			name:    "an empty resource name is an error",
			in:      ".example.com",
			wantErr: "resource name is empty",
		},
		{
			name:    "a trailing dot is an error",
			in:      "widgets.",
			wantErr: "API group is empty",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			a, err := ParseAllowlist(tc.in)
			if tc.wantErr != "" {
				if err == nil || !strings.Contains(err.Error(), tc.wantErr) {
					t.Fatalf("ParseAllowlist(%q) error = %v, want containing %q", tc.in, err, tc.wantErr)
				}
				if !a.IsEmpty() {
					t.Errorf("a rejected allowlist must permit nothing, got %q", a.String())
				}
				return
			}
			if err != nil {
				t.Fatalf("ParseAllowlist(%q) = %v", tc.in, err)
			}
			for _, probe := range tc.permits {
				g, p, _ := strings.Cut(probe, "/")
				if !a.Permits(g, p) {
					t.Errorf("Permits(%q, %q) = false, want true", g, p)
				}
			}
			for _, probe := range tc.denies {
				g, p, _ := strings.Cut(probe, "/")
				if a.Permits(g, p) {
					t.Errorf("Permits(%q, %q) = true, want false", g, p)
				}
			}
		})
	}
}

func TestAllowlistZeroValuePermitsNothing(t *testing.T) {
	t.Parallel()
	var a Allowlist
	if !a.IsEmpty() {
		t.Error("zero Allowlist should be empty")
	}
	if a.Permits("", "pods") {
		t.Error("zero Allowlist must not permit anything: a gateway that failed to configure must serve nothing")
	}
}

func TestAllowlistGroups(t *testing.T) {
	t.Parallel()
	a, err := ParseAllowlist("pods,configmaps,widgets.example.com,gadgets.example.com,things.a.io")
	if err != nil {
		t.Fatal(err)
	}
	got := a.Groups()
	want := []string{"", "a.io", "example.com"}
	if len(got) != len(want) {
		t.Fatalf("Groups() = %v, want %v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("Groups() = %v, want %v", got, want)
		}
	}
}

func TestServesEverything(t *testing.T) {
	t.Parallel()
	if DefaultServe != "*.*" {
		t.Fatalf("DefaultServe = %q; this test assumes the default narrows nothing", DefaultServe)
	}
	for in, want := range map[string]bool{
		"*.*":             true,
		"pods,*.*":        true,
		"pods,configmaps": false,
		"*":               false,
		"*.example.com":   false,
		"":                false,
	} {
		a, err := ParseAllowlist(in)
		if err != nil {
			t.Fatalf("ParseAllowlist(%q) = %v", in, err)
		}
		if got := a.ServesEverything(); got != want {
			t.Errorf("ServesEverything(%q) = %v, want %v", in, got, want)
		}
	}
}

func TestAllowlistStringRoundTrips(t *testing.T) {
	t.Parallel()
	const in = "pods,configmaps,widgets.example.com"
	a, err := ParseAllowlist(in)
	if err != nil {
		t.Fatal(err)
	}
	if got := a.String(); got != in {
		t.Errorf("String() = %q, want %q", got, in)
	}
}
