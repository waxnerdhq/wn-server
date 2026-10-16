package navidrome

import (
	"context"
	"fmt"
	"path/filepath"
	"strconv"
)

// LibraryAPI is the part of Client that SyncLibraries uses.
type LibraryAPI interface {
	ListLibraries(ctx context.Context) ([]Library, error)
	CreateLibrary(ctx context.Context, name, path string) error
}

// SyncLibraries creates a Navidrome library for each path that has none.
// Libraries are matched by cleaned path. Libraries that are not in paths are
// left alone. Each new library is named after the last element of its path;
// a name already in use gets a numeric suffix. It returns the paths it
// created.
func SyncLibraries(ctx context.Context, api LibraryAPI, paths []string) ([]string, error) {
	existing, err := api.ListLibraries(ctx)
	if err != nil {
		return nil, fmt.Errorf("list libraries: %w", err)
	}
	havePath := make(map[string]bool)
	haveName := make(map[string]bool)
	for _, l := range existing {
		havePath[filepath.Clean(l.Path)] = true
		haveName[l.Name] = true
	}
	var created []string
	for _, p := range paths {
		p = filepath.Clean(p)
		if havePath[p] {
			continue
		}
		name := uniqueName(libraryName(p), haveName)
		if err := api.CreateLibrary(ctx, name, p); err != nil {
			return created, fmt.Errorf("create library %q for %s: %w", name, p, err)
		}
		havePath[p] = true
		haveName[name] = true
		created = append(created, p)
	}
	return created, nil
}

func libraryName(path string) string {
	name := filepath.Base(path)
	if name == "." || name == string(filepath.Separator) {
		return "Music"
	}
	return name
}

func uniqueName(name string, used map[string]bool) string {
	if !used[name] {
		return name
	}
	for i := 2; ; i++ {
		n := name + " " + strconv.Itoa(i)
		if !used[n] {
			return n
		}
	}
}
