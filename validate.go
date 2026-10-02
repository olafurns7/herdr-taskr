package main

import (
	"flag"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

var (
	namePattern    = regexp.MustCompile(`^[a-z][a-z0-9_-]{0,31}$`)
	herdrIDPattern = regexp.MustCompile(`^[0-9A-Za-z:_-]+$`)
)

func validateName(value, field string) error {
	if !namePattern.MatchString(value) {
		return usageErr("%s must match [a-z][a-z0-9_-]{0,31}", field)
	}
	return nil
}

func validateHerdrID(value, field string) error {
	if strings.TrimSpace(value) == "" || value == "null" || !herdrIDPattern.MatchString(value) {
		return usageErr("%s must be a non-empty, non-whitespace ID matching [0-9A-Za-z:_-]+ and not `null`", field)
	}
	return nil
}

func flagWasSet(fs *flag.FlagSet, name string) bool {
	set := false
	fs.Visit(func(f *flag.Flag) {
		if f.Name == name {
			set = true
		}
	})
	return set
}

func validateLocationFlags(fs *flag.FlagSet, workspace, tab, pane string) error {
	values := []struct{ name, value string }{{"workspace", workspace}, {"tab", tab}, {"pane", pane}}
	for _, v := range values {
		if flagWasSet(fs, v.name) {
			if err := validateHerdrID(v.value, "--"+v.name); err != nil {
				return err
			}
		}
	}
	if !flagWasSet(fs, "workspace") {
		return nil
	}
	for _, v := range values[1:] {
		if !flagWasSet(fs, v.name) {
			continue
		}
		prefix, _, hasColon := strings.Cut(v.value, ":")
		if hasColon && prefix != workspace {
			return usageErr("--%s must use --workspace prefix %q before the first colon (ID shape: [0-9A-Za-z:_-]+)", v.name, workspace)
		}
	}
	return nil
}

func absolutePath(base, path, field string) (string, error) {
	if path == "" {
		return "", nil
	}
	if !filepath.IsAbs(path) {
		path = filepath.Join(base, path)
	}
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", usageErr("%s must resolve to an absolute path: %v", field, err)
	}
	return absolute, nil
}

func requireDirectory(path string) error {
	info, err := os.Stat(path)
	if err != nil || !info.IsDir() {
		return usageErr("--cwd must be an existing directory, got %q", path)
	}
	return nil
}

func requireFile(path string) error {
	info, err := os.Stat(path)
	if err != nil || !info.Mode().IsRegular() {
		return usageErr("--brief must be an existing file, got %q", path)
	}
	return nil
}
