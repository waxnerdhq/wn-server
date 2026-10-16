package navidrome

import (
	"context"
	"errors"
	"fmt"
	"path/filepath"
	"strconv"
)

// LibraryAPI is the part of Client that SyncLibraries uses.
type LibraryAPI interface {
	ListLibraries(ctx context.Context) ([]Library, error)
	CreateLibrary(ctx context.Context, name, path string) error
}

// SyncLibraries creates a Navidrome library for each folder that has none.
// Libraries are matched by cleaned path. Libraries that are not in folders
// are left alone. Each new library is named after the last element of its
// path; a name already in use gets a numeric suffix.
//
// sortedDir is Navidrome's MusicFolder, which Navidrome stores as library 1
// on every start. It is expected to exist already and is never created
// here; a missing one is reported as an error.
//
// A failed CreateLibrary does not stop the sync. Every folder is attempted
// and the errors are returned joined. The returned slice holds the paths
// that were created.
func SyncLibraries(ctx context.Context, api LibraryAPI, sortedDir string, folders []string) ([]string, error) {
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
	var errs []error
	if sortedDir = filepath.Clean(sortedDir); !havePath[sortedDir] {
		errs = append(errs, fmt.Errorf("sorted_dir %s is not a navidrome library", sortedDir))
	}
	var created []string
	for _, p := range folders {
		p = filepath.Clean(p)
		if havePath[p] {
			continue
		}
		name := uniqueName(libraryName(p), haveName)
		if err := api.CreateLibrary(ctx, name, p); err != nil {
			errs = append(errs, fmt.Errorf("create library %q for %s: %w", name, p, err))
			continue
		}
		havePath[p] = true
		haveName[name] = true
		created = append(created, p)
	}
	return created, errors.Join(errs...)
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
