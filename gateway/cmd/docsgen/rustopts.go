package main

import (
	"fmt"
	"os"
	"regexp"
	"strings"
)

// FDW option reference generation.
//
// The option descriptors live in extension/src/options.rs as OptionDoc
// literals, and the FDW validator checks incoming OPTIONS against that same
// table (see option_docs there). Documenting them therefore means reading that
// table, which is why this parses Rust source rather than maintaining a second
// copy here.
//
// Linking the crate instead is not available: the extension is a pgrx cdylib
// that only builds inside a Postgres build, so a Go generator cannot call into
// it and a Rust generator would need the same toolchain CI does not run for
// documentation. The parser is strict to compensate -- an entry it cannot read
// is an error, never a skipped row.

type fdwOption struct {
	Name     string
	Required bool
	Default  string // empty means "no default"
	Summary  string
}

type fdwCatalog struct {
	Const   string // the Rust const name
	Title   string // the SQL statement it configures
	Doc     string // the doc comment on the const
	Options []fdwOption
}

var (
	reConstDoc = regexp.MustCompile(`(?m)((?:^///.*\n)+)pub const (\w+): &\[(?:crate::options::)?OptionDoc\] = &\[`)
	// (?s) so `.` spans the line continuations a multi-line summary uses.
	reEntry = regexp.MustCompile(`(?s)(?:crate::options::)?OptionDoc \{\s*` +
		`name: "([^"]*)",\s*` +
		`required: (true|false),\s*` +
		`default: (?:None|Some\("((?:[^"\\]|\\.)*)"\)),\s*` +
		`summary: "((?:[^"\\]|\\.)*)",\s*` +
		`\}`)
)

// unRust turns a Rust string literal body into plain text: line continuations
// (a backslash at end of line swallowing the following indentation) collapse
// to a single space, and escaped quotes become quotes.
func unRust(s string) string {
	s = regexp.MustCompile(`\\\s*\n\s*`).ReplaceAllString(s, " ")
	s = strings.ReplaceAll(s, `\"`, `"`)
	s = strings.ReplaceAll(s, `\\`, `\`)
	return strings.Join(strings.Fields(s), " ")
}

// parseFDWOptions extracts every `pub const ...: &[OptionDoc]` table from the
// given Rust source.
func parseFDWOptions(path string) ([]fdwCatalog, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	src := string(raw)

	titles := map[string]string{
		"SERVER_OPTION_DOCS": "CREATE SERVER",
		"TABLE_OPTION_DOCS":  "CREATE FOREIGN TABLE",
		"IMPORT_OPTION_DOCS": "IMPORT FOREIGN SCHEMA",
	}

	locs := reConstDoc.FindAllStringSubmatchIndex(src, -1)
	if len(locs) == 0 {
		return nil, fmt.Errorf("%s: no `pub const ...: &[OptionDoc]` table found; "+
			"has the descriptor table been renamed or restructured?", path)
	}
	var out []fdwCatalog
	for _, loc := range locs {
		doc := firstPara(src[loc[2]:loc[3]])
		name := src[loc[4]:loc[5]]
		title, ok := titles[name]
		if !ok {
			return nil, fmt.Errorf("%s: option table %q has no SQL statement mapped in docsgen; "+
				"add it to the titles map", path, name)
		}
		end := strings.Index(src[loc[1]:], "\n];")
		if end < 0 {
			return nil, fmt.Errorf("%s: option table %s is not terminated by `];`", path, name)
		}
		body := src[loc[1] : loc[1]+end]

		cat := fdwCatalog{Const: name, Title: title, Doc: doc}
		for _, m := range reEntry.FindAllStringSubmatch(body, -1) {
			def := unRust(m[3])
			cat.Options = append(cat.Options, fdwOption{
				Name:     m[1],
				Required: m[2] == "true",
				Default:  def,
				Summary:  unRust(m[4]),
			})
		}
		// Every `OptionDoc {` in the body must have produced a row. A partial
		// parse would silently drop an option from the reference page.
		if want := strings.Count(body, "OptionDoc {"); want != len(cat.Options) {
			return nil, fmt.Errorf("%s: %s has %d OptionDoc entries but only %d parsed; "+
				"the literal shape changed", path, name, want, len(cat.Options))
		}
		out = append(out, cat)
	}
	return out, nil
}

// firstPara turns a Rust `///` comment block into its first paragraph, which
// is the summary sentence a section heading wants. Later paragraphs explain
// the mechanism to whoever is editing the table and do not belong on the page.
func firstPara(block string) string {
	var out []string
	for _, l := range strings.Split(block, "\n") {
		l = strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(l), "///"))
		if l == "" {
			if len(out) > 0 {
				break
			}
			continue
		}
		out = append(out, l)
	}
	return strings.Join(out, " ")
}
