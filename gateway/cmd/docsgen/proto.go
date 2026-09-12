package main

import (
	"fmt"
	"os"
	"regexp"
	"strings"
)

// Proto reference generation.
//
// The .proto file is parsed directly rather than through protoc-gen-doc. The
// grammar used here is a small, regular subset -- service/rpc/message/field
// with leading `//` comments -- and parsing it in-tree avoids a plugin
// dependency that CI would have to install and pin just to render a page.
// Anything the parser does not recognise is reported as an error rather than
// skipped, so the page can never be quietly incomplete.

type rpcDef struct {
	Name, Req, Resp string
	ServerStream    bool
	Doc             []string
}

type fieldDef struct {
	Type, Name string
	Num        string
	Repeated   bool
	Doc        []string
}

type messageDef struct {
	Name   string
	Doc    []string
	Fields []fieldDef
}

type protoFile struct {
	Package  string
	FileDoc  []string
	Service  string
	SvcDoc   []string
	RPCs     []rpcDef
	Messages []messageDef
}

var (
	reService = regexp.MustCompile(`^service\s+(\w+)\s*\{$`)
	reRPC     = regexp.MustCompile(`^rpc\s+(\w+)\s*\(\s*(\w+)\s*\)\s*returns\s*\(\s*(stream\s+)?(\w+)\s*\)\s*;$`)
	reMessage = regexp.MustCompile(`^message\s+(\w+)\s*\{$`)
	// An empty message is written on one line by the formatter.
	reMessageEmpty = regexp.MustCompile(`^message\s+(\w+)\s*\{\s*\}$`)
	reField        = regexp.MustCompile(`^(repeated\s+)?([\w.]+)\s+(\w+)\s*=\s*(\d+)\s*;$`)
	rePackage      = regexp.MustCompile(`^package\s+([\w.]+)\s*;$`)
	reEnum         = regexp.MustCompile(`^enum\s+(\w+)\s*\{$`)
)

// parseProto reads one .proto file into the shape the reference page needs.
func parseProto(path string) (*protoFile, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	pf := &protoFile{}
	var doc []string // comment block accumulated for the next declaration
	var depth int    // brace depth: 0 = file scope
	var inSvc bool   // inside the service block
	var msg *messageDef
	var inEnum bool

	for i, line := range strings.Split(string(raw), "\n") {
		t := strings.TrimSpace(line)
		lineNo := i + 1

		switch {
		case t == "":
			// A blank line ends a comment block only at file scope, where it
			// separates the file header from the first declaration.
			if depth == 0 && pf.Package == "" && len(doc) > 0 && pf.FileDoc == nil {
				pf.FileDoc = doc
			}
			doc = nil
			continue
		case strings.HasPrefix(t, "//"):
			doc = append(doc, strings.TrimSpace(strings.TrimPrefix(t, "//")))
			continue
		case t == "}":
			depth--
			switch {
			case inEnum:
				inEnum = false
			case msg != nil:
				pf.Messages = append(pf.Messages, *msg)
				msg = nil
			case inSvc:
				inSvc = false
			}
			doc = nil
			continue
		}

		switch {
		case rePackage.MatchString(t):
			pf.Package = rePackage.FindStringSubmatch(t)[1]
			if pf.FileDoc == nil {
				pf.FileDoc = doc
			}
		case reService.MatchString(t):
			pf.Service = reService.FindStringSubmatch(t)[1]
			pf.SvcDoc = doc
			inSvc, depth = true, depth+1
		case reMessageEmpty.MatchString(t):
			pf.Messages = append(pf.Messages, messageDef{
				Name: reMessageEmpty.FindStringSubmatch(t)[1], Doc: doc,
			})
		case reMessage.MatchString(t):
			msg = &messageDef{Name: reMessage.FindStringSubmatch(t)[1], Doc: doc}
			depth++
		case reEnum.MatchString(t):
			// Enums carry no documented surface today; consume the block so a
			// nested one does not confuse the brace depth.
			inEnum, depth = true, depth+1
		case inSvc && reRPC.MatchString(t):
			m := reRPC.FindStringSubmatch(t)
			pf.RPCs = append(pf.RPCs, rpcDef{
				Name: m[1], Req: m[2], ServerStream: m[3] != "", Resp: m[4], Doc: doc,
			})
		case msg != nil && reField.MatchString(t):
			m := reField.FindStringSubmatch(t)
			msg.Fields = append(msg.Fields, fieldDef{
				Repeated: m[1] != "", Type: m[2], Name: m[3], Num: m[4], Doc: doc,
			})
		case strings.HasPrefix(t, "syntax"), strings.HasPrefix(t, "option"),
			strings.HasPrefix(t, "import"), inEnum:
			// Not part of the generated surface.
		default:
			// Fail rather than skip: a silently dropped declaration is exactly
			// the drift this generator exists to prevent.
			return nil, fmt.Errorf("%s:%d: unrecognised proto syntax: %q", path, lineNo, t)
		}
		doc = nil
	}
	if pf.Service == "" {
		return nil, fmt.Errorf("%s: no service found", path)
	}
	return pf, nil
}
