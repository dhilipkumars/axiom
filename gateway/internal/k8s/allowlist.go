package k8s

import (
	"fmt"
	"sort"
	"strings"
)

// Allowlist bounds which resources a gateway deployment serves.
//
// It exists because discovery would otherwise widen the gateway from "the two
// kinds Phase 1-3 hardcoded" to "everything in the cluster" the moment the
// static registry was removed. docs/RULES.md §3 requires the served set to be
// an explicit deployment decision, with the ServiceAccount's RBAC scoped to
// match. This is the configuration side of that pair; RBAC remains the
// enforcing side, and nothing here grants access the identity does not have.
//
// The zero Allowlist permits nothing, so a gateway that fails to parse its
// configuration serves nothing rather than everything.
type Allowlist struct {
	rules []rule
}

// rule is one parsed entry. An empty group is the core API group; a plural of
// "*" covers every resource in the group.
type rule struct {
	group  string
	plural string
}

// DefaultServe is the allowlist a gateway uses when none is configured: exactly
// the kinds Phases 1-3 served, so removing the static registry does not widen
// any existing deployment.
const DefaultServe = "pods,configmaps"

// ParseAllowlist parses a comma-separated serve list.
//
// Each entry is `plural` (core API group) or `plural.group`, matching the
// kubectl spelling, e.g. "pods", "configmaps", "widgets.example.com". A plural
// of "*" covers a whole group: "*.example.com", or plain "*" for all core
// kinds. Whitespace around entries is ignored and empty entries are skipped, so
// a trailing comma is not an error.
//
// Returns an error for an entry with an empty plural or an empty group segment,
// since those are far more likely to be a typo than an intent to serve nothing.
func ParseAllowlist(s string) (Allowlist, error) {
	var a Allowlist
	for _, entry := range strings.Split(s, ",") {
		entry = strings.TrimSpace(entry)
		if entry == "" {
			continue
		}
		plural, group, hasGroup := strings.Cut(entry, ".")
		if plural == "" {
			return Allowlist{}, fmt.Errorf("serve entry %q: resource name is empty", entry)
		}
		if hasGroup && group == "" {
			return Allowlist{}, fmt.Errorf("serve entry %q: API group is empty after %q", entry, plural+".")
		}
		a.rules = append(a.rules, rule{group: group, plural: strings.ToLower(plural)})
	}
	return a, nil
}

// Permits reports whether group/plural is inside the allowlist.
//
// Matching is exact on the group and exact-or-wildcard on the plural. A group
// is never matched by a wildcard: "*.example.com" does not imply "*.other.com",
// so widening to a new group is always an explicit configuration change.
func (a Allowlist) Permits(group, plural string) bool {
	plural = strings.ToLower(plural)
	for _, r := range a.rules {
		if r.group != group {
			continue
		}
		if r.plural == "*" || r.plural == plural {
			return true
		}
	}
	return false
}

// Groups returns the distinct API groups the allowlist mentions, sorted, with
// the core group as the empty string. Discovery uses it to avoid walking groups
// this deployment could not serve anyway.
func (a Allowlist) Groups() []string {
	seen := make(map[string]struct{}, len(a.rules))
	out := make([]string, 0, len(a.rules))
	for _, r := range a.rules {
		if _, dup := seen[r.group]; dup {
			continue
		}
		seen[r.group] = struct{}{}
		out = append(out, r.group)
	}
	sort.Strings(out)
	return out
}

// IsEmpty reports whether the allowlist permits nothing.
func (a Allowlist) IsEmpty() bool { return len(a.rules) == 0 }

// String renders the allowlist back to its configuration spelling, for logs.
func (a Allowlist) String() string {
	out := make([]string, 0, len(a.rules))
	for _, r := range a.rules {
		if r.group == "" {
			out = append(out, r.plural)
			continue
		}
		out = append(out, r.plural+"."+r.group)
	}
	return strings.Join(out, ",")
}
